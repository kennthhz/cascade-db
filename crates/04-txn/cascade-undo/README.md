# cascade-undo

Undo-log rollback segments. The "U" in in-place MVCC.

## Role

Layer **04-txn**. Implements the storage side of pillar §3.3 — when a row is updated in place, its pre-image is written to a sequential undo segment so older snapshots and aborted transactions can reconstruct it.

## Public API (initial)

- `UndoSegmentMgr` — allocates, recycles, GCs segments.
- `UndoRef` — pointer from a row header back to its pre-image record.
- `read_pre_image(undo_ref) -> Row` — used by snapshot reads and rollback.

## Dependencies

- `cascade-bpm` (undo pages live in the buffer pool)
- `cascade-storage` (extents, allocation)
- `cascade-types`, `cascade-error`, `cascade-telemetry`

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.3.
