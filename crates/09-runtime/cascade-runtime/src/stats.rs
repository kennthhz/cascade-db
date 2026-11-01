//! The statistics collector (T4.6, FR-33, §4/§9): per-core counters,
//! **summed on read**.
//!
//! Each worker owns its counter set (`WorkerCounters`, §4 Tier B —
//! `Arc`-shared atomics, one set per reactor, never a cross-worker
//! aggregate at write time). Nothing on a session's hot path ever
//! touches another core's counters; attribution is by construction — a
//! connection is pinned for its whole life to one ring (invariant 2),
//! so every increment it makes lands on exactly one worker's set. The
//! aggregate exists only in [`StatsSnapshot`], materialized when
//! `Runtime::stats()` sums the per-core sets on read.
//!
//! The staged read path: the spec's collector reads via `submit_to`
//! fan-in (§6.1 — ask each reactor to produce its own slice), which is
//! what will let these counters become Tier A (reactor-private,
//! non-atomic, zero contention). T5.1 is not built yet, so the staged
//! stand-in sums the shared atomics directly from the caller — correct
//! (the increments are already the Arc-atomic substrate §4 prescribes
//! for Tier B; Relaxed loads over sums of monotonic counters are exact
//! at the read instant) and documented as the deviation, same shape as
//! the checkpointer's staged control channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// One worker's counter set (§4 Tier B): the connection gauge the
/// router's least-loaded pick reads (FR-11), plus the pg_stat_database-
/// style tallies the collector aggregates (FR-33). Per-core by
/// construction — the review question "per-core counters accidentally
/// shared?" is answered by the plumbing: each reactor's set is created
/// in boot's per-core loop and `Arc`-cloned only into that reactor's
/// router route, session tasks, and the read-side sum.
#[derive(Debug, Default)]
pub struct WorkerCounters {
    /// Live connections on this reactor (FR-15's decrement included).
    pub connections: AtomicU64,
    /// Statements processed (attempted — parse errors never reach the
    /// engine and are not counted; empty statements are protocol
    /// no-ops, EmptyQueryResponse, and are not counted — PG parity).
    pub statements: AtomicU64,
    /// Transactions committed: explicit `COMMIT` of an open transaction
    /// and autocommit writes (each is its own transaction, PG parity).
    pub commits: AtomicU64,
    /// Transactions rolled back: explicit `ROLLBACK`, the
    /// aborted-transaction `COMMIT`-acts-as-`ROLLBACK` path (25P02), a
    /// failed autocommit write's aborted implicit transaction (counted
    /// at the failure — the txn never left Idle, so no later path would
    /// count it), and the FR-15 disconnect cleanup of a
    /// mid-transaction session (reaped or otherwise).
    pub rollbacks: AtomicU64,
}

impl WorkerCounters {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The summed-on-read aggregate (FR-33's pg_stat_database-style view of
/// the whole runtime, one instant). Each counter is summed with Relaxed
/// loads over per-core sets — no fence is needed because the three
/// tallies are monotonic (a snapshot is a lower bound that every later
/// snapshot meets or exceeds); `connections` is a live gauge (it
/// decrements on disconnect), so its sum is a point-in-time sample,
/// not a bound of any kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsSnapshot {
    pub connections: u64,
    pub statements: u64,
    pub commits: u64,
    pub rollbacks: u64,
}

impl StatsSnapshot {
    /// The read-side sum: one pass over every worker's set, one place
    /// the per-core counters ever meet. Callers pass the per-core Arcs
    /// in any order — the sum is order-free.
    pub fn sum(counters: &[Arc<WorkerCounters>]) -> Self {
        let mut snap = StatsSnapshot::default();
        for c in counters {
            snap.connections += c.connections.load(Ordering::Relaxed);
            snap.statements += c.statements.load(Ordering::Relaxed);
            snap.commits += c.commits.load(Ordering::Relaxed);
            snap.rollbacks += c.rollbacks.load(Ordering::Relaxed);
        }
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_are_per_field_and_order_free() {
        let a = Arc::new(WorkerCounters::new());
        let b = Arc::new(WorkerCounters::new());
        a.statements.fetch_add(3, Ordering::Relaxed);
        b.statements.fetch_add(4, Ordering::Relaxed);
        a.commits.fetch_add(1, Ordering::Relaxed);
        b.rollbacks.fetch_add(2, Ordering::Relaxed);
        assert_eq!(
            StatsSnapshot::sum(&[b, a]),
            StatsSnapshot {
                connections: 0,
                statements: 7,
                commits: 1,
                rollbacks: 2,
            },
            "the sum is order-free and per-field"
        );
    }

    #[test]
    fn empty_fleet_sums_to_zero() {
        let all: Vec<Arc<WorkerCounters>> = Vec::new();
        assert_eq!(StatsSnapshot::sum(&all), StatsSnapshot::default());
    }
}
