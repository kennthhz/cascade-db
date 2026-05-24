# cascade-mvcc

In-place MVCC, transaction state, snapshots, visibility rules.

## Role

Layer **04-txn**. Implements pillar §3.3 (Anti-VACUUM Engine). Per-database `Xid` space (pillar §3.4). Coordinates with `cascade-undo` for rollback segments and `cascade-wal` for durability.

## Public API (initial)

- `Txn` — transaction handle with isolation level + snapshot.
- `Snapshot` — immutable visibility set.
- `XidAllocator` — per-database, monotonic, wide enough to avoid wraparound.
- `visible_to(xid, snapshot, row_header)` — the visibility predicate.

## Dependencies

- `cascade-wal` — to log txn boundaries (`BEGIN`/`COMMIT`/`ABORT`).
- `cascade-undo` — to preserve pre-image rows during in-place updates.
- `cascade-types`, `cascade-error`, `cascade-telemetry`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.3, §3.4.
