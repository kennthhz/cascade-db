# cascade-undo — AI Design Context

## Role

Undo-log segment manager. Allocates sequential undo pages, writes pre-image records, exposes a stable `UndoRef` for row headers to point at.

## Pillar Reservations

- **§3.3 (Anti-VACUUM):** the pre-image storage that makes in-place updates safe for older snapshots.
- **§6.1:** undo space is per-database; counts feed the per-DB accountant.

## Hard Invariants

1. **Sequential writes.** Undo log is append-only on the hot path. No random writes within a segment.
2. **`UndoRef` stability.** Once written, the address never moves; row headers point at it indefinitely until GC.
3. **GC is snapshot-aware.** Cannot reclaim an undo record while any snapshot in any session could still need it.
4. **WAL covers undo writes** for crash recovery.

## Out of Scope

- The visibility rule itself (in `cascade-mvcc`).
- Snapshot tracking (in `cascade-mvcc`).
- Undo *page layout* — could live here or in `cascade-page` depending on coupling.

## Open Design Questions

- Page format: row-format pre-images vs. column-delta. Row-format is simpler and faster to apply; delta saves space.
- One undo segment per table vs. one per transaction vs. per-database global. PG uses per-cluster, MySQL uses per-segment-rollback. Lean: per-database, sharded by hash for concurrency.
- "Snapshot too old" semantics — surface as a query error, mimic Oracle/MySQL behavior.
