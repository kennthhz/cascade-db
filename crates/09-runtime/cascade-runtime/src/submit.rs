//! The `submit_to` primitive (T5.1, FR-35, §6.1): cross-reactor calls
//! where the *closure* is `Send` but the future's internal state may be
//! `!Send`.
//!
//! The signature is the spec's §6.1 shape, verbatim where it counts
//! (R14): `f: FnOnce() -> Fut + Send + 'static` with
//! `Fut: Future<Output = T> + 'static` — **no `Send` bound on the
//! future** — and `T: Send + 'static`. The closure's captured state
//! crosses reactors, so it must be `Send`; the future it *constructs*
//! never does, because the closure is run on the target ring and the
//! future is spawned right there (`spawn_local`) — `Rc`s and other
//! ring-local state are free inside it. That asymmetry is the whole
//! design: the queued job carries only the boxed closure (a `Send`
//! type, as the channel requires), so no `unsafe` `Send` claim is ever
//! made for the future itself.
//!
//! Only the result crosses back — over a oneshot whose
//! dropped-without-send case (the job's task died before sending, the
//! queue was torn down) resolves the caller's future with an error
//! rather than hanging it.
//!
//! Backpressure follows the codebase's counted-refusal discipline (the
//! checkpointer's control channel, the reaper's fanout): the job queue
//! is bounded, and a submit onto a full one is refused and counted,
//! never blocking the caller's ring. §6.1 is explicit that this is a
//! coordination primitive — "use sparingly"; the OLTP query hot path
//! must never call it (FR-36, I12) — so refusal is the honest shape for
//! a saturated target, and [`SubmitStats::submitted`] is the debug
//! counter T5.4's zero-`submit_to` probe reads.
//!
//! The channel comes up in two halves, like every family in this
//! runtime: [`channel`] returns the unconnected producer ([`SubmitInit`])
//! and the raw receiver; the target ring runs [`submit_loop`] over its
//! connected receiver, and each caller ring connects its own
//! [`SubmitInit`] into a [`SubmitHandle`]. A `connect` blocks until the
//! peer endpoint connects on its own executor (T4.5's rule) — with a
//! single family there is no ordering lattice, but both halves must
//! connect for traffic to flow.
//!
//! Two properties consumers must know before building on this:
//!
//! * **Panic blast radius is the whole target ring.** A panicking job
//!   does NOT hang its caller — glommio drops the dying task's future
//!   during unwind, the oneshot sender cancels, and the caller
//!   resolves with [`SubmitError::Lost`] — but the panic then
//!   propagates through glommio's executor loop (no `catch_unwind`
//!   there), killing the ring itself: `submit_loop` and every sibling
//!   detached job die, and every in-flight caller gets `Lost`. For
//!   T5.4 that means a panicking housekeeper job is a housekeeper-ring
//!   outage; whatever recovery that needs belongs to T5.4, not here.
//! * **The channel is 1:1 and ring-bound.** glommio's
//!   `ConnectedSender` is not `Send` (it holds `Rc` state), so a
//!   [`SubmitHandle`] lives on the ring that connected it: clones of
//!   one handle submit concurrently but only from that ring; fan-OUT
//!   to N targets is N handles (one `channel` per target); fan-IN
//!   from N worker rings onto one target is N channels + N sibling
//!   `submit_loop`s on the target ring (the T5.4 housekeeper shape).

use std::{
    any::Any,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    task::{Context, Poll, Waker},
};

use glommio::channels::shared_channel::{
    new_bounded, ConnectedReceiver, ConnectedSender, SharedReceiver, SharedSender,
};

/// A constructed-on-target future, boxed. Deliberately NOT `Send` —
/// this type only ever exists on the ring that built it and is spawned
/// right there.
type LocalJobFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// One unit of cross-reactor work. The boxed closure is `Send` (its
/// captured state crosses); the future it RETURNS is constructed on the
/// target ring and may hold `!Send` state — it never leaves there.
pub struct SubmitJob {
    run: Box<dyn FnOnce() -> LocalJobFuture + Send + 'static>,
}

/// The FR-36 debug counters: coordination traffic must stay off the
/// query hot path, and T5.4's zero-`submit_to` probe reads `submitted`
/// over an OLTP workload to prove it.
#[derive(Debug, Default)]
pub struct SubmitStats {
    /// Calls to `submit_to` (eager — before queue admission).
    pub submitted: AtomicU64,
    /// Jobs started on a target ring.
    pub executed: AtomicU64,
    /// Submits refused because the target's queue was full.
    pub refused: AtomicU64,
}

impl SubmitStats {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The caller-half before connection: the shared producer endpoint plus
/// the stats the handle will report. Connect it on the ring that will
/// submit (see the module doc).
pub struct SubmitInit {
    sender: SharedSender<SubmitJob>,
    stats: Arc<SubmitStats>,
}

/// A connected submit endpoint for one target reactor: `submit_to`
/// queues work onto that ring and awaits the result. `Clone` — every
/// clone submits to the same target through the same bounded queue.
///
/// (glommio's `ConnectedSender` is not `Clone`; the `Arc` newtype makes
/// the handle so while every clone shares the one producer endpoint.)
#[derive(Clone)]
pub struct SubmitHandle {
    // `Rc`, not `Arc`: glommio's `ConnectedSender` is neither Send nor
    // Sync, and the handle is ring-local BY DESIGN (see the module doc)
    // — the type says so.
    sender: Rc<ConnectedSender<SubmitJob>>,
    stats: Arc<SubmitStats>,
}

/// Why a submit's result never arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The result channel closed without a value: the job's task died
    /// before its sender fired (note: a panicking job takes the whole
    /// target ring down — see the module doc), or the queue was torn
    /// down. Delivered once BOTH channel halves drop — including the
    /// caller's own handle: a receiver disconnecting mid-submit while
    /// the handle stays alive leaves the caller pending until it drops
    /// the handle. The caller always resolves; it never hangs forever.
    #[error("submit_to target lost the result (task died or queue torn down)")]
    Lost,
    /// The target's queue was full — the counted refusal (see the
    /// module doc). Retry or treat as overload.
    #[error("submit_to queue full (refused)")]
    Saturated,
}

/// Creates a bounded submit channel: the handle half and the raw
/// receiver for the target ring's [`submit_loop`]. `capacity` bounds
/// the number of queued jobs; beyond it submits are refused and counted.
///
/// **Connect the halves on DIFFERENT executors, concurrently** — a
/// `connect` blocks until its peer connects on its own ring, so
/// sequencing both connects on one ring deadlocks in the known 100%-CPU
/// spin (the T4.5 lesson; `connect` the receiver inside the spawned
/// target executor and the producer inside the caller's). Dropping the
/// producer without connecting is safe: the peer's connect resolves,
/// `recv` returns `None`, [`submit_loop`] exits.
pub fn channel(capacity: usize) -> (SubmitInit, SharedReceiver<SubmitJob>) {
    let (tx, rx) = new_bounded(capacity);
    (
        SubmitInit {
            sender: tx,
            stats: Arc::new(SubmitStats::new()),
        },
        rx,
    )
}

impl SubmitInit {
    /// The stats this init's future handle will report — take a handle
    /// before `connect` consumes the init (the target's `submit_loop`
    /// must receive the SAME Arc for the executed counter to line up).
    pub fn stats(&self) -> Arc<SubmitStats> {
        Arc::clone(&self.stats)
    }

    /// Connects this producer on the CURRENT executor (blocking until
    /// the target ring has connected its receiver — T4.5's rule).
    pub async fn connect(self) -> SubmitHandle {
        SubmitHandle {
            sender: Rc::new(self.sender.connect().await),
            stats: self.stats,
        }
    }
}

impl SubmitHandle {
    /// The stats backing this handle (the executed counter is shared
    /// with the target's `submit_loop`).
    pub fn stats(&self) -> Arc<SubmitStats> {
        Arc::clone(&self.stats)
    }

    /// FR-35, §6.1: run `f` on the target reactor. The closure (and its
    /// captured state) crosses; the future it returns is constructed ON
    /// the target and may hold `!Send` state; the result comes back to
    /// the caller. Queue admission is eager and bounded — a full queue
    /// resolves the returned future with [`SubmitError::Saturated`]
    /// without ever running `f`.
    pub fn submit_to<T, F, Fut>(&self, f: F) -> Submit<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        self.stats.submitted.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Box<dyn Any + Send>>();
        let job = SubmitJob {
            run: Box::new(move || {
                // Constructed HERE, on the target ring: internal state
                // may be !Send — it never crosses anything.
                let fut = f();
                Box::pin(async move {
                    let value = fut.await;
                    let _ = tx.send(Box::new(value));
                }) as LocalJobFuture
            }),
        };
        match self.sender.try_send(job) {
            Ok(()) => Submit {
                inner: SubmitInner::Queued(rx),
                out: PhantomData,
            },
            // Full: the refusal IS the result. The job (and its
            // oneshot sender) drops; the rx in this arm drops unused.
            Err(glommio::GlommioError::WouldBlock(
                glommio::ResourceType::Channel(_),
            )) => {
                self.stats.refused.fetch_add(1, Ordering::Relaxed);
                Submit {
                    inner: SubmitInner::Saturated,
                    out: PhantomData,
                }
            }
            // Closed (target gone): resolve as Lost via the rx — the
            // job's sender died with it, canceling the oneshot.
            Err(_) => Submit {
                inner: SubmitInner::Queued(rx),
                out: PhantomData,
            },
        }
    }
}

enum SubmitInner {
    Queued(oneshot::Receiver<Box<dyn Any + Send>>),
    Saturated,
}

/// The caller-side future of a `submit_to`.
pub struct Submit<T> {
    inner: SubmitInner,
    out: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> Future for Submit<T> {
    type Output = Result<T, SubmitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().inner;
        match inner {
            SubmitInner::Saturated => Poll::Ready(Err(SubmitError::Saturated)),
            SubmitInner::Queued(rx) => match Pin::new(rx).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(boxed)) => Poll::Ready(
                    // Unreachable-by-construction: sender and receiver
                    // are built around the same T inside `submit_to`.
                    boxed
                        .downcast::<T>()
                        .map(|b| Ok(*b))
                        .unwrap_or_else(|_| panic!("submit_to result type mismatched")),
                ),
                Poll::Ready(Err(oneshot::Canceled)) => Poll::Ready(Err(SubmitError::Lost)),
            },
        }
    }
}

/// The target-side loop: drains the queue forever, constructs each
/// job's future HERE — on the target ring — and spawns it as a sibling
/// task (a long job never head-of-line blocks the next one). Ends when
/// every producer endpoint drops.
pub async fn submit_loop(recv: ConnectedReceiver<SubmitJob>, stats: Arc<SubmitStats>) {
    while let Some(job) = recv.recv().await {
        stats.executed.fetch_add(1, Ordering::Relaxed);
        let fut = (job.run)();
        glommio::spawn_local(fut).detach();
    }
}

/// A minimal cross-ring oneshot. The contract that matters for
/// `submit_to`: a sender dropped without sending (a job's task died,
/// teardown) CANCELS the channel — the receiver resolves with an error
/// instead of hanging. Not exposed at the crate root; `submit_to` is
/// the consumer. (`std::sync::Mutex` is fine here: the critical
/// sections are a state swap and a waker store, never held across an
/// await; both sides take state-then-waker, so no ordering deadlock.)
pub(crate) mod oneshot {
    use super::*;

    /// The sender side vanished without a value.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Canceled;

    enum State<T> {
        Empty,
        Filled(T),
        Canceled,
        Taken,
    }

    struct Inner<T> {
        state: Mutex<State<T>>,
        waker: Mutex<Option<Waker>>,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wake<T>(inner: &Inner<T>) {
        if let Some(w) = lock(&inner.waker).take() {
            w.wake();
        }
    }

    /// `Option`-disarmed: `send` takes the Arc so Drop's cancel logic
    /// never fires after a successful send, and the Inner block (two
    /// Mutexes + the value) frees normally at the last Arc — no leak
    /// per submit, and a never-polled Receiver drops a `Filled` value
    /// with the block instead of pinning it.
    pub struct Sender<T>(Option<Arc<Inner<T>>>);
    pub struct Receiver<T>(Arc<Inner<T>>);

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            // No Arc held (send succeeded): nothing to cancel.
            let Some(inner) = self.0.take() else {
                return;
            };
            let mut s = lock(&inner.state);
            if matches!(*s, State::Empty) {
                *s = State::Canceled;
            }
            drop(s);
            wake(&inner);
        }
    }

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let inner = Arc::new(Inner {
            state: Mutex::new(State::Empty),
            waker: Mutex::new(None),
        });
        (Sender(Some(Arc::clone(&inner))), Receiver(inner))
    }

    impl<T> Sender<T> {
        /// Delivers the value and wakes the receiver. `Err` if the
        /// receiver side already resolved (canceled or taken).
        pub fn send(mut self, value: T) -> Result<(), T> {
            // Disarm first: after this, Drop is a no-op and the Inner
            // Arc frees normally at return — no `forget` leak.
            let inner = self.0.take().expect("oneshot sender disarmed twice");
            {
                let mut s = lock(&inner.state);
                if matches!(*s, State::Canceled | State::Taken) {
                    return Err(value);
                }
                *s = State::Filled(value);
            }
            wake(&inner);
            Ok(())
        }
    }

    impl<T> Future for Receiver<T> {
        type Output = Result<T, Canceled>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let inner = &self.0;
            let mut s = lock(&inner.state);
            if matches!(*s, State::Empty) {
                // Store the waker while holding the state lock: a
                // concurrent send blocks on the state lock until this
                // poll releases it, so it cannot slip a Filled between
                // the waker store and the final read below (no lost
                // wakeup, no re-check needed).
                *lock(&inner.waker) = Some(cx.waker().clone());
            }
            match std::mem::replace(&mut *s, State::Taken) {
                State::Filled(v) => Poll::Ready(Ok(v)),
                State::Canceled => Poll::Ready(Err(Canceled)),
                // Empty stays Empty — `Taken` must never persist past a
                // pending poll, or a later send() would see it as
                // already-resolved and silently drop the value (the
                // caller would hang).
                other => {
                    *s = other;
                    Poll::Pending
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::task::Wake;

        fn noop_waker() -> Waker {
            struct NoopWake;
            impl Wake for NoopWake {
                fn wake(self: Arc<Self>) {}
            }
            Waker::from(Arc::new(NoopWake))
        }

        #[test]
        fn send_then_receive_delivers() {
            let (tx, mut rx) = channel::<u64>();
            tx.send(41).expect("fresh channel accepts");
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Ready(Ok(41)));
        }

        #[test]
        fn dropped_sender_cancels_instead_of_hanging() {
            let (tx, mut rx) = channel::<u64>();
            drop(tx);
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert_eq!(
                Pin::new(&mut rx).poll(&mut cx),
                Poll::Ready(Err(Canceled))
            );
        }

        #[test]
        fn pending_then_send_wakes_the_stored_waker() {
            use std::sync::atomic::{AtomicUsize, Ordering as AO};

            let flag = Arc::new(AtomicUsize::new(0));
            struct FlagWaker(Arc<AtomicUsize>);
            impl Wake for FlagWaker {
                fn wake(self: Arc<Self>) {
                    self.0.fetch_add(1, AO::SeqCst);
                }
            }

            let (tx, mut rx) = channel::<u64>();
            let waker = Waker::from(Arc::new(FlagWaker(Arc::clone(&flag))));
            let mut cx = Context::from_waker(&waker);
            assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Pending);
            assert_eq!(flag.load(AO::SeqCst), 0, "no spurious wake");
            tx.send(7).expect("send after pending");
            assert_eq!(flag.load(AO::SeqCst), 1, "the stored waker fired");
            assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Ready(Ok(7)));
        }
    }
}
