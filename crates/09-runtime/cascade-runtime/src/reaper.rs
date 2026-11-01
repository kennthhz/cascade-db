//! The idle-connection reaper (T4.5, FR-34, §9): a tick job on the serving
//! housekeeper times out *idle* connections and signals each worker
//! reactor to drop its own.
//!
//! Division of labor (§9), same shape as the checkpointer/recycler: the
//! housekeeper-side [`IdleReaper`] is a [`TickJob`] that — every tick —
//! `try_send`s a [`ReapTick`] down each worker's dedicated channel. The
//! worker-side [`reap_loop`] (a sibling task on that ring, like
//! `control_loop`/`recycle_loop`) walks the reactor-local
//! [`ReapRegistry`] and condemns overdue entries. Idle connections live
//! on *every* worker, not just designated ones, so this family fans out
//! to all of them — and like every signal family it owns its own channel
//! (SPSC: the fourth after control/recycle/drain), so targeting = scope:
//! a reap signal is structurally unable to reach a reactor that doesn't
//! own sessions.
//!
//! The **idle definition is load-bearing** (the plan's code-review
//! question): a connection is idle when it is *parked in its message
//! read* AND its last completed statement is older than the timeout.
//! Both halves matter:
//!
//! - *last statement completion, not connection age* — the session
//!   touches its registry entry the moment each statement's
//!   ReadyForQuery goes out, so a long-lived connection that just did
//!   work is not overdue, and a connection in the middle of a
//!   long-running statement is not overdue either (its completion
//!   timestamp is old, but it is not parked in a read).
//! - *parked in the read* — the reaper may only condemn a connection
//!   that is actually waiting for the client. A session mid-statement
//!   is never condemned; the reap decision is a snapshot at signal
//!   time, and the session-side read wrapper observes the condemnation
//!   (`is_dropped`) at its next poll — indistinguishable from EOF at
//!   that layer, so the reap exits through the ordinary FR-15 cleanup
//!   path (mid-txn abort, counter decrement, fd close). This mirrors
//!   PostgreSQL's `idle_session_timeout`, which also only interrupts a
//!   backend that is waiting for input.
//!
//! One known, deliberate race (same as PG): the condemn decision is a
//! snapshot taken worker-side, inside the reap pass. A query that
//! arrives *just after* that snapshot condemned the connection — already
//! in the kernel buffer, unread — loses: the session observes the reap
//! first and closes. The window is one pass of channel latency wide and
//! indistinguishable at the protocol level from a client that connected
//! and stayed silent.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glommio::channels::shared_channel::{ConnectedReceiver, ConnectedSender, SharedSender};

use crate::orchestrator::{Tick, TickJob};

/// Identifies one live session inside its worker's [`ReapRegistry`].
/// Reactor-local by construction — connections are pinned for life
/// (invariant 2), so the id never crosses a thread.
pub type ConnId = u64;

/// One session's reaper state (Tier A — reactor-local, zero atomics).
struct ReapEntry {
    /// When the session's last statement *completed* (its ReadyForQuery
    /// went out). `touch`ed at exactly that point, never at statement
    /// start — idle time must not include the statement's own runtime.
    last_complete: Instant,
    /// The session is parked in its message read right now. Only
    /// `waiting` entries are reap-eligible: a session mid-statement is
    /// not idle, whatever the clock says (the code-review demand that
    /// killed the naive "elapsed since completion" definition).
    waiting: bool,
    /// The parked read's current waker. Re-stored on every poll of the
    /// session's read wrapper; the reaper's wake must land on the waker
    /// of the poll that is actually parked.
    waker: Option<std::task::Waker>,
    /// Condemned. Set only by [`ReapRegistry::reap_overdue`]; the
    /// session observes it at its next read poll and exits through the
    /// FR-15 cleanup path.
    dropped: bool,
}

/// A worker's reactor-local registry of live sessions and their idle
/// clocks (Tier A: `Rc`, created in-task, never crossing a thread).
/// The reaper loop and every session on this ring share one instance.
pub struct ReapRegistry {
    entries: RefCell<HashMap<ConnId, ReapEntry>>,
    next: Cell<ConnId>,
}

impl ReapRegistry {
    pub fn new() -> Self {
        ReapRegistry {
            entries: RefCell::new(HashMap::new()),
            next: Cell::new(1),
        }
    }
}

impl Default for ReapRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ReapRegistry {

    /// Registers a session: fresh entry, idle clock started now. The
    /// session wraps the id in an unregister guard — a panic must not
    /// leave a zombie entry for every later reap pass to reconsider
    /// forever (the GaugeGuard discipline, T4.4).
    pub fn register(&self) -> ConnId {
        let id = self.next.get();
        self.next.set(id.wrapping_add(1));
        self.entries.borrow_mut().insert(
            id,
            ReapEntry {
                last_complete: Instant::now(),
                waiting: false,
                waker: None,
                dropped: false,
            },
        );
        id
    }

    /// Idle clock reset: the statement just completed (its RFB went
    /// out). This is the *only* place the clock moves — idle is
    /// measured from completion, never from connection age or statement
    /// start.
    pub fn touch(&self, id: ConnId) {
        if let Some(e) = self.entries.borrow_mut().get_mut(&id) {
            e.last_complete = Instant::now();
        }
    }

    /// Removes the session's entry (the unregister guard's Drop).
    /// Whatever the reaper was about to decide about this connection,
    /// it is gone before the decision can read it.
    pub fn unregister(&self, id: ConnId) {
        self.entries.borrow_mut().remove(&id);
    }

    /// Marks whether the session is parked in its message read right
    /// now. Set on entry to the read wrapper, cleared when it returns.
    pub fn set_waiting(&self, id: ConnId, waiting: bool) {
        if let Some(e) = self.entries.borrow_mut().get_mut(&id) {
            e.waiting = waiting;
        }
    }

    /// Records the parked read's current waker (cloned). Called on every
    /// poll before delegating to the underlying read, so a reaper wake
    /// always lands on the live poll.
    pub fn store_waker(&self, id: ConnId, waker: &std::task::Waker) {
        if let Some(e) = self.entries.borrow_mut().get_mut(&id) {
            e.waker = Some(waker.clone());
        }
    }

    /// Has the reaper condemned this connection?
    pub fn is_dropped(&self, id: ConnId) -> bool {
        self.entries
            .borrow()
            .get(&id)
            .map(|e| e.dropped)
            .unwrap_or(false)
    }

    /// One reap pass: condemn every entry that is parked in its read AND
    /// whose last completion is older than `timeout`, waking its parked
    /// read so the session observes the condemnation. Returns the number
    /// reaped. Runs on this ring — serialized with the sessions it
    /// walks, so the snapshot is exact, not approximate.
    pub fn reap_overdue(&self, timeout: Duration) -> usize {
        let mut entries = self.entries.borrow_mut();
        let mut reaped = 0;
        for e in entries.values_mut() {
            // `!e.dropped`: an entry condemned by an earlier pass whose
            // session hasn't polled yet (a backed-up ring) must not be
            // re-counted — `reaped` is the tests' and diagnostics' oracle
            // (T4.5 review F1).
            if !e.dropped && e.waiting && e.last_complete.elapsed() >= timeout {
                e.dropped = true;
                // The entry STAYS in the map (the session must still find
                // `dropped` at its next poll); the unregister guard
                // removes it when the session exits.
                if let Some(w) = e.waker.take() {
                    w.wake();
                }
                reaped += 1;
            }
        }
        reaped
    }

    /// Live entries on this ring (test observable).
    #[cfg(test)]
    pub fn live(&self) -> usize {
        self.entries.borrow().len()
    }

    /// Backdates a session's idle clock (test hook: reap eligibility
    /// without real wall-clock waits).
    #[cfg(test)]
    pub fn age_override(&self, id: ConnId, past: Duration) {
        if let Some(e) = self.entries.borrow_mut().get_mut(&id) {
            e.last_complete = Instant::now() - past;
        }
    }
}

/// One reap tick (FR-34's "signals the owning reactor"): an empty marker
/// — the state lives in each reactor's registry, so the signal carries
/// nothing. Rides the dedicated reap channel (the fourth signal family);
/// no other message type ever does.
#[derive(Debug)]
pub struct ReapTick;

/// The housekeeper-side reaper (FR-34's orchestrating half): a tick job
/// fanning a [`ReapTick`] to EVERY worker — idle connections are not a
/// designated-reactor concern, they live everywhere. `try_send` only —
/// a full channel drops the signal (counted); the next tick re-signals
/// and the pass is idempotent per snapshot, so a drop costs one tick of
/// reap latency, nothing more. Built on the serving thread (the senders
/// are connected against its executor, right before the orchestrator's
/// first tick can fire). The signal carries no timeout deliberately: the
/// idle decision is made worker-side, on the ring that owns the
/// entries — the housekeeper only rings the bell.
pub struct IdleReaper {
    /// One sender per worker, in boot's per-core order.
    senders: Vec<ConnectedSender<ReapTick>>,
    stats: ReapStats,
}

impl IdleReaper {
    /// `senders` are already connected on the serving executor; `stats`
    /// is the boot-built counter set `Runtime` exposes, whose `reaped`
    /// counter the workers' [`reap_loop`]s increment into.
    pub fn new(senders: Vec<ConnectedSender<ReapTick>>, stats: ReapStats) -> Self {
        IdleReaper { senders, stats }
    }
}

impl TickJob for IdleReaper {
    fn name(&self) -> &str {
        "idle-reaper"
    }

    fn tick(&mut self, _info: &Tick) {
        for sender in &self.senders {
            self.stats.signals.fetch_add(1, Ordering::Relaxed);
            if sender.try_send(ReapTick).is_err() {
                self.stats.refused.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The worker-side reap loop: what a [`ReapTick`] acts on — one pass
/// over this ring's registry. Shares the session's `Rc<ReapRegistry>`
/// (same ring, Tier A), so a reap snapshot is exact. The loop dies with
/// the ring or when the serving side drops its sender (shutdown joins
/// the workers, whose ring drop closes the channel).
pub async fn reap_loop(
    recv: ConnectedReceiver<ReapTick>,
    reap: Rc<ReapRegistry>,
    timeout: Duration,
    reaped: Arc<AtomicU64>,
) {
    while let Some(ReapTick) = recv.recv().await {
        let n = reap.reap_overdue(timeout);
        if n > 0 {
            reaped.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
}

/// Aggregates for `Runtime` — the same counters the tick job and the
/// workers' reap loops increment. `None` on boots with no listener or
/// with reaping disabled (`idle_reap_ms == 0`, the default): nothing
/// orchestrates.
#[derive(Debug, Clone)]
pub struct ReapStats {
    signals: Arc<AtomicU64>,
    refused: Arc<AtomicU64>,
    reaped: Arc<AtomicU64>,
}

impl ReapStats {
    /// The boot path's constructor: fresh signal/refusal counters over
    /// the one reaped counter every worker's [`reap_loop`] increments
    /// (boot hands the same `Arc` to both sides).
    pub fn new(signals: Arc<AtomicU64>, refused: Arc<AtomicU64>, reaped: Arc<AtomicU64>) -> Self {
        ReapStats {
            signals,
            refused,
            reaped,
        }
    }

    /// Reap signals attempted since boot (one per worker per tick —
    /// includes the rare full-channel attempt that was refused).
    pub fn signals(&self) -> u64 {
        self.signals.load(Ordering::Relaxed)
    }

    /// Signals dropped because a worker's reap channel was full.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Connections condemned and dropped across all workers since boot.
    pub fn reaped(&self) -> u64 {
        self.reaped.load(Ordering::Relaxed)
    }
}

/// The boot→serving-thread ingredient bundle (all `Send`, unlike the job
/// itself, which owns connected senders and never crosses a thread). One
/// `sender` per worker; the matching receivers go to the same workers in
/// the same order.
pub struct ReapInit {
    pub senders: Vec<SharedSender<ReapTick>>,
    pub stats: ReapStats,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::task::Waker;

    /// A `Waker` that flips an `Arc<AtomicBool>` — the minimal observable
    /// wake for registry tests (`std::task::Wake` owns the refcounting
    /// the raw vtable would otherwise hand-roll).
    fn flag_waker(flag: &Arc<AtomicBool>) -> Waker {
        struct WakeFlag(Arc<AtomicBool>);
        impl std::task::Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        Waker::from(Arc::new(WakeFlag(Arc::clone(flag))))
    }

    #[test]
    fn reaps_only_waiting_overdue_entries() {
        let reg = Rc::new(ReapRegistry::new());
        let parked = reg.register();
        let busy = reg.register();
        let fresh = reg.register();
        // All three are old enough; only `parked` is parked in its read
        // (the mid-statement connection must survive — the reaper's idle
        // definition is "parked AND overdue", not just "overdue").
        reg.age_override(parked, Duration::from_millis(5_000));
        reg.age_override(busy, Duration::from_millis(5_000));
        reg.age_override(fresh, Duration::from_millis(0));
        reg.set_waiting(parked, true);
        reg.set_waiting(fresh, true);
        let n = reg.reap_overdue(Duration::from_secs(1));
        assert_eq!(n, 1, "busy-mid-statement and fresh-idle survive");
        assert!(reg.is_dropped(parked));
        assert!(!reg.is_dropped(busy));
        assert!(!reg.is_dropped(fresh));
    }

    #[test]
    fn reap_wakes_the_stored_waker_and_keeps_the_entry_for_the_session() {
        let reg = Rc::new(ReapRegistry::new());
        let id = reg.register();
        reg.set_waiting(id, true);
        let flag = Arc::new(AtomicBool::new(false));
        reg.store_waker(id, &flag_waker(&flag));
        assert!(!flag.load(Ordering::SeqCst));
        reg.age_override(id, Duration::from_secs(10));
        assert_eq!(reg.reap_overdue(Duration::from_secs(1)), 1);
        assert!(flag.load(Ordering::SeqCst), "parked read must be woken");
        // The entry stays for the session to observe its own condemnation
        // (and the unregister guard to remove) — the reaper never drops
        // another task's entry behind its back.
        assert!(reg.is_dropped(id));
        assert_eq!(reg.live(), 1);
        // Observing the reap unregisters; a second pass finds nothing.
        reg.unregister(id);
        assert_eq!(reg.live(), 0);
        assert_eq!(reg.reap_overdue(Duration::from_secs(1)), 0);
    }

    #[test]
    fn unregister_preempts_the_reap_snapshot() {
        // The review's race, unit-sized: the client disconnects while the
        // reap pass is being computed. On one ring the pass and the
        // unregister serialize — here the disconnect wins by ordering,
        // and the reaped count never includes the departed connection.
        let reg = Rc::new(ReapRegistry::new());
        let id = reg.register();
        reg.set_waiting(id, true);
        reg.age_override(id, Duration::from_secs(10));
        reg.unregister(id);
        assert_eq!(reg.reap_overdue(Duration::from_secs(1)), 0);
        assert_eq!(reg.live(), 0);
    }

    #[test]
    fn touch_resets_the_idle_clock_from_completion() {
        let reg = Rc::new(ReapRegistry::new());
        let id = reg.register();
        reg.set_waiting(id, true);
        reg.age_override(id, Duration::from_secs(10));
        // The statement completed just now (the touch the session makes
        // when its RFB goes out): not overdue, whatever the connection's
        // age — idle is measured from LAST COMPLETION.
        reg.touch(id);
        assert_eq!(reg.reap_overdue(Duration::from_secs(1)), 0);
        assert!(!reg.is_dropped(id));
    }
}
