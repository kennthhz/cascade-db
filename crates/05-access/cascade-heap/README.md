# cascade-heap

Heap table access method.

## Role

Layer **05-access**. The row store for primary table data. Implements in-place MVCC (§3.3): rows carry xmin/xmax/undo-ref headers, updates rewrite the row and stash the pre-image in the undo log.

## Public API (initial)

- `Heap` — handle to a heap relation.
- `insert(row, txn) -> Tid`.
- `update_in_place(tid, row, txn)` — writes undo, mutates the row.
- `delete(tid, txn)` — sets xmax, optionally writes undo for older snapshots.
- `scan(predicate, snapshot)` — visibility-aware iterator.

## Dependencies

- `cascade-page` — page layout.
- `cascade-bpm` — page pinning.
- `cascade-mvcc` — visibility rules, txn handle.
- `cascade-undo` — pre-image storage.
- `cascade-types`, `cascade-error`, `cascade-telemetry`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.3.
