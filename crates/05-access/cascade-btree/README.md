# cascade-btree

B+Tree access method with Optimistic Lock Coupling (OLC).

## Role

Layer **05-access**. The primary index structure for Cascade DB. Used for primary keys, secondary indexes, and PG's hash/GIN/GiST/BRIN (built later as variants or peers).

## Public API (initial)

- `BTree<K, V>` — typed B+Tree handle.
- `BTreeCursor` — range-scan iterator with snapshot-aware visibility.
- `insert`, `delete`, `update_in_place`.

## Concurrency

Optimistic Lock Coupling — readers proceed without latching, validating version counters; writers latch only the page being modified. Designed to scale linearly with cores under thread-per-core (§3.2).

## Dependencies

- `cascade-page` — node layout.
- `cascade-bpm` — page pinning.
- `cascade-types`, `cascade-error`, `cascade-telemetry`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
