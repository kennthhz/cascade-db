# cascade-heap — AI Design Context

## Role

The heap table engine — **the compatibility engine**: append-only row versions, HOT chains, incremental prune + vacuum (HLD v2 §12). The `AnyTable::Heap` arm. The default engine is the index-organized one (`cascade-iot`); this crate exists for tables without a primary key and for applications that depend on heap behavior (product definition §4.1).

**Authoritative design:** [`AI/heap-design.md`](heap-design.md) (engine semantics — this crate) and [`cascade-page/AI/storage-page-mutation-design.md`](../../cascade-page/AI/storage-page-mutation-design.md) D1–D6 (the kind-1 format: slot states, prune plans, redo primitives — locked, this crate obeys it). Layer contracts: [`access-layer-design.md`](../../AI/access-layer-design.md) (the `AnyTable` surface, execution model, mini-txn seam, visibility seam).

## Pillar Reservations

- **§4.1 (compatibility engine):** every update writes a new version; cleanup is continuous and incremental (prune on access, vacuum one page at a time) — operators never run `VACUUM FULL` and there is no freeze. The Anti-VACUUM in-place design is `cascade-iot`'s, not this crate's (pre-pivot CONTEXTs said otherwise; they were rewritten 2026-11-26).
- **§6.3 (CDC):** every mutation emits full row images — inserts carry the cell verbatim (WP1 F3's cascade-page seam), updates carry old + new, deletes carry the before-image via the deleter stamp + undo-free design.
- **§3.7 (HTAP):** writes that should propagate to a columnar projection emit a hook event for `cascade-columnar` to pick up (reserved; the log is the transport, never a direct call).

## Hard Invariants

1. **Append-only versions.** No in-place row overwrite, ever. The old version stays readable in place; the new version is a new tuple.
2. **No undo.** Rollback is the abort record plus status-table invisibility (WP2 D5); aborted tuples are prune/vacuum food. `cascade-undo` is *not* a dependency of this crate (pre-pivot `Cargo.toml` deps are dropped at step L3).
3. **Every page change is a mini-transaction group** (layer design A1): reserve the record group (S16 group-end) → apply through the format's redo primitives to the pinned frame → stamp page LSN with the group end (S17). No direct-BPM-write path exists above the seam.
4. **A HOT chain never crosses a page.** The update path chooses same-page HOT only when the new version fits and no indexed column changed (heap design §4.2); `HEAP_ONLY` is stamped iff same-page.
5. **Tid stability.** `(page_no, slot)` never renumbers, never shrinks; slot reuse only via Unused, lowest-first, only after vacuum's index cleanup (page D1's transition table; heap design H6's ordering is structural).
6. **Prune eligibility is engine-computed, replay-applied:** verdicts (never watermarks) ride in the HEAP_PRUNE record; uncommitted stamps are never eligible (page D2).
7. **WAL before commit** — a txn is committed iff its commit record is durable (WP2 invariant 2); the engine's mini-txn groups ride that path, never a separate durability mechanism.
8. **Placement follows the record format.** While `HEAP_INSERT` (type 5) names only a segment, replay derives the page from `cascade-redo`'s sequential-fill rule, so this engine places inserts by that rule and by no other (no FSM, no slot reuse, no prune before the `HEAP_INSERT_V2` switch), advancing the fill cursor in LSN order so that live layout equals replayed layout (heap design H9). A cell the page format cannot place is refused before it is logged (heap design §9).

## Out of Scope

- Index maintenance — `cascade-index`; this crate calls it inside writes (WP5 D5) and vacuum (H6's ordering).
- TOAST chunk format/compression — reserved pointer format only (heap design §7); lands with `cascade-index` (L8).
- Columnar propagation — `cascade-columnar` subscribes; no direct dependency.
- Visibility policy — `Snapshot::row_visible` + injected `VisibilitySource` (layer design A8); this crate stamps words and walks chains, it never invents a visibility rule.

## Existing Code

Stub (`src/lib.rs`: `Tid`, `SlotId`, placeholder `Heap`) until T6.4 — the M6 interim slice (insert/scan through the BPM, xmin only, no MVCC) lands against the `cascade-access-api` contract (layer design §10, steps L0–L2). The pre-pivot `Cargo.toml` description ("in-place MVCC rows, undo references") and its `cascade-mvcc`/`cascade-undo` deps are stale — fixed at L3.

## Open Design Questions

Owned by [`heap-design.md` §12](heap-design.md) — the live ones: version-word stamping / status-retention horizon (L4 txn pass, engine-neutral via the A8 seam); FSM kind-6 freeze (H5); TOAST chunk format freeze (H7, at L8); prune background cadence (tuned with `cascade-maintenance`). Resolved by the pivot and closed there: row migration on update (append-only has none); "HOT-equivalent in v1?" (it *is* the design).
