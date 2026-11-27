# cascade-btree-core

B+Tree access method.

## Role

Layer **05-access**. The primary index structure for Cascade DB. Used for primary keys, secondary indexes, and PG's hash/GIN/GiST/BRIN (built later as variants or peers).

## Public API (initial)

- `BTree<K, V>` — typed B+Tree handle.
- `BTreeCursor` — range-scan iterator with snapshot-aware visibility.
- `insert`, `delete`, `update_in_place`.

## Concurrency

Per the storage foundations (crates/02-storage/AI/storage-layer-design.md §12): one writer per scope (§12.1 principle 1, §12.2) — all mutations to one tree are serialized on the owning core. Readers share pages through the page cache (§8); cross-core reader access is that section's open question. The concrete reader/writer protocol is pinned by the detailed design under `AI/`.

## Dependencies

- `cascade-page` — node layout.
- `cascade-bpm` — page pinning.
- `cascade-types`, `cascade-error`, `cascade-telemetry`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
