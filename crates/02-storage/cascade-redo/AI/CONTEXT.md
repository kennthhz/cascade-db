# cascade-redo — crate context

## What this crate is

The redo contract (storage-layer-design.md §12.4): the apply functions
shared by leaders, followers and page stores. One function — `apply_record`
— plus one routing law — `SegmentFill`. M7 scope: the **heap-insert family**
only (WAL type 5, `HeapInsert`).

## The four laws (§12.4, one-way door)

1. Self-contained per page: only the page image and the record.
2. Deterministic: same image + record → same bytes.
3. Versioned: type bytes are cascade-wal-format's frozen table; the page
   format is cascade-page's locked layout.
4. Idempotent by LSN (S17): applies only when `start >= page_lsn`, where
   the page LSN is the END of the last applied record. The skip happens
   *before* payload validation — an already-applied record cannot hurt the
   page whatever its bytes now say.

## The routing law: sequential fill

`HeapInsert` names a **segment**, not a page (the type-5 payload is frozen,
`[segment u32][cell verbatim]` — no page number can be retrofitted). So
routing is fixed here: a segment's inserts fill page 0, 1, 2, … by
cascade-page's exact append arithmetic, spilling whole cells.

**The rule binds the heap access method** (heap design H9, decided
2026-11-26): the heap places every insert where the fill cursor says and
advances the cursor in LSN order, so the leader's live layout *is* the
replayed layout. The page store serves an evicted page back to the leader
in replayed layout, so a leader that laid rows out differently would find
them moved.

`SegmentFill::route` is the single authority, with one companion for the
page store; the two must not be confused:

- `route(&mut self, cell_len)` — *where* the next cell goes, and the
  cursor moves there: past full pages, then the cell is appended (one more
  slot, `free_offset` down by the cell's bytes). For a caller that keeps a
  cursor across records — the heap engine, once per applied insert.
  Afterwards the cursor equals that page's header.
- `would_route(&self, cell_len)` — the same answer, and the cursor does
  not move. For a caller that holds no cursor and derives one from the
  page it is about to append to: the page store, for which the open page's
  header on disk *is* the state.

A driver that keeps a cursor but only ever calls `would_route` sends every
cell to the same page, because the cursor never learns the page is
filling. Routing is a pure function of the insert sequence, which is why
recovery can resume from a materialized page's header (`from_page_header`)
and replay-from-empty reproduces pages byte for byte (the §10 differential
law).

## The order inside `apply_record`

1. The record's family. A record that touches no page is `NotAPageRecord`
   whatever its LSN, and nothing of it is examined.
2. For a heap insert, the S17 check, before anything is read from the
   payload.
3. The cell's structure (`parse_cell`), then the size ceiling.
4. **The page is validated before its header is reasoned about.** An
   initialized page goes through `PageView::parse` (header bounds and every
   cell); a never-written page is an empty one — its zeroed header must not
   be read as "no free space". Without this order a damaged page's
   impossible header reads as a full page, and a first insert into any
   segment is refused as a spill.
5. The space check, which mirrors `PageMut::append_cell` exactly, the
   cell's 2-byte slot entry included.
6. The append, through `PageMut::wrap` for an existing page.

## Refusals that are laws

- `Unplaceable`: cell larger than `MAX_CELL_LEN` (`PAGE_SIZE - HEADER_LEN - 2`,
  8158 — the cell's own 2-byte slot entry counts). The WAL frame's u16
  ceiling is wider, so the producer must gate — this is the loud backstop.
- `UnsupportedRecord`: `LegacyInsert` (type 4) — the pre-T1.3 row image
  predates the page format; page-level replay lands with cascade-heap's row
  codec. The runtime's own recovery keeps handling legacy records for the
  debug substrate.
- `CorruptCell` / `Page`: only a buggy encoder or a corrupt image gets
  here; both refuse instead of guessing.

`NotAPageRecord` covers BEGIN/COMMIT/ABORT and the reserved families —
inert to pages; ingest still advances their LSN. A page's LSN is therefore
the *insert's* end even when a COMMIT closes its group.

## Test law worth knowing

`fill_cursor_routing_matches_naive_replay` is the differential kernel:
`route` + apply must equal naive replay, page for page, over 2000
spill-straddling random cells — and the cursor's final state must equal the
open page's header. `resumed_cursor_continues_an_uninterrupted_replay`
pins the recovery/resume foundation.
`would_route_only_answers_and_route_advances` pins the difference between
the two operations. All three are deterministic (xorshift).

## Where it is used

`cascade-pagestore` (M7) wraps this crate in `HeapInsertRedo`: it asks the
store for the segment's highest page, reads that page as a fill cursor with
`from_page_header`, asks `would_route`, and lets `apply_record` do the
append — it keeps no cursor of its own.
`RedoError` is mapped there to the storage contract's `StoreError`
(`Corruption` at the record's LSN or naming the page; `Unsupported` for
`LegacyInsert`, which the page store refuses rather than skips).

## Known mirror to close

The access-layer design (A2, `crates/05-access/AI/access-layer-design.md`)
says the page format owns the free-space predicate and that redo should
ask it. This crate still *mirrors* that arithmetic (`fits`,
`MAX_CELL_LEN`) — which is how the fit-rule defect of 2026-11-26 reached
both crates at once. One public predicate in `cascade-page` closes it.

## Not here (yet)

Delete/update/undo/btree redo families; the checkpoint-free rule's other
consumers; page-level LegacyInsert replay. New record *types* extend —
never renumber (wal-format F3).
