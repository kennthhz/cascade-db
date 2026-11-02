//! The load view's CPU component (T5.6, FR-11, §5.2; R9 phase two, A2
//! defaults): each reactor samples its OWN thread's CPU once per interval
//! and publishes an exponentially weighted average for the router to poll.
//!
//! The design point that matters: the EWMA is computed HERE, on the
//! sampling reactor, from `getrusage(RUSAGE_THREAD)` of the thread that IS
//! the reactor (pinned for life — one thread, one core, D6). A
//! self-observation is synchronous with the sample it averages, so there
//! is no window in which the housekeeper updates the average from samples
//! that went stale in transit — the review question's failure shape has no
//! code to fail. The housekeeper's router reads the latest published
//! milli-value (Relaxed load, exactly like the connection gauge it already
//! reads) — the spec's "lightweight per-reactor atomic counter polled by
//! housekeeper" shape, no channel family of its own.
//!
//! Math (A2's recorded defaults): interval 1 s, α = 0.5 —
//! `ewma ← (ewma + sample) / 2` in integer milli-units, seeded by the
//! first sample (standard EWMA practice; converges from the first
//! interval instead of halving in from 0). The sample itself is
//! `Δcpu / Δwall` over the interval, clamped to 0..=1 000 milli — a
//! pinned thread cannot consume more CPU than wall time on its one core,
//! so the clamp is measurement slop, not physics.

use std::{
    sync::{
        atomic::Ordering,
        Arc,
    },
    time::{Duration, Instant},
};

use crate::stats::WorkerCounters;

/// α = 0.5 (A2) in integer milli-units: the new average is the midpoint
/// of the old average and the sample.
pub fn next_ewma_milli(prev: u64, sample: u64) -> u64 {
    (prev + sample) / 2
}

/// One interval's utilization: `Δcpu / Δwall` in milli-units (0..=1 000).
/// A zero wall window (a sampler resuming after a skip) yields 0 — the
/// next real window carries the utilization.
pub fn utilization_milli(cpu_delta_us: u64, wall_delta_us: u64) -> u64 {
    if wall_delta_us == 0 {
        return 0;
    }
    (cpu_delta_us.min(wall_delta_us) * 1000 / wall_delta_us).min(1000)
}

/// This thread's cumulative user+system CPU time in microseconds, or
/// `None` when the kernel refuses (no caller treats that as data — the
/// sampler just skips the step and lets the next window span the gap).
fn thread_cpu_us() -> Option<u64> {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: ru is a stack-allocated, properly aligned rusage; getrusage
    // only writes it. RUSAGE_THREAD scopes the reading to THIS thread —
    // the reactor is a pinned thread, so this IS the reactor's CPU time.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
    if rc != 0 {
        return None;
    }
    Some(
        (ru.ru_utime.tv_sec as u64 * 1_000_000 + ru.ru_utime.tv_usec as u64)
            .saturating_add(ru.ru_stime.tv_sec as u64 * 1_000_000 + ru.ru_stime.tv_usec as u64),
    )
}

/// The per-reactor sampler state. `step` is injectable (explicit clock and
/// reading) so the EWMA sequence is unit-tested exactly — the real loop is
/// a thin `getrusage` + `Instant` wrapper over it.
pub struct CpuSampler {
    last_wall: Instant,
    last_cpu_us: u64,
    ewma_milli: u64,
    first: bool,
    out: Arc<WorkerCounters>,
}

impl CpuSampler {
    pub fn new(out: Arc<WorkerCounters>) -> Self {
        CpuSampler {
            last_wall: Instant::now(),
            last_cpu_us: thread_cpu_us().unwrap_or(0),
            ewma_milli: 0,
            first: true,
            out,
        }
    }

    /// One sample→publish step at explicit `now` with explicit cumulative
    /// `cpu_us`. Returns the published value. The FIRST sample seeds the
    /// average; every later one is α = 0.5.
    pub fn step(&mut self, now: Instant, cpu_us: u64) -> u64 {
        let wall = now.duration_since(self.last_wall).as_micros() as u64;
        let cpu = cpu_us.saturating_sub(self.last_cpu_us);
        self.last_wall = now;
        self.last_cpu_us = cpu_us;
        let sample = utilization_milli(cpu, wall);
        self.ewma_milli = if self.first {
            self.first = false;
            sample
        } else {
            next_ewma_milli(self.ewma_milli, sample)
        };
        self.out.cpu_ewma_milli.store(self.ewma_milli, Ordering::Relaxed);
        self.ewma_milli
    }

    /// The real loop: sleep the interval, read this thread's rusage, step.
    /// Lives on the reactor's own ring as a sibling task (like the
    /// control/recycle/drain/reap planes); dies with the ring at shutdown.
    /// A skipped read keeps the previous window's edges, so the next
    /// successful sample spans the gap honestly.
    pub async fn run(mut self, interval: Duration) {
        loop {
            glommio::timer::sleep(interval).await;
            let now = Instant::now();
            if let Some(cpu_us) = thread_cpu_us() {
                self.step(now, cpu_us);
            }
        }
    }
}

/// Installs the sampler on the CURRENT ring (the worker's async block, at
/// boot). `load_sample_ms == 0` never reaches here — boot gates on it.
pub fn spawn_sampler(interval: Duration, out: Arc<WorkerCounters>) {
    glommio::spawn_local(CpuSampler::new(out).run(interval)).detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The EWMA sequence at α = 0.5, exact: seed, then midpoints. Milli
    /// units keep every step lossless at these values.
    #[test]
    fn ewma_is_the_midpoint_sequence_seeded_by_the_first_sample() {
        assert_eq!(next_ewma_milli(0, 100), 50, "first update halves in");
        assert_eq!(next_ewma_milli(50, 100), 75);
        assert_eq!(next_ewma_milli(75, 25), 50);
        assert_eq!(next_ewma_milli(1000, 1000), 1000, "saturated stays");
    }

    /// Utilization is Δcpu/Δwall clamped to milli-units.
    #[test]
    fn utilization_is_the_clamped_ratio() {
        assert_eq!(utilization_milli(100_000, 1_000_000), 100);
        assert_eq!(utilization_milli(0, 1_000_000), 0, "idle window");
        assert_eq!(utilization_milli(1_000_000, 1_000_000), 1000, "saturated");
        assert_eq!(
            utilization_milli(2_000_000, 1_000_000),
            1000,
            "slop above wall clamps, never extrapolates"
        );
        assert_eq!(utilization_milli(500, 0), 0, "zero-width window is 0");
    }

    /// The injectable step, driven as the real loop would drive it: a
    /// worker burning 10% of one second per interval publishes exactly the
    /// EWMA sequence — seed 100, midpoints, decay — into the shared
    /// counter the router reads. The edges are constructed directly (the
    /// fields are in-module) so no real-clock reading leaks into the
    /// exactness.
    #[test]
    fn sampler_publishes_the_exact_sequence_to_the_shared_counter() {
        let out = Arc::new(WorkerCounters::new());
        let t0 = Instant::now();
        let base_cpu = 1_000_000; // arbitrary cumulative micros
        let mut s = CpuSampler {
            last_wall: t0,
            last_cpu_us: base_cpu,
            ewma_milli: 0,
            first: true,
            out: Arc::clone(&out),
        };
        // Interval 1: 100 ms of CPU over 1 000 ms of wall → 100 milli,
        // seeding the average.
        let t1 = t0 + Duration::from_millis(1000);
        assert_eq!(s.step(t1, base_cpu + 100_000), 100);
        assert_eq!(out.cpu_ewma_milli.load(Ordering::Relaxed), 100, "the counter carries it");
        // Interval 2: another 100/1000 → (100 + 100) / 2 = 100.
        let t2 = t1 + Duration::from_millis(1000);
        assert_eq!(s.step(t2, base_cpu + 200_000), 100);
        // Interval 3: the load stops → (100 + 0) / 2 = 50 — decay, not a
        // cliff: the composite reacts to recent utilization.
        let t3 = t2 + Duration::from_millis(1000);
        assert_eq!(s.step(t3, base_cpu + 200_000), 50);
        assert_eq!(out.cpu_ewma_milli.load(Ordering::Relaxed), 50);
        // A refused reading (None) skips the step in the real loop; the
        // next one spans the accumulated gap — 300 ms over 2 000 ms of
        // wall → 150 milli, averaged in: (50 + 150) / 2 = 100.
        let t5 = t3 + Duration::from_millis(2000);
        assert_eq!(s.step(t5, base_cpu + 500_000), 100);
        assert_eq!(out.cpu_ewma_milli.load(Ordering::Relaxed), 100);
    }
}
