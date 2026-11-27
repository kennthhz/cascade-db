# cascade-heap — AI Design Context

## Role

The heap table engine — **the compatibility engine**: append-only row versions, HOT chains, incremental prune and vacuum. The `AnyTable::Heap` arm. The default engine is the index-organized one (`cascade-iot`); this crate serves tables without a primary key and applications that depend on PostgreSQL's heap behavior (product definition §4.1).

**Authoritative design:** [`AI/heap-design.md`](heap-design.md) (the engine — decisions H1–H13) and [`cascade-page/AI/storage-page-mutation-design.md`](../../cascade-page/AI/storage-page-mutation-design.md) D1–D6 (the heap page format — locked; this crate obeys it). Layer contracts: [`access-layer-design.md`](../../AI/access-layer-design.md) §6 (the contract), §8 (execution model), §9 (the write path).

## Pillar Reservations

- **Compatibility engine (product definition §4.1):** every update writes a new version; cleanup is continuous and incremental — prune on access, vacuum one page at a time. Operators never run `VACUUM FULL`, and there is no freeze. The in-place design with undo belongs to `cascade-iot`, not here.
- **Change data capture:** inserts carry the cell verbatim through the page format's own encoder; every mutation is a full-image redo record.
- **Columnar propagation (reserved):** writes that should reach a columnar projection do so through the log, never by a direct call.

## Hard Invariants

1. **Append-only versions.** No row is overwritten. An update writes a new version.
2. **No undo.** Rollback is the abort record plus the status table. `cascade-undo` is not a dependency of this crate. What that costs the status table is the blocking dependency of layer §13.1.
3. **Every page change is a mini-transaction** (layer A1, A10): the engine builds records; the seam applies them through `cascade-redo` to frames the engine pins exclusively; a page's LSN is the end of its last record (layer §9.3). No other path changes a page.
4. **A HOT chain never crosses a page** (heap §4.2).
5. **Tids are stable.** A slot is reused only once it is unused, which vacuum makes it once the row's death is older than the cleanup watermark (heap §7.3).
6. **Prune verdicts are computed by the engine, under the exclusive pin, and carried by the record.** Replay never re-derives them (page D2; heap H11). An in-progress stamp is never prunable.
7. **A commit is durable before it is acknowledged.** The engine's groups ride that path and have no durability mechanism of their own.
8. **Placement follows the record format** (heap H9, accepted 2026-11-26). While `HEAP_INSERT` (type 5) names only a segment, this engine places every insert by `cascade-redo`'s sequential-fill rule, advancing the fill cursor in LSN order. No free-space placement, slot reuse or prune before the switch to type 10. A cell the page format cannot place is refused before it is logged.
9. **Row locks are recorded** (heap §4.4): an exclusive lock in the row's deleter word with `XMAX_LOCK_ONLY`; a shared lock in the lock manager. `abort_speculative` stamps the deleter word and the index entry's delete word (heap §4.5).
10. **The chain walk** (heap H10): `Deleted` hops to the successor; `NotBornYet` is "no row".

## Out of Scope

- Index maintenance — `cascade-index`; this crate calls it inside its writes (heap §4.6).
- The chunk format of out-of-line storage — decided when it lands with `cascade-index` (heap §8).
- Columnar propagation — `cascade-columnar` reads the log.
- Visibility policy — `Snapshot::row_visible` with the injected `VisibilitySource` (layer §6.6, A8). This crate stamps words and walks chains; it invents no rule.

## Existing Code

Stub (`src/lib.rs`: `Tid`, `SlotId`, a placeholder `Heap`) until Step 3 of the layer's migration (layer §12), which lands the first insert-and-scan slice. The crate description and its `cascade-mvcc`/`cascade-undo` dependencies in `Cargo.toml` describe a replaced design and are corrected at Step 5.

## Open Design Questions

Owned by [`heap-design.md`](heap-design.md) §12: how version words become self-describing (the transaction layer; blocks Step 4); the two row-lock flag bits (the page layer); the free-space map's page layout (H5, H12); the chunk format of out-of-line storage (H7, Step 8); how prune is scheduled in the background.
