# cascade-btree — AI Design Context

## Role

The B+tree **mechanism** shared by the IOT primary tree (`cascade-iot`) and secondary index trees (`cascade-index`): descent, OLC latching, splits, ordered iteration, byte-level entry operations over byte keys and byte values. It knows nothing about transactions or visibility (architecture §4.6 verbatim) — version words in leaf payloads are opaque bytes here.

**Authoritative design:** [`AI/btree-design.md`](btree-design.md) (B1–B7). Layer contracts: [`../../AI/access-layer-design.md`](../../AI/access-layer-design.md) (A2 redo ownership, A3 page-format home, A4 execution model, A9 this crate's scope); storage foundations in [`../../../02-storage/AI/storage-layer-design.md`](../../../02-storage/AI/storage-layer-design.md) §12.4–§12.5.

## Pillar Reservations

- **Storage foundations (storage-layer-design §12) are the source of truth:** one writer per scope (§12.2), the redo contract (§12.4), page cache durability rules (§12.5).
- **A3 (2026-11-26):** the btree *page formats* — branch, IOT leaf, index leaf kinds — live in **`cascade-page`** under the prologue registry (the architecture's layer map puts every page layout there, and `cascade-redo` cannot depend on an L5 crate). This crate owns the tree *logic* over `cascade-page`'s typed views, not bytes. This amends the pre-A3 CONTEXT invariant that hosted the kinds' bytes here.
- **§6.3 (CDC):** index change records are part of the log's full-image story; entry payloads are `cascade-wal-format`'s once the family lands.

## Hard Invariants

1. **Shared-memory engine over the write core set** (layer design A4 — supersedes the retired owner-core mutator rule): every structure this crate touches per scope is `Send + Sync`; page-level exclusion is the BPM pin protocol; concurrent writers to one node serialize on the frame; the multi-pin order (parent → left → right) is the only one in the crate.
2. **No node visible to readers until fully constructed — a split is one mini-transaction** (B5): every record of an SMO is one group ending on the group-end flag, so no durable LSN or replay boundary ever lands inside a split (storage §4.3 promise 5).
3. **Optimistic descent (B3):** no pin is held across an await, across a level, or across a version check; validated descents retry on `mod_count` change. Misrouting and stale-epoch calls are debug asserts, never runtime paths.
4. **The redo binding applies to every record** — self-contained per page, deterministic, versioned, idempotent by LSN; the apply functions live in `cascade-redo` over `cascade-page`'s views (A2), the payload encodings in `cascade-wal-format` (Rule 5).
5. **Prune/purge verdicts are engine-computed, never replayed** — the same law as the heap's prune (page design D2): deadness needs the status table, which replay lacks.

## Out of Scope

- Visibility, unique checks, speculative arbitration — `cascade-iot` / `cascade-index` decode entries and call `row_visible`; this crate only finds entries.
- Undo (`roll_ptr` is payload bytes) and purge policy — `cascade-iot` (WP6).
- Other index kinds (hash, GIN, GiST, BRIN, vector) — sibling crates behind `cascade-index`'s seam; never a tagged enum inside this crate.
- Locking above the index (row locks live in the txn layer).
- Bulk load — point operations in v1; bulk load is a later phase (record types reserved, WP1 F3).

## Existing Code

Stub (`src/lib.rs`: doc comment only) until the tree lands at layer-design migration step L6. Prerequisite: `cascade-page`'s frame/heap module split (page design L3) — this crate consumes the layer contract, never a forked prologue. The OLC latch protocol here *is* the resolution of the BPM CONTEXT's "the OLC latch lands with the btree": a usage protocol over the landed pin word, not a new latch type (fused in-frame latch is a measured optimization behind the same interface).

## Open Design Questions

Owned by [`btree-design.md` §10](btree-design.md): metapage as fourth kind vs flagged branch (landing review — one-way door); merge/rebalance (deferred, B6, revisit on deletion-heavy benchmarks); leaf prefix compression (rejected for v1) and separator suffix truncation (minor-bump candidate); fused in-frame OLC latch (if profiled); `BT_SPLIT` record size vs delta SMO records (page-store ingest profiling only). Resolved and closed there: prefix compression v1 (no); hash variant home (sibling crate).
