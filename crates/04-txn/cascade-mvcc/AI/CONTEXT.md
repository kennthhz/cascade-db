# cascade-mvcc — AI Design Context

## Role

The MVCC engine: XID allocation, snapshots, visibility, transaction state machine.

## Pillar Reservations

- **§3.3 (In-place MVCC):** the defining pillar — rows are updated in-place, pre-images go to the undo log, visibility is computed from row headers + snapshot.
- **§3.4 (Per-DB Xid):** each `DatabaseId` has its own `Xid` sequence. No global counter.
- **§6.3 (Logical CDC):** xid + commit order must be reconstructable from WAL alone.

## Hard Invariants

1. **In-place updates.** No append-only tuple chains. Updates overwrite the row; the pre-image goes to the undo log.
2. **Snapshot once.** A transaction captures its visibility set at start (or first read) and reuses it. No re-snapshotting mid-statement.
3. **Wide XIDs.** 64-bit. No wraparound vacuum. (PG's 32-bit wraparound is part of what we're escaping.)
4. **Per-DB.** XID space is scoped to `DatabaseId`. Cross-DB transactions are not supported (per the PG decision documented in §3.4).
5. **WAL records carry the XID.** Required for §6.3 logical decoding.

## Out of Scope

- Undo log on-disk layout (lives in `cascade-undo`).
- Lock manager (Phase 3, possibly its own crate later).
- Distributed transactions (out of v1 scope; revisit when §6.4 page service is real).

## Open Design Questions

- Isolation levels: ship all four PG levels in v1 or start with Read Committed + Repeatable Read? PG defaults are Read Committed.
- Snapshot data structure — sorted vector vs. roaring bitmap.
- Hot-row contention: do we adopt optimistic-concurrency-control hints from the planner?
