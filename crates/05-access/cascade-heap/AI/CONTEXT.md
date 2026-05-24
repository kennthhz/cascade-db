# cascade-heap — AI Design Context

## Role

Heap (row-store) access method.

## Pillar Reservations

- **§3.3:** in-place updates with undo references. This is the crate that *embodies* the Anti-VACUUM design.
- **§6.3 (CDC):** every mutation must emit a WAL record sufficient for logical decoding — full before/after images on update, before-image on delete.
- **§3.7 (HTAP):** writes that should propagate to a columnar projection emit a hook event for `cascade-columnar` to pick up.

## Hard Invariants

1. **In-place updates.** No tuple version chains on the row page.
2. **Undo write before row mutation** — if undo write fails, the row mutation never happens.
3. **WAL before commit** — the txn cannot commit until its WAL is durable.
4. **Tid stability** — `(PageId, SlotId)` is the addressable row identity; mutations never move a row to a new Tid unless explicitly part of a split.
5. **Row header carries xmin / xmax / undo-ref**, format defined by `cascade-page`.

## Out of Scope

- Index maintenance — separate from this crate; secondary indexes coordinate via the txn layer.
- Toast / oversized rows — Phase 3 follow-up; design must reserve room.
- Columnar segment propagation — `cascade-columnar` subscribes to mutation events.

## Open Design Questions

- Row migration on update: if the new version doesn't fit on the page, do we move it (PG HOT-style) or chain?
- HOT-update equivalent: in-place updates that don't change any indexed columns avoid touching secondary indexes — worth implementing in v1?
- TOAST: extract the oversized-row mechanism into its own crate or keep here?
