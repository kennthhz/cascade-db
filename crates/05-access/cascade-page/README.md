# cascade-page

8 KB page layout — headers, slot arrays, checksums, `PageLSN`.

## Role

Layer **05-access**. The definitive home for the on-disk page format. Every access method (heap, B+Tree, undo, columnar) reads / writes pages through this crate's primitives.

## Public API (initial)

- `PageHeader` — 32-byte (proposed) fixed header: page type, free-space pointer, PageLSN, CRC32, special area offset.
- `SlotArray` — variable-length slot directory at the page tail.
- `PageView` / `PageMut` — typed views over a raw `&[u8; 8192]`.

## Dependencies

- `cascade-types` — `Lsn`, `PageId`.
- `cascade-error`.
- `crc32fast`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.3.
