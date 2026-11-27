# cascade-btree — AI Design Context

## Role

The B+tree **mechanism** shared by the IOT primary tree (`cascade-iot`) and secondary index trees (`cascade-index`): descent, latching, structure changes, ordered iteration, and operations on byte keys. It knows nothing about transactions or visibility (architecture §4.6): version words inside leaf payloads are opaque bytes here.

**Authoritative design:** [`AI/btree-design.md`](btree-design.md) (decisions B1–B11). Layer contracts: [`../../AI/access-layer-design.md`](../../AI/access-layer-design.md) — A2 (redo ownership), A3 (where the page formats live), A4 (execution model), A9 (this crate's scope), §9 (the write path). Storage foundations: `crates/02-storage/AI/storage-layer-design.md` §12.4–§12.5.

## Pillar Reservations

- **Storage foundations (storage design §12) are the source of truth:** one writer per scope (§12.2), the redo contract (§12.4), page cache durability rules (§12.5).
- **A3:** the B-tree *page formats* — branch, IOT leaf, index leaf, metapage (kinds 2–5) — live in **`cascade-page`** under the prologue registry. The architecture's layer map puts every page layout there, and `cascade-redo` (L1) cannot depend on an L5 crate. This crate owns the tree *logic* over `cascade-page`'s typed views, not bytes.
- **Change data capture:** index change records are part of the log; entry payloads are `cascade-wal-format`'s once the family lands.

## Hard Invariants

1. **Shared-memory engine over the write core set** (layer A4). Every structure this crate touches per scope is `Send + Sync`. Page-level exclusion is the buffer pool's pin protocol. Parent → left → right is the only multi-pin order in the crate (btree §3.4).
2. **A split is one mini-transaction** (B5): every record of a structure change is one group ending on the group-end flag, so no durable LSN or replay boundary falls inside a split.
3. **Optimistic descent** (B3, B4): readers copy, then validate; writers pin, then validate; the root pointer is checked on every lookup; no pin is held across an `await` or a tree level. A call from a core outside the write core set is a debug assertion; a stale epoch is the runtime outcome `Fenced` (layer §6.7).
4. **The redo binding applies to every record:** self-contained per page, deterministic, versioned, idempotent by LSN. The apply functions live in `cascade-redo` over `cascade-page`'s views (A2); the payload encodings in `cascade-wal-format`. A record leaves each node with its own end as the page LSN (layer §9.3) and increments each changed node's `mod_count` by one (B10).
5. **Purge verdicts are computed by the host, never by replay** — deadness needs the status table, which replay lacks (btree §7; the same law as the heap's prune, page D2).
6. **The tree is transaction-blind.** It answers "which entries are at this tree key" as a run, oldest first; which of them is visible is the host's decision (B8).

## Out of Scope

- Visibility, unique checks, speculative arbitration — `cascade-iot` and `cascade-index` decode entries and call `row_visible`.
- Undo (`roll_ptr` is payload) and purge policy — `cascade-iot`.
- Other index kinds (hash, GIN, GiST, BRIN, vector) — behind `cascade-index`'s seam; never a variant inside this crate.
- Merge and root collapse — not in version 1 (B6, btree §5.5).
- Bulk load — point operations only in version 1.

## Existing Code

Stub (`src/lib.rs`: doc comment only) until Step 6 of the layer's migration (layer §12). Prerequisite: `cascade-page`'s frame/heap module split (page L3). The latch protocol of btree §3 is the answer to the buffer pool context's "the OLC latch lands with the btree": a usage protocol over the landed pin word, not a new latch type.

## Open Design Questions

Owned by [`btree-design.md`](btree-design.md) §10: the metapage's layout (frozen with its golden test); suffix truncation and prefix compression (later, as minor versions); a latch fused into the frame (if measured); a hot rightmost leaf (acknowledged, no mitigation designed); the size of the split record (revisited on page-store ingest profiling).
