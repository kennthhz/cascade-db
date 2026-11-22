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

A cell is `[len u16][xmin u64][xmax u64][next u64][payload..]`; `len` counts
the whole cell (`CELL_FIXED = 26` + payload). A cell fits iff
`free_offset − len ≥ 32 + 2·slot_count` (the two growth regions must never
touch). `xmin`/`xmax` are **typed version words** (`cascade_types::VersionWord`,
WP1 F1 — upstream HLD §12's creator/deleter; top bit = uncommitted tag over a
63-bit `Xid`, else the packed `Hlc` commit_ts; 0 = EMPTY). `next` is the packed
chain word (`NextWord`: bits 0..15 slot, 16..47 relation-local `PageNo`,
48..63 flags `HAS_NEXT`/`HEAP_ONLY`/`HAS_TOAST`; `HAS_NEXT` clear ⇒ chain
ends; "HOT-updated root" is derived as `HAS_NEXT && same page`) — the v1
`undo_ref` reservation repurposed at WP1 F2 without moving a byte (the heap
has no undo; undo chains are IOT, §11). Fresh cells stamp `xmax`/`next` zero.
`payload` is opaque here — heap row encoding is `cascade-heap`'s.

## Pillar Reservations

- **HLD v2 §6/§12 (version words + heap tuple header; superseded the old §3.3 mapping):** the row header lives on the page and this crate defines its offsets: `xmin` is stamped at insert via `append_cell(VersionWord::uncommitted(Xid), …)`, `xmax`/`next` are stamped by `set_xmax`/`set_next` at the §12 delete/update steps (format level only — engine semantics are the heap engine's, T3.x).
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

## Existing Code (T6.1, amended T1.1)

**T1.1 (WP1 F1/F2, 2026-11-14):** `xmin`/`xmax` are `VersionWord`s and the
reserved `undo_ref` became `NextWord` (`chain(page_no, slot, flags)` forces
`HAS_NEXT`; flag consts `NEXT_HAS_NEXT`/`NEXT_HEAP_ONLY`/`NEXT_HAS_TOAST`);
`append_cell` takes the typed creator word; `set_xmax(slot, VersionWord)` and
`set_next(slot, NextWord)` mutate cells in place (out-of-range slot = panic).
`CELL_FIXED` stays 26; `FORMAT_VERSION` stays 1 (no persisted readers exist;
the evolution is documented in crates/02-storage/AI/storage-layer-design.md (§12.7 WP table)). Parse remains
structural — version-word semantics are the visibility layer's, never the
page parser's. Tests grew: version-word bit matrix (cascade-types), chain
word layout literal (`0x0003_0102_0304_0506` for `chain(0x01020304, 0x0506,
NEXT_HEAP_ONLY)`), stamp read-back, out-of-range `set_xmax` refusal; the
goldens moved exactly one byte per cell (the creator word's tag MSB).

`src/lib.rs`: constants (`FORMAT_VERSION`, `PAGE_KIND_HEAP`, `HEADER_LEN = 32`,
`SLOTS_BASE = 32`, `CELL_FIXED = 26`, compile-time format locks),
`PageError` (Uninitialized / WrongVersion / WrongKind / CellTooLarge / Full /
Malformed), `is_initialized`, `PageView` (parse / cell / iter / free_bytes /
page_lsn), `PageMut` (init / wrap / append_cell / set_page_lsn / free_bytes /
view), and the standalone-cell half of the format since T1.3: `encode_cell`
/ `parse_cell` — byte-identical to what `append_cell` lays down (pinned by
test); every WAL producer stamps cells through them (the HEAP_INSERT
"verbatim" seam, WP1 F3), never by hand. Append-only pages — no
delete/compact yet (the MVCC milestone owns
that). Six tests: golden byte literals (header + slots + two cells, whole
bytes), roundtrip readback, exact full-boundary (291 zero-payload cells, 12
free bytes left, the 292nd append refused and the page stays consistent),
oversized-cell refusal, the malformed-image matrix (each `PageError` variant
hand-mangled and refused), wrap-validates-like-parse.

## Open Design Questions

- ~~Header size: 32 bytes is generous; could squeeze to 24.~~ → **Resolved 2026-11-08 (T6.1):** 32 bytes, locked — the extra 16 reserved bytes buy page_lsn plus a versioned growth margin without a format bump; squeezing to 24 saves 0.1% of the page.
- Where to put schema version: row header vs. page header. Page-level is cheaper; row-level allows mid-page schema mixing during online ALTER TABLE. Still open — deferred with the ALTER TABLE era (§6.3 CDC needs the answer before logical decoding ships).
