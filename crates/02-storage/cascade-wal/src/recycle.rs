//! The recycle boundary (T4.3, FR-32): which bytes of a DB's log the
//! recycler may reclaim, given the checkpointer's truncatable frontier.
//!
//! LSNs in this WAL *are* byte offsets (`WalWriter` hands out reservations
//! as offsets), so "segments below the per-DB truncatable LSN" reduces to
//! byte windows of `db_<db>.log`. The decision is deliberately
//! conservative — it can only ever be loosened by a redesign of the
//! frontier's promise, never by the recycler:
//!
//! - **Strictly below.** The frontier `T` promises WAL strictly below `T`
//!   is not needed to reconstruct durable state ([`crate::writer`]'s
//!   durable watermark aside, the checkpointer advances `T` only to the
//!   minimum stamp over flushed-candidate pages). The record AT `T` and
//!   everything after it are always protected — the floor computation
//!   below can never claim a window containing byte `T`.
//! - **Full windows only.** Reclamation is quantized to
//!   [`RECYCLE_SEGMENT_BYTES`] windows (the PG 16 MB segment analog): a
//!   window is reclaimable only when its *end* is at or below `T`. A
//!   window straddling `T` is protected even though most of its bytes are
//!   expendable — never punch inside a window that recovery might need.
//! - **Per call, per DB.** The decision takes only that DB's frontier;
//!   one database's recycle can never cover another's bytes (FR-32's
//!   isolation, structural in the pure function).
//!
//! The *physical* act for a single-file-per-DB log (rotate-and-retire,
//! segment files) arrives with the segmented WAL layout (§8.2); until
//! then the runtime's recycler computes this decision and counts — and
//! the live engine's frontier is permanently 0 (no worker BPM exists:
//! rows live only in the log), so nothing is ever reclaimable in
//! production, exactly as the debug engine's replay-from-zero recovery
//! requires. The boundary arithmetic is where the off-by-one risk the
//! plan flags lives (recycling the segment recovery needs), and it is
//! pinned by the tests below at the exact frontier byte and ±1.

use cascade_types::Lsn;

/// The recycle window (the PG 16 MB segment analog). A debug-engine log
/// never reaches it; it exists so the decision quantizes identically on
/// any log size and the segmented-WAL future inherits the constant.
pub const RECYCLE_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;

/// What may be reclaimed for one DB at one frontier snapshot. Bytes
/// `[0, reclaimable_bytes)` are reclaimable; bytes
/// `[protected_at, …)` are not — and `protected_at == reclaimable_bytes`:
/// the protecting window is `[protected_at, protected_at + segment)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecycleDecision {
    /// The frontier snapshot the decision was computed from.
    pub frontier: Lsn,
    /// Reclaimable bytes: every byte strictly below the frontier, rounded
    /// down to a whole window. Never above `frontier.0`.
    pub reclaimable_bytes: u64,
    /// The first protected byte (the start of the window straddling or
    /// touching the frontier).
    pub protected_at: Lsn,
}

/// The recycle boundary for one DB (FR-32). `segment_bytes` of 0 is
/// saturated to 1 — a zero window size is a caller bug, but the answer
/// must stay total and safe (every byte its own window), never a panic on
/// the recycler's tick.
pub fn recycle_decision(frontier: Lsn, segment_bytes: u64) -> RecycleDecision {
    let seg = segment_bytes.max(1);
    let full_windows = (frontier.0 / seg) * seg;
    RecycleDecision {
        frontier,
        reclaimable_bytes: full_windows,
        protected_at: Lsn(full_windows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ±1 matrix the plan's test review demands: the boundary is
    /// pinned at exactly-truncatable, not just "clearly old / new".
    /// Windows end at 100·k; the window ending exactly AT the frontier is
    /// reclaimable (all its bytes are strictly below); the one ending one
    /// byte later is not.
    #[test]
    fn boundary_at_exactly_the_frontier_is_reclaimable() {
        let s = 100;
        assert_eq!(recycle_decision(Lsn(300), s).reclaimable_bytes, 300);
        assert_eq!(recycle_decision(Lsn(301), s).reclaimable_bytes, 300);
        assert_eq!(recycle_decision(Lsn(299), s).reclaimable_bytes, 200);
        assert_eq!(recycle_decision(Lsn(0), s).reclaimable_bytes, 0);
        assert_eq!(
            recycle_decision(Lsn(300), s).protected_at,
            Lsn(300),
            "the first protected byte is the first byte of the straddling window"
        );
    }

    /// The off-by-one proof, as a property: for every frontier, the
    /// reclaimable prefix stays below the frontier AND the protecting
    /// window covers the frontier byte itself — the record AT the
    /// truncatable LSN can never fall inside a reclaimed window.
    #[test]
    fn the_frontier_byte_is_never_inside_a_reclaimed_window() {
        let s = 64;
        for t in 0..2000u64 {
            let d = recycle_decision(Lsn(t), s);
            assert!(
                d.reclaimable_bytes <= t,
                "frontier {t}: claimed {d:?} above the frontier"
            );
            assert!(
                t < d.reclaimable_bytes + s,
                "frontier {t}: protecting window ends at {} ≤ {t}",
                d.reclaimable_bytes + s
            );
            assert_eq!(d.protected_at, Lsn(d.reclaimable_bytes));
        }
    }

    #[test]
    fn zero_segment_size_saturates_instead_of_panicking() {
        // Every byte its own window: all strictly-below bytes reclaimable,
        // the frontier byte protected — total and safe, never a divide by
        // zero on a tick.
        let d = recycle_decision(Lsn(5), 0);
        assert_eq!(d.reclaimable_bytes, 5);
        assert_eq!(d.protected_at, Lsn(5));
    }

    /// The production window: a debug-engine frontier (byte-scale LSNs)
    /// never reaches 16 MB — nothing reclaimable. This is the live
    /// engine's standing truth, pinned here so a change to either side of
    /// the inequality has to explain itself.
    #[test]
    fn debug_scale_frontiers_reclaim_nothing_at_production_window() {
        assert_eq!(
            recycle_decision(Lsn(1 << 20), RECYCLE_SEGMENT_BYTES).reclaimable_bytes,
            0
        );
    }
}
