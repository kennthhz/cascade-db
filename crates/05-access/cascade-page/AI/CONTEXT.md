# cascade-page — AI Design Context

## Role

Page format authority. Defines the 8 KB byte layout that every access method shares. **The format is implemented (plan T6.1, FR-39): `src/lib.rs` is the one legal reader/writer of the bytes — no `#[repr(C)]` header struct, the byte offsets below ARE the spec.**

## The format (locked at version 1, 2026-11-08)

```text
offset  0: format_version u16   (1)
offset  2: page_kind       u16   (1 = heap)
offset  4: slot_count      u16
offset  6: free_offset     u16   cells occupy [free_offset, PAGE_SIZE)
offset  8: page_lsn        u64   reserved ZERO in M6 — the BPM frame stamp is
                                 the authoritative writeback boundary while
                                 nothing replays from page images; the field
                                 is format-complete so replay-from-pages (the
                                 FR-50 milestone) needs no format change.
offset 16: reserved 16 bytes (zero)
offset 32: slot array — slot_count × u16 LE cell offsets, growing UP
tail:     cells growing DOWN from PAGE_SIZE
```

A cell is `[len u16][xmin u64][xmax u64][undo_ref u64][payload..]`; `len` counts
the whole cell (`CELL_FIXED = 26` + payload). A cell fits iff
`free_offset − len ≥ 32 + 2·slot_count` (the two growth regions must never
touch). `xmax`/`undo_ref` are reserved zero until the MVCC/undo milestone.
`payload` is opaque here — heap row encoding is `cascade-heap`'s.

## Pillar Reservations

- **§3.3 (In-place MVCC):** row headers (xmin, xmax, undo pointer) live on the page; this crate defines their offsets. The cell layout above is the reservation: `xmin` is stamped at insert, `xmax`/`undo_ref` wait for the visibility milestone.
- **§6.3 (CDC):** rows must carry enough header state for logical decoders to interpret historical row formats. Schema version stays undecided (below).

## Hard Invariants

1. **CRC32 over the page — OWNED BY `cascade-storage`, not this crate.** The stored page image is `[u32 LE CRC32 of the rest][payload]`, recomputed on write and validated on read by the storage layer. This crate deliberately carries no checksum field; nesting two CRCs over the same bytes would double validation cost and blur which layer answers for integrity. (The original stub sketched a header `crc32` field — deleted at T6.1 when the division was settled.)
2. **PageLSN is the high-water mark** of WAL records applied to this page. Used by WAL-before-data enforcement (`cascade-bpm`). In M6 the on-page field stays zero and the BPM frame stamp (`AtomicU64`) is authoritative; the field exists so later milestones need no format change.
3. **Endianness fixed** at format-definition time. Little-endian (matches x86_64, ARM64). Golden byte-literal tests in `src/lib.rs` pin every header and cell byte.
4. **No pointers into the page from outside.** Access methods reconstruct row references from `(PageId, SlotId)`, never raw addresses. `CellRef::payload` borrows the page image only.
5. **Format is versioned.** Header carries a format version (`FORMAT_VERSION = 1`); `parse` refuses other versions and `is_initialized` (nonzero version bytes) distinguishes a never-written page — a normal empty page, not corruption.
6. **Validation is total at the boundary.** `PageView::parse` checks the header AND every cell (slot in cell region, cell length sane, no tail overrun); a corrupt image is refused once at parse, never mid-scan. `PageMut::wrap` validates through the same read path — the mutation path cannot bypass the rules.

## Out of Scope

- Index-specific node layout (B+Tree internals live in `cascade-btree`).
- Heap-specific row encoding (lives in `cascade-heap`).
- Columnar segment format (lives in `cascade-columnar` — different format, different crate, same 8 KB unit).
- The outer integrity CRC (lives in `cascade-storage`, see invariant 1).

## Existing Code (T6.1)

`src/lib.rs`: constants (`FORMAT_VERSION`, `PAGE_KIND_HEAP`, `HEADER_LEN = 32`,
`SLOTS_BASE = 32`, `CELL_FIXED = 26`, compile-time format locks),
`PageError` (Uninitialized / WrongVersion / WrongKind / CellTooLarge / Full /
Malformed), `is_initialized`, `PageView` (parse / cell / iter / free_bytes /
page_lsn), `PageMut` (init / wrap / append_cell / set_page_lsn / free_bytes /
view). Append-only pages — no delete/compact yet (the MVCC milestone owns
that). Six tests: golden byte literals (header + slots + two cells, whole
bytes), roundtrip readback, exact full-boundary (291 zero-payload cells, 12
free bytes left, the 292nd append refused and the page stays consistent),
oversized-cell refusal, the malformed-image matrix (each `PageError` variant
hand-mangled and refused), wrap-validates-like-parse.

## Open Design Questions

- ~~Header size: 32 bytes is generous; could squeeze to 24.~~ → **Resolved 2026-11-08 (T6.1):** 32 bytes, locked — the extra 16 reserved bytes buy page_lsn plus a versioned growth margin without a format bump; squeezing to 24 saves 0.1% of the page.
- Where to put schema version: row header vs. page header. Page-level is cheaper; row-level allows mid-page schema mixing during online ALTER TABLE. Still open — deferred with the ALTER TABLE era (§6.3 CDC needs the answer before logical decoding ships).
