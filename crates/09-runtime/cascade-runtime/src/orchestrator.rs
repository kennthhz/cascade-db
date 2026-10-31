//! The orchestrator tick loop (T4.1; spec §9, D9; FR-5, FR-31's frame).
//!
//! Background work (§9 — checkpointer, reaper, statistics) is centralized
//! on the housekeeper for v1 (D9). The serving housekeeper interleaves two
//! kinds of sleep: `accept(2)` for connections and the tick wait for the
//! next grid deadline — it never spins (FR-5). The [`Orchestrator`] owns
//! the tick *schedule*; the caller (the router loop) asks it how long it
//! may sleep, and after every sleep calls [`Orchestrator::poll_due`].
//!
//! ## Scheduling contract
//!
//! Ticks live on a **fixed grid**: deadline₀ + n·interval. After a tick
//! fires, the next deadline is `deadlineₙ + interval` — not `now +
//! interval` — so healthy ticks never drift. A tick that fired *late*
//! (long job, busy housekeeper) reports its lateness but **never bursts**:
//! missed grid slots are dropped without firing and the grid slides to the
//! first slot after *now* — the phase never re-anchors at a tick's own
//! lateness, so skew cannot compound. That is the "skip missed ticks"
//! policy: per-tick skew is bounded by one job pass (bounded work ⇒
//! bounded lateness), and debt never accrues — a `loop {}` of catch-up
//! ticks is structurally impossible, and so is re-entrant scheduling: jobs
//! run sequentially on the one housekeeper thread, never nested
//! ([`Orchestrator::poll_due`] returns before the caller may call it
//! again).

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// What one firing hands its jobs: the tick's 1-based ordinal and how late
/// it fired against its grid deadline (0 for a punctual tick).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub ordinal: u64,
    pub late: Duration,
}

/// One piece of background work. Registered at boot, called in
/// registration order, once per tick, on the housekeeper thread.
pub trait TickJob: Send {
    fn name(&self) -> &str;
    fn tick(&mut self, info: &Tick);
}

/// The tick schedule plus its observables. Owned by the serving
/// housekeeper thread; its counters are `Arc`-shared so `Runtime` reads
/// them after the thread has taken ownership.
pub struct Orchestrator {
    interval: Duration,
    jobs: Vec<Box<dyn TickJob>>,
    /// Grid deadline of the next tick; `None` until first armed.
    next: Option<Instant>,
    ordinal: u64,
    ticks: Arc<AtomicU64>,
    max_late_us: Arc<AtomicU64>,
}

impl Orchestrator {
    pub fn new(interval: Duration) -> Self {
        assert!(interval > Duration::ZERO, "tick interval must be positive");
        Orchestrator {
            interval,
            jobs: Vec::new(),
            next: None,
            ordinal: 0,
            ticks: Arc::new(AtomicU64::new(0)),
            max_late_us: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn register(&mut self, job: Box<dyn TickJob>) {
        self.jobs.push(job);
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Cloneable view of the tick observables for `Runtime`. The wakeups
    /// counter counts `accept(2)` returns on the serving housekeeper — owned
    /// by the router loop, not the orchestrator — so boot stitches it in
    /// here: one stats surface for the whole tick loop.
    pub fn stats(&self, wakeups: Arc<AtomicU64>) -> OrchestratorStats {
        OrchestratorStats {
            interval: self.interval,
            ticks: Arc::clone(&self.ticks),
            max_late_us: Arc::clone(&self.max_late_us),
            wakeups,
        }
    }

    /// How long the caller may sleep before a tick could be due. Arming the
    /// grid on first call; floored at 1 ms so a just-missed deadline never
    /// produces a zero-length wait (the caller's sleep primitive may treat
    /// that as "no timeout").
    pub fn until_next(&self) -> Duration {
        let now = Instant::now();
        let deadline = match self.next {
            Some(d) => d,
            None => return self.interval,
        };
        deadline
            .saturating_duration_since(now)
            .max(Duration::from_millis(1))
    }

    /// Fire the tick if its deadline passed — at most one. Returns whether
    /// a tick fired. Skip-late: one firing regardless of how many grid
    /// slots elapsed while the housekeeper was busy; the grid slides to
    /// the first slot after `now` — punctual ticks advance exactly one
    /// slot, buried slots are dropped without firing, and the phase never
    /// re-anchors at a tick's own lateness.
    pub fn poll_due(&mut self) -> bool {
        let now = Instant::now();
        let Some(deadline) = self.next else {
            // First call arms the grid: the first tick is one interval out.
            self.next = Some(now + self.interval);
            return false;
        };
        if now < deadline {
            return false;
        }
        let late = now - deadline;
        self.ordinal += 1;
        let info = Tick {
            ordinal: self.ordinal,
            late,
        };
        for job in &mut self.jobs {
            job.tick(&info);
        }
        self.ticks.fetch_add(1, Ordering::Relaxed);
        self.max_late_us
            .fetch_max(late.as_micros() as u64, Ordering::Relaxed);
        // Skip missed grid slots without firing them (no burst), keeping
        // the grid *phase*: the next deadline is the first slot after
        // `now`, i.e. `deadline + k·interval`. A punctual tick advances
        // exactly one slot — the grid never re-anchors at the tick's own
        // lateness, so drift cannot compound. (Re-arming at
        // `now + interval` instead would shift the phase by `late` every
        // firing — the doc's promised property is the grid, keep it.)
        let mut next = deadline;
        loop {
            match next.checked_add(self.interval) {
                // Strictly-after-`now`: a slot due *right now* would fire
                // on the next poll, bursting the skipped debt back.
                Some(n) if n > now => {
                    self.next = Some(n);
                    break;
                }
                Some(n) => next = n,
                // Instant overflow (practically unreachable): re-anchor.
                None => {
                    self.next = Some(now + self.interval);
                    break;
                }
            }
        }
        true
    }

    /// Ticks fired since boot (one per due grid slot, missed slots skipped).
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    /// The worst lateness any tick fired with.
    pub fn max_late(&self) -> Duration {
        Duration::from_micros(self.max_late_us.load(Ordering::Relaxed))
    }
}

/// `Runtime`'s view of the tick observables — the same atomics the
/// housekeeper thread updates. `None` when no listener was handed to boot
/// (nothing serves, nothing ticks: D9 centralizes background work on the
/// *serving* housekeeper).
#[derive(Debug, Clone)]
pub struct OrchestratorStats {
    interval: Duration,
    ticks: Arc<AtomicU64>,
    max_late_us: Arc<AtomicU64>,
    wakeups: Arc<AtomicU64>,
}

impl OrchestratorStats {
    /// The configured tick interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Ticks fired since boot.
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    /// The worst lateness any tick fired with — the "skew under load
    /// bounded" observable (T4.1).
    pub fn max_late(&self) -> Duration {
        Duration::from_micros(self.max_late_us.load(Ordering::Relaxed))
    }

    /// `accept(2)` returns on the serving housekeeper, timed out or not.
    /// An idle server wakes roughly once per tick; a busy-waiting loop
    /// would show millions per second — the FR-5 negative test reads this.
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }
}

/// The §7.2 stall sampler as a tick job: the periodic call
/// `Runtime::poll_stalls` documented as arriving "with the housekeeper
/// (M4+)". A stalled reactor logs through the coop watchdog exactly as the
/// exit-path read does; idle reactors cost one atomic load per tick.
pub struct StallWatch {
    stalls: Vec<Arc<crate::coop::ReactorStall>>,
    soft: Duration,
    hard: Duration,
}

impl StallWatch {
    pub fn new(
        stalls: Vec<Arc<crate::coop::ReactorStall>>,
        soft: Duration,
        hard: Duration,
    ) -> Self {
        StallWatch { stalls, soft, hard }
    }
}

impl TickJob for StallWatch {
    fn name(&self) -> &str {
        "stall-watch"
    }

    fn tick(&mut self, _info: &Tick) {
        for stall in &self.stalls {
            crate::coop::poll_stall(stall, self.soft, self.hard);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Log = Arc<Mutex<Vec<(u64, u128)>>>;

    fn ordinals(log: &Log) -> Vec<(u64, u128)> {
        log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A job recording `(ordinal, late_us)` into a shared log the test
    /// thread reads after the ticks fired.
    struct Recorder(Log);

    impl TickJob for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn tick(&mut self, info: &Tick) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((info.ordinal, info.late.as_micros()));
        }
    }

    #[test]
    fn first_poll_arms_the_grid_without_firing() {
        let mut orch = Orchestrator::new(Duration::from_millis(20));
        assert!(!orch.poll_due(), "arming is not a tick");
        assert_eq!(orch.ticks(), 0);
    }

    #[test]
    fn due_tick_fires_jobs_once_in_registration_order() {
        let log_a: Log = Arc::new(Mutex::new(Vec::new()));
        let log_b: Log = Arc::new(Mutex::new(Vec::new()));
        let mut orch = Orchestrator::new(Duration::from_millis(20));
        orch.register(Box::new(Recorder(Arc::clone(&log_a))));
        orch.register(Box::new(Recorder(Arc::clone(&log_b))));
        assert!(!orch.poll_due(), "arming is not a tick");
        std::thread::sleep(Duration::from_millis(45));
        assert!(orch.poll_due(), "past the deadline, a tick fires");
        let seen = ordinals(&log_a);
        assert_eq!(seen.len(), 1, "first job fired exactly once");
        assert_eq!(seen[0].0, 1, "first job saw ordinal 1");
        assert_eq!(ordinals(&log_b).len(), 1, "second job fired too");
        assert_eq!(orch.ticks(), 1);

        // One poll fires at most one tick — even if the next deadline also
        // passed while we stood here (the grid re-arms, it never bursts).
        std::thread::sleep(Duration::from_millis(45));
        assert!(orch.poll_due());
        assert!(!orch.poll_due(), "no burst catch-up");
        assert_eq!(orch.ticks(), 2);
    }

    /// The T4.1 code-review item "timer drift compounding": ticks land on
    /// the fixed grid. A loop of punctual polls must fire ≈ window/interval
    /// ticks — not fewer (drift accumulating) and not more (busy poll).
    #[test]
    fn punctual_ticks_stay_on_the_grid() {
        let mut orch = Orchestrator::new(Duration::from_millis(20));
        assert!(!orch.poll_due()); // arm
        let mut fired = 0;
        let window = Instant::now() + Duration::from_millis(300);
        while Instant::now() < window {
            if orch.poll_due() {
                fired += 1;
            }
            // Poll hot: the orchestrator itself must not busy-fire between
            // grid slots.
            std::thread::sleep(Duration::from_millis(2));
        }
        // 15 slots nominal; ±6 tolerates scheduler slop on a loaded box.
        assert!(
            (8..=21).contains(&fired),
            "grid drift: {fired} ticks in 300ms at 20ms interval"
        );
    }

    /// The other review item — "tick overtaking itself" — is structural
    /// (jobs run sequentially, one firing per poll), so the test pins the
    /// *observable*: a slow job buries the grid; the next tick reports its
    /// lateness, fires exactly once, and re-arms one interval from *now*
    /// (late ≈ 0 for the following punctual tick). Skew stays bounded; no
    /// debt spirals.
    #[test]
    fn slow_tick_reports_lateness_then_returns_to_punctual() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let mut orch = Orchestrator::new(Duration::from_millis(20));
        orch.register(Box::new(Recorder(Arc::clone(&log))));
        assert!(!orch.poll_due()); // arm

        std::thread::sleep(Duration::from_millis(45));
        assert!(orch.poll_due(), "tick 1: on time-ish");

        // The "slow job": the housekeeper busy elsewhere for ~5 intervals.
        std::thread::sleep(Duration::from_millis(100));
        assert!(orch.poll_due(), "tick 2: fires despite the burial");
        let (_, late_us) = ordinals(&log).last().copied().expect("tick 2 recorded");
        assert!(
            late_us >= 20_000,
            "the burial must be visible as lateness, got {late_us}µs"
        );
        assert_eq!(orch.ticks(), 2, "buried slots are skipped, never burst");
        // The grid slid to the first slot after *now*: an immediate
        // re-poll must not fire — no burst catch-up even after a burial.
        assert!(!orch.poll_due(), "slid grid: no burst");

        // The next tick is punctual again — the debt did not compound into
        // permanent lateness. A hot poll loop catches it at the slid slot:
        // a naive "advance one slot" re-arm would leave the deadline in the
        // past and burst here with ~an interval of debt lateness, failing
        // the bound below; the slide lands the tick near-zero late.
        let window = Instant::now() + Duration::from_millis(60);
        loop {
            if orch.poll_due() {
                break;
            }
            assert!(
                Instant::now() < window,
                "tick 3 never fired after the re-arm"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(orch.ticks(), 3, "one firing per burial, never more");
        let (_, late3_us) = ordinals(&log).last().copied().expect("tick 3 recorded");
        assert!(
            late3_us < 10_000,
            "post-burial ticks must be punctual again, got {late3_us}µs late"
        );
        assert!(orch.max_late() >= Duration::from_millis(20), "max_late saw the burial");
    }

    /// The StallWatch is the frame's first real job — a smoke check that
    /// registration dispatches it (the stall totals move only under actual
    /// stalls; an idle reactor costs nothing, which is fine here).
    #[test]
    fn stall_watch_job_dispatches() {
        let stall = Arc::new(crate::coop::ReactorStall::new());
        let mut orch = Orchestrator::new(Duration::from_millis(20));
        orch.register(Box::new(StallWatch::new(
            vec![Arc::clone(&stall)],
            Duration::from_millis(1),
            Duration::from_millis(10),
        )));
        assert!(!orch.poll_due(), "arming is not a tick");
        std::thread::sleep(Duration::from_millis(45));
        assert!(orch.poll_due(), "the registered job's tick fires");
        assert_eq!(orch.ticks(), 1);
    }
}
