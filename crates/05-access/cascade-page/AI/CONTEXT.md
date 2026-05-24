# cascade-page — AI Design Context

## Role

Page format authority. Defines the 8 KB byte layout that every access method shares.

## Pillar Reservations

- **§3.3 (In-place MVCC):** row headers (xmin, xmax, undo pointer) live on the page; this crate defines their offsets.
- **§6.3 (CDC):** rows must carry enough header state for logical decoders to interpret historical row formats. Schema version in the header is worth considering.

## Hard Invariants

1. **CRC32 over the page** — recomputed on every write, validated on every read.
2. **PageLSN is the high-water mark** of WAL records applied to this page. Used by WAL-before-data enforcement (`cascade-bpm`).
3. **Endianness fixed** at format-definition time. Little-endian (matches x86_64, ARM64).
4. **No pointers into the page from outside.** Access methods reconstruct row references from `(PageId, SlotId)`, never raw addresses.
5. **Format is versioned.** Header carries a format version; new versions must remain readable by old code for as long as we promise on-disk compatibility.

## Out of Scope

- Index-specific node layout (B+Tree internals live in `cascade-btree`).
- Heap-specific row encoding (lives in `cascade-heap`).
- Columnar segment format (lives in `cascade-columnar` — different format, different crate, same 8 KB unit).

## Open Design Questions

- Header size: 32 bytes is generous; could squeeze to 24. Wait until row format is settled.
- Where to put schema version: row header vs. page header. Page-level is cheaper; row-level allows mid-page schema mixing during online ALTER TABLE.
