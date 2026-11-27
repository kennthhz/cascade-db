//! Cross-core gates. The page store is one `Send + Sync` object shared by
//! every core (§8), and two of its critical sections span `await` points
//! (a flush writes and syncs; a lazy page read waits on the device). A
//! `std::Mutex` cannot be held across an `await` — so these are small
//! async mutexes: lock state under a `std::Mutex` that is never held across
//! an await, and park the caller through its waker.
//!
//! **The gate is fair.** A release does not re-open the gate for whoever
//! gets there first: it *hands* the gate to the longest-waiting waiter, and
//! the gate stays held until that waiter's next poll claims it. A newcomer
//! may take a free gate only when nobody is queued.
//!
//! That is a liveness requirement, not politeness. The first version of
//! this type released with "wake every waiter and let them re-contend",
//! which had two faults. A release drained the queue while a woken waiter
//! still believed itself queued, so a waiter that re-polled after another
//! task had taken the gate was never woken again — a lost wakeup, found by
//! reading and shown by a test before it was fixed. And even with the
//! queue right, a parked waiter is at a structural disadvantage against a
//! thread hot-looping on the gate: it runs microseconds after being woken,
//! and the looping thread re-takes the gate in well under one, so it can
//! be starved for as long as the looping goes on. That second fault is an
//! argument from scheduling latency; it was not observed in the store.
//! With hand-off, a hot-looping acquirer finds the gate spoken for and
//! queues behind those already waiting.
//!
//! Two more rules:
//!
//! * **Cancelling is safe at every point.** A waiter that is dropped while
//!   queued leaves the queue. A waiter dropped *after* being handed the
//!   gate passes it on, so the gate is never left held by nobody.
//! * **No `Send`.** The guard borrows the gate; it lives and dies on the
//!   core that took it, like every storage-contract future (S3).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use cascade_types::LocalBoxFuture;

/// One queued acquisition.
#[derive(Debug)]
struct Waiter {
    /// How to wake the waiting task. Replaced on every poll: a task may
    /// move between executors.
    waker: Mutex<Option<Waker>>,
    /// Set by the release that hands the gate to this waiter. From then on
    /// the gate is *this waiter's* — nobody else may take it — until its
    /// next poll claims it, or its drop passes it on.
    handed: AtomicBool,
}

#[derive(Debug, Default)]
struct Inner {
    /// Held: by a guard, or by the waiter it has been handed to. Whenever
    /// this is false the queue is empty.
    locked: bool,
    /// Longest-waiting first.
    waiters: VecDeque<Arc<Waiter>>,
}

/// A fair async mutex. Cheap when uncontended: one `std::Mutex` round trip.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    inner: Mutex<Inner>,
}

fn lock_inner(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    // Poison-tolerant: a panic inside a critical section leaves this
    // two-field state structurally valid, and wedging every later caller
    // would turn one panic into a hang.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn waker_of(waiter: &Waiter) -> std::sync::MutexGuard<'_, Option<Waker>> {
    waiter
        .waker
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Passes a held gate on: to the longest-waiting waiter, which the gate
/// then belongs to (`locked` stays true), or to nobody (`locked` clears).
fn hand_off(g: &mut Inner) {
    // No cancelled waiter can be found here: a dropped one leaves the
    // queue at once, under this same lock. So the first in line is live.
    if let Some(next) = g.waiters.pop_front() {
        next.handed.store(true, Ordering::Release);
        // Woken under the lock: the waiter's poll takes the same lock, so
        // it cannot observe the half-finished hand-off.
        if let Some(waker) = waker_of(&next).clone() {
            waker.wake();
        }
    } else {
        g.locked = false;
    }
}

/// The lock guard. Dropping it hands the gate on.
pub(crate) struct Guard<'a>(&'a Gate);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        hand_off(&mut lock_inner(&self.0.inner));
    }
}

/// The pending acquisition. Dropping it cancels the wait.
pub(crate) struct Lock<'a> {
    gate: &'a Gate,
    /// `None` until the first poll has to queue.
    waiter: Option<Arc<Waiter>>,
}

impl<'a> Future for Lock<'a> {
    type Output = Guard<'a>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Guard<'a>> {
        let this = &mut *self;
        let mut g = lock_inner(&this.gate.inner);

        // A release has handed the gate to this future: it is ours. The
        // claim is recorded before the guard leaves, because this future
        // is dropped once it resolves — and a drop must not hand on a gate
        // that has just been claimed.
        if this
            .waiter
            .as_ref()
            .is_some_and(|w| w.handed.load(Ordering::Acquire))
        {
            this.waiter = None;
            return Poll::Ready(Guard(this.gate));
        }
        if let Some(waiter) = &this.waiter {
            // Still queued. Re-register: the task may have moved between
            // executors, and the next hand-off must reach it.
            *waker_of(waiter) = Some(cx.waker().clone());
            return Poll::Pending;
        }
        if !g.locked {
            // Free, and (by the invariant above) nobody waiting: ours.
            g.locked = true;
            return Poll::Ready(Guard(this.gate));
        }
        // Spoken for — held, or handed to a waiter that has not claimed it
        // yet. Queue behind those already waiting.
        let waiter = Arc::new(Waiter {
            waker: Mutex::new(Some(cx.waker().clone())),
            handed: AtomicBool::new(false),
        });
        g.waiters.push_back(Arc::clone(&waiter));
        this.waiter = Some(waiter);
        Poll::Pending
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        let Some(waiter) = &self.waiter else {
            return;
        };
        let mut g = lock_inner(&self.gate.inner);
        g.waiters.retain(|w| !Arc::ptr_eq(w, waiter));
        if waiter.handed.load(Ordering::Acquire) {
            // Handed to a future that will never claim it: pass it on.
            hand_off(&mut g);
        }
    }
}

impl Gate {
    /// Acquires the gate, parking the caller while it is held or spoken
    /// for. Waiters are served in the order they arrived.
    pub(crate) fn lock(&self) -> LocalBoxFuture<'_, Guard<'_>> {
        Box::pin(Lock {
            gate: self,
            waiter: None,
        })
    }

    /// Acquires the gate only if it is free and nobody is waiting. For
    /// tests: it is how a test observes "held" and "released" without
    /// waiting.
    #[cfg(test)]
    pub(crate) fn try_lock(&self) -> Option<Guard<'_>> {
        let mut g = lock_inner(&self.inner);
        debug_assert!(g.locked || g.waiters.is_empty(), "a free gate has no waiters");
        if g.locked {
            None
        } else {
            g.locked = true;
            Some(Guard(self))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    /// A counting waker: a test can tell "parked, not yet woken" from
    /// "parked and handed the gate", which a noop waker cannot show. A
    /// waker must be `Send + Sync`, so the counter is an atomic.
    struct CountingWaker(Arc<AtomicUsize>);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Waker, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Waker::from(Arc::new(CountingWaker(Arc::clone(&wakes))));
        (w, wakes)
    }

    fn wake_count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    /// Polls an acquisition once by hand. What `Gate::lock` returns is
    /// `Unpin` (it is boxed, and the `Lock` inside holds only a reference
    /// and an `Arc`), so a stack pin is enough.
    fn poll_once<'a, F>(f: &mut F, w: &Waker) -> Poll<Guard<'a>>
    where
        F: Future<Output = Guard<'a>> + Unpin,
    {
        let mut cx = Context::from_waker(w);
        Pin::new(f).poll(&mut cx)
    }

    fn queued(gate: &Gate) -> usize {
        lock_inner(&gate.inner).waiters.len()
    }

    /// The uncontended path is synchronous: first poll takes the gate,
    /// nothing is queued, and the guard's drop releases it.
    #[test]
    fn uncontended_lock_resolves_on_first_poll() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.try_lock().expect("free: try_lock takes it");
            assert!(gate.try_lock().is_none(), "held: try_lock refuses");
            drop(held);

            let (w, wakes) = counting();
            let mut acquire = gate.lock();
            let ready = poll_once(&mut acquire, &w);
            assert!(matches!(ready, Poll::Ready(_)), "uncontended: ready at once");
            assert_eq!(wake_count(&wakes), 0, "nothing to wake");
            drop(ready);
            assert!(gate.try_lock().is_some(), "the guard's drop released it");
        });
    }

    /// A waiter parked on a held gate is handed it by the release — and
    /// the gate stays spoken for until the waiter claims it.
    #[test]
    fn a_waiter_is_held_for_until_it_claims() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.lock().await;

            let (w, wakes) = counting();
            let mut waiter = gate.lock();
            assert!(poll_once(&mut waiter, &w).is_pending(), "held: parks");
            assert_eq!(wake_count(&wakes), 0, "still held: not woken");
            assert_eq!(queued(&gate), 1, "queued");

            drop(held);
            assert_eq!(wake_count(&wakes), 1, "the release handed it over");
            assert_eq!(queued(&gate), 0, "no longer queued: it is next");
            assert!(
                gate.try_lock().is_none(),
                "spoken for until the waiter runs: nobody may take it"
            );

            let _mine = waiter.await;
            assert!(gate.try_lock().is_none(), "the waiter holds it now");
        });
    }

    /// A waiter that gives up (its future is dropped) leaves the queue.
    #[test]
    fn a_dropped_waiter_leaves_the_queue() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.lock().await;
            {
                let (w, _) = counting();
                let mut abandoned = gate.lock();
                assert!(poll_once(&mut abandoned, &w).is_pending());
                assert_eq!(queued(&gate), 1);
            } // the future is dropped here
            assert_eq!(queued(&gate), 0, "unregistered on drop");

            drop(held);
            let _next = gate.lock().await;
        });
    }

    /// A waiter cancelled *ahead of* another waiter cannot strand it.
    #[test]
    fn a_cancelled_waiter_does_not_strand_the_next() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.lock().await;

            let (w_live, wakes_live) = counting();
            let (w_dead, wakes_dead) = counting();
            let mut live = gate.lock();
            let mut dead = gate.lock();
            assert!(poll_once(&mut live, &w_live).is_pending());
            assert!(poll_once(&mut dead, &w_dead).is_pending());
            assert_eq!(queued(&gate), 2, "two distinct waiters queued");
            drop(dead); // cancelled while parked
            assert_eq!(queued(&gate), 1);

            drop(held);
            assert_eq!(wake_count(&wakes_live), 1, "the survivor was handed the gate");
            assert_eq!(wake_count(&wakes_dead), 0, "the cancelled one was not");
            let _mine = live.await;
            assert!(gate.try_lock().is_none());
        });
    }

    /// A waiter cancelled *after* being handed the gate passes it on: the
    /// gate is never left held by nobody.
    #[test]
    fn a_waiter_cancelled_after_the_hand_off_passes_the_gate_on() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.lock().await;
            let (w_first, wakes_first) = counting();
            let (w_next, wakes_next) = counting();
            let mut first = gate.lock();
            let mut next = gate.lock();
            assert!(poll_once(&mut first, &w_first).is_pending());
            assert!(poll_once(&mut next, &w_next).is_pending());

            drop(held);
            assert_eq!(wake_count(&wakes_first), 1, "handed to the first");
            drop(first); // it never runs to claim it
            assert_eq!(
                wake_count(&wakes_next),
                1,
                "…so the gate is passed on to the next in line at once"
            );
            assert!(gate.try_lock().is_none(), "still spoken for");
            let _mine = next.await;
            assert!(gate.try_lock().is_none(), "the next waiter holds it");
        });
    }

    /// The property the first version of this type lacked. A hot-looping
    /// acquirer cannot overtake a waiter the gate has already been handed
    /// to, however many times it tries before that waiter gets to run — so
    /// a parked waiter is never starved.
    #[test]
    fn a_latecomer_cannot_overtake_a_waiter_the_gate_was_handed_to() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.lock().await;
            let (w_waiter, wakes_waiter) = counting();
            let mut waiter = gate.lock();
            assert!(poll_once(&mut waiter, &w_waiter).is_pending());

            drop(held);
            assert_eq!(wake_count(&wakes_waiter), 1, "handed to the waiter");

            // The waiter has not run yet. A looper tries a thousand times…
            let (w_looper, wakes_looper) = counting();
            let mut looper = gate.lock();
            for _ in 0..1000 {
                assert!(
                    poll_once(&mut looper, &w_looper).is_pending(),
                    "the gate is spoken for: a latecomer waits"
                );
            }
            assert_eq!(queued(&gate), 1, "the looper is queued behind");
            assert_eq!(wake_count(&wakes_looper), 0, "and has been handed nothing");

            // The waiter finally runs, and the gate is its.
            let mine = waiter.await;
            drop(mine);
            assert_eq!(wake_count(&wakes_looper), 1, "only now is the looper's turn");
            let _seated = looper.await;
            assert!(gate.try_lock().is_none(), "the looper holds it");
        });
    }

    /// Waiters are served in the order they arrived, one per release.
    #[test]
    fn waiters_are_served_in_order() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.lock().await;
            let (wa, wakes_a) = counting();
            let (wb, wakes_b) = counting();
            let (wc, wakes_c) = counting();
            let mut a = gate.lock();
            let mut b = gate.lock();
            let mut c = gate.lock();
            assert!(poll_once(&mut a, &wa).is_pending());
            assert!(poll_once(&mut b, &wb).is_pending());
            assert!(poll_once(&mut c, &wc).is_pending());

            drop(held);
            assert_eq!(
                (wake_count(&wakes_a), wake_count(&wakes_b), wake_count(&wakes_c)),
                (1, 0, 0),
                "only the first in line"
            );
            let first = a.await;
            drop(first);
            assert_eq!(
                (wake_count(&wakes_a), wake_count(&wakes_b), wake_count(&wakes_c)),
                (1, 1, 0),
                "then the second"
            );
            let second = b.await;
            drop(second);
            assert_eq!(wake_count(&wakes_c), 1, "then the third");
            let _third = c.await;
            assert!(gate.try_lock().is_none());
        });
    }

    /// A task polled again before it is served keeps one queue entry, and
    /// a hand-off after the re-poll still reaches it.
    #[test]
    fn repeated_polls_keep_one_queue_entry() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let held = gate.lock().await;
            let (w, _) = counting();
            let mut waiter = gate.lock();
            assert!(poll_once(&mut waiter, &w).is_pending());
            assert!(poll_once(&mut waiter, &w).is_pending());
            assert_eq!(queued(&gate), 1, "one entry per waiter");
            drop(held);
            let _mine = waiter.await;
        });
    }

    /// The proof the whole module exists for: a gate may be held across an
    /// await on one thread, which a `std::sync::MutexGuard` cannot be.
    #[test]
    fn a_guard_can_be_held_across_an_await() {
        futures_lite::future::block_on(async {
            let gate = Gate::default();
            let _across = gate.lock().await;
            futures_lite::future::yield_now().await;
            assert!(gate.try_lock().is_none(), "still held after the await");
        });
    }
}
