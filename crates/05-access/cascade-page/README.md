# cascade-page

8 KB page layout — headers, slot arrays, cells, `PageLSN`.

## Role

Layer **05-access**. The definitive home for the on-disk page format. Every access method (heap, B+Tree, undo, columnar) reads / writes pages through this crate's primitives. The byte layout is the spec — see [AI/CONTEXT.md](AI/CONTEXT.md) for the offset table.

## Public API (T6.1, format version 1)

- `PageView` — validated read-only view over a raw `&[u8; 8192]`: `parse` (total validation, header + every cell), `cell(slot)`, `iter()`, `slot_count()`, `free_bytes()`, `page_lsn()`.
- `PageMut` — writable view: `init` (fresh page), `wrap` (admit an existing image through the same validation), `append_cell(xmin, payload) -> slot` (cells grow down, slots up, `Full` when the regions touch), `set_page_lsn`.
- `CellRef` — one decoded cell: `xmin` / `xmax` / `undo_ref` (reserved zero until MVCC) + opaque `payload`.
- `is_initialized` — nonzero version bytes: a never-written page is empty, not corrupt.
- `PageError` — `Uninitialized` / `WrongVersion` / `WrongKind` / `CellTooLarge` / `Full` / `Malformed`.

Integrity note: the outer CRC32 lives in `cascade-storage` (each stored image is `[u32 LE CRC][page]`); this crate carries no checksum field — one CRC, one owner.

## Dependencies

- `cascade-types` — `PAGE_SIZE`.
- `thiserror` — the local `PageError`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.3.
