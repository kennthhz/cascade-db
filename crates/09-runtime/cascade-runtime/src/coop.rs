//! Cooperative-yield contract + reactor stall detection (§7.1, §7.2;
//! FR-17, FR-18, FR-19).
//!
//! [`yield_point`] implements §7.1's exact three cases:
//! 1. CPU time since the last yield ≥ the soft deadline → yield now.
//! 2. Below the deadline, run queue non-empty → delegate to glommio's
//!    `yield_if_needed` (yields only then).
//! 3. Below the deadline, queue empty → no-op.
//!
//! Cases 2/3 share one delegation: `yield_if_needed` is glommio's primitive
//! and decides internally (run queue / latency / preemption — its source's
//! `need_preempt`); case 3 is its immediate return. The deadline (case 1) is
//! what glommio does not implement and why the runtime owns this helper.
//!
//! The "CPU time since last yield" clock is approximated with monotonic
//! *wall* time between real yields (`Instant` — CLOCK_MONOTONIC, no wrap).
//! A task that just woke from a long I/O wait may yield once early; the
//! cell refreshes on that yield, so no repeat — conservative in the right
//! direction (a busy reactor yields sooner, never later).
//!
//! ## Stall detection (§7.2, A15)
//!
//! A reactor that is *idle* is not stalled, but it also does not yield — so
//! a literal yield-to-yield wall-clock gap false-positives on any workload
//! with think-time between queries (tried in T1.8; the boot test failed
//! immediately). The detector therefore tracks *scheduler starvation*:
//! every reactor runs a heartbeat task beating every [`BEAT_INTERVAL`]; a
//! compute stretch that starves the run queue starves the heartbeat too.
//! The housekeeper polls [`poll_stall`]: gap = now − last heartbeat,
//! classified by [`stall_level`] — ≥ soft increments
//! `reactor_stall_soft_total` + INFO log, ≥ hard increments
//! `reactor_stall_hard_total` + WARN log, both published through
//! cascade-telemetry under those canonical names. This measures "gap
//! between scheduler chances" — the §7.2 quantity that distinguishes a
//! stalled reactor from a parked one.
//!
//! `BEAT_INTERVAL` must stay well below the soft threshold (it is how fresh
//! "the reactor is alive" can possibly be). Kill switch: config-only, and
//! validation rejects enabling it until kill wiring lands (plan R12:
//! default off — no task is killed under default v1 configuration).
//! Detection of a stall *in progress* lands with the housekeeper tick loop
//! (M4+); today [`crate::Runtime::poll_stalls`] is the poll surface.

use std::{
    cell::{Cell, RefCell},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

use cascade_telemetry::MetricsRegistry;

/// Canonical counter names (§7.2) — the exporter contract.
pub const SOFT_TOTAL: &str = "reactor_stall_soft_total";
pub const HARD_TOTAL: &str = "reactor_stall_hard_total";

/// Heartbeat period. Must stay well below the smallest useful soft
/// threshold — it bounds how fresh "the reactor is alive" can be. 200 µs
/// costs 5 000 timer wakes/s per reactor (io_uring timeouts, µs-scale
/// each); revisit against real perf runs before changing.
pub const BEAT_INTERVAL: Duration = Duration::from_micros(200);

/// Process-wide monotonic anchor for the microsecond heartbeat clock.
fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64
}

/// Per-reactor stall state, shared between the reactor (heartbeat writer)
/// and the housekeeper poll / tests (readers).
#[derive(Debug)]
pub struct ReactorStall {
    /// Microseconds on the [`now_us`] clock of the last heartbeat.
    last_beat_us: AtomicU64,
    soft_total: AtomicU64,
    hard_total: AtomicU64,
}

impl ReactorStall {
    /// The only constructor: beats once so a fresh instance can never be
    /// mistaken for a reactor stalled since the epoch (a `last_beat_us` of
    /// zero would report a process-lifetime gap on first poll).
    pub fn new() -> Self {
        let s = Self {
            last_beat_us: AtomicU64::new(0),
            soft_total: AtomicU64::new(0),
            hard_total: AtomicU64::new(0),
        };
        s.beat();
        s
    }

    /// Heartbeat: the reactor got scheduler time.
    pub fn beat(&self) {
        self.last_beat_us.store(now_us(), Ordering::Relaxed);
    }

    /// Microseconds since the last beat, on this process's monotonic
    /// [`now_us`] clock — the same quantity [`poll_stall`] classifies,
    /// readable directly (diagnostics; tests sample it in-reactor where no
    /// teardown beat can race the measurement).
    pub fn gap_us(&self) -> u64 {
        now_us().saturating_sub(self.last_beat_us.load(Ordering::Relaxed))
    }

    /// Gaps that reached the soft threshold (§7.2: ≥ 1 ms).
    pub fn soft_total(&self) -> u64 {
        self.soft_total.load(Ordering::Relaxed)
    }

    /// Gaps that reached the hard threshold (§7.2: ≥ 10 ms).
    pub fn hard_total(&self) -> u64 {
        self.hard_total.load(Ordering::Relaxed)
    }
}

impl Default for ReactorStall {
    /// Delegates to [`new`](Self::new): even `Default::default()` starts
    /// with a fresh beat, closing the stale-epoch footgun.
    fn default() -> Self {
        Self::new()
    }
}

struct ReactorState {
    last_yield: RefCell<Option<Instant>>,
    deadline: Cell<Duration>,
    /// None on threads that are not a configured reactor (yield_point is a
    /// no-op there — pure functions in tests, housekeeper threads).
    stall: RefCell<Option<Arc<ReactorStall>>>,
    /// Branch instrumentation: real deadline yields and queue delegations.
    /// The no-op case increments neither — the tests assert each branch by
    /// failing if it is removed.
    deadlined_yields: Cell<u64>,
    queue_delegations: Cell<u64>,
}

thread_local! {
    static REACTOR: ReactorState = const {
        ReactorState {
            last_yield: RefCell::new(None),
            deadline: Cell::new(Duration::from_micros(500)),
            stall: RefCell::new(None),
            deadlined_yields: Cell::new(0),
            queue_delegations: Cell::new(0),
        }
    };
}

/// Install reactor-local state at executor start (boot does this for every
/// worker; tests do it for their own executors). Resets the yield clock —
/// the reactor starts with a full deadline budget.
pub fn configure_for_reactor(stall: Arc<ReactorStall>, deadline: Duration) {
    REACTOR.with(|r| {
        r.last_yield.borrow_mut().replace(Instant::now());
        r.deadline.set(deadline);
        r.stall.borrow_mut().replace(stall);
        r.deadlined_yields.set(0);
        r.queue_delegations.set(0);
    });
}

/// Spawn the reactor's heartbeat task. Must be called inside a configured
/// reactor (boot does this right after [`configure_for_reactor`]); the task
/// lives for the executor's lifetime.
pub fn spawn_heartbeat() {
    glommio::spawn_local(async {
        loop {
            glommio::timer::sleep(BEAT_INTERVAL).await;
            REACTOR.with(|r| {
                if let Some(stall) = r.stall.borrow().clone() {
                    stall.beat();
                }
            });
        }
    })
    .detach();
}

/// Deadline-yields performed by this reactor since `configure_for_reactor`.
pub fn deadlined_yields() -> u64 {
    REACTOR.with(|r| r.deadlined_yields.get())
}

/// `yield_if_needed` delegations performed by this reactor since
/// configuration (a delegation yields only when glommio's scheduler says
/// so; otherwise it returns immediately).
pub fn queue_delegations() -> u64 {
    REACTOR.with(|r| r.queue_delegations.get())
}

/// §7.1's cooperative yield point. Operators call this at every vectorized
/// batch boundary and at internal sub-boundaries (I10, FR-18).
pub async fn yield_point() {
    let state = REACTOR.with(|r| {
        let elapsed = r
            .last_yield
            .borrow()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO);
        (elapsed, r.deadline.get())
    });

    if state.0 >= state.1 {
        // Case 1: over the soft deadline — yield even with an empty queue
        // (glommio's yield_if_needed would not).
        glommio::executor().yield_now().await;
        REACTOR.with(|r| {
            r.last_yield.borrow_mut().replace(Instant::now());
            r.deadlined_yields.set(r.deadlined_yields.get() + 1);
        });
    } else {
        // Cases 2/3: delegate — glommio yields iff its scheduler says so
        // (non-empty queue), otherwise this returns immediately (no-op).
        glommio::yield_if_needed().await;
        REACTOR.with(|r| {
            r.queue_delegations.set(r.queue_delegations.get() + 1);
        });
    }
}

/// Which §7.2 action a gap triggers: `None` below soft, INFO for a soft
/// gap, WARN for a hard gap (a hard gap is also a soft gap — the thresholds
/// are independent). Pure so the classification is testable without
/// wall-clock races.
fn stall_level(gap: Duration, soft: Duration, hard: Duration) -> Option<cascade_telemetry::Level> {
    if gap >= hard {
        Some(cascade_telemetry::Level::Warn)
    } else if gap >= soft {
        Some(cascade_telemetry::Level::Info)
    } else {
        None
    }
}

/// One housekeeper-style sample of a reactor's heartbeat (A15): classify
/// the starvation gap, increment the §7.2 counters, and log at the mapped
/// level. Returns the level acted on (`None` below soft).
///
/// Counter semantics: one increment *per poll* above a threshold, not per
/// episode — a reactor stuck for 1 s polls as ~1 000 soft hits at the
/// default 1 ms tick. That sampling rate is itself the signal an exporter
/// graphs; episode deduplication belongs to the consumer.
pub fn poll_stall(
    stall: &ReactorStall,
    soft: Duration,
    hard: Duration,
) -> Option<cascade_telemetry::Level> {
    let gap_us = now_us().saturating_sub(stall.last_beat_us.load(Ordering::Relaxed));
    let gap = Duration::from_micros(gap_us);
    let level = stall_level(gap, soft, hard)?;

    stall.soft_total.fetch_add(1, Ordering::Relaxed);
    let registry = MetricsRegistry::new();
    registry.add(SOFT_TOTAL, 1);
    if gap >= hard {
        stall.hard_total.fetch_add(1, Ordering::Relaxed);
        registry.add(HARD_TOTAL, 1);
    }
    let gap_us = gap_us.to_string();
    cascade_telemetry::log(level, "reactor_stall", &[("gap_us", gap_us.as_str())]);
    Some(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_by_threshold_with_hard_implying_soft() {
        let (soft, hard) = (Duration::from_millis(1), Duration::from_millis(10));
        // Below soft: no action, no log.
        assert_eq!(stall_level(Duration::from_micros(999), soft, hard), None);
        // Exactly at a threshold counts (≥ semantics, §7.2).
        assert_eq!(
            stall_level(Duration::from_millis(1), soft, hard),
            Some(cascade_telemetry::Level::Info)
        );
        // Between soft and hard: INFO only.
        assert_eq!(
            stall_level(Duration::from_millis(9), soft, hard),
            Some(cascade_telemetry::Level::Info)
        );
        // At and beyond hard: WARN.
        assert_eq!(
            stall_level(Duration::from_millis(10), soft, hard),
            Some(cascade_telemetry::Level::Warn)
        );
        assert_eq!(
            stall_level(Duration::from_secs(3600), soft, hard),
            Some(cascade_telemetry::Level::Warn)
        );
    }

    #[test]
    fn poll_stall_classifies_counts_and_logs() {
        // Ensure the anchor clock has headroom: rewinding the beat by N µs
        // below `now` must never saturate at zero (tests start < 1 ms after
        // the anchor on fast machines). Spin *before* creating the stall —
        // `new()` beats, and this spin must not count as starvation.
        while now_us() < 12_000 {}

        let stall = ReactorStall::new();
        // Fresh beat: nothing due.
        assert_eq!(
            poll_stall(&stall, Duration::from_millis(1), Duration::from_millis(10)),
            None
        );
        assert_eq!((stall.soft_total(), stall.hard_total()), (0, 0));

        // Simulate starvation by winding the beat back on the anchor clock.
        stall
            .last_beat_us
            .store(now_us().saturating_sub(3_000), Ordering::Relaxed);
        assert_eq!(
            poll_stall(&stall, Duration::from_millis(1), Duration::from_millis(10)),
            Some(cascade_telemetry::Level::Info)
        );
        assert_eq!((stall.soft_total(), stall.hard_total()), (1, 0));

        stall
            .last_beat_us
            .store(now_us().saturating_sub(11_000), Ordering::Relaxed);
        assert_eq!(
            poll_stall(&stall, Duration::from_millis(1), Duration::from_millis(10)),
            Some(cascade_telemetry::Level::Warn)
        );
        assert_eq!((stall.soft_total(), stall.hard_total()), (2, 1));
    }
}
