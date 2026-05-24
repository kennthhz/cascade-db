# cascade-wal

Per-database Write-Ahead Log.

## Role

Layer **02-storage**. Implements pillar §3.4 (Per-Database WAL & LSN). Hosts the §6.3 (logical CDC) reservation in the WAL record format.

## Why a separate crate from `cascade-storage`

WAL has different access patterns (append-only, fsync-heavy) and may eventually live on a different physical tier than data pages (§6.4 — fast NVMe WAL, object-storage data). Keeping `WalStore` separate from `PageStore` makes that pluggability cheap.

## Public API (initial)

- `trait WalStore` — `append`, `flush`, `truncate`, `read_records`.
- WAL record format — versioned, CRC'd, carries enough state for logical decoding (full row images on update where decoders need them).
- `trait WalReader` — abstract WAL consumer. Physical recovery is one consumer; logical CDC (§6.3) will be another.

## Dependencies

- `cascade-types`, `cascade-error`, `cascade-telemetry`.

## v1 reservations honored

- **§6.3:** record format includes full column values on `INSERT`/`UPDATE`/`DELETE` where logical decoders need them. DDL changes are themselves logged so decoders can interpret historical row formats.
- **§6.4:** `WalStore` is a separate trait from `PageStore` so it can move to a different backend.
- **§3.4:** each `DatabaseId` has its own WAL file series and `Lsn` sequence.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.4, §6.3.
