# cascade-storage

Random-access 8 KB page I/O for Cascade DB. Owns the `PageStore` trait and the local `O_DIRECT` + `io_uring` implementation.

## Role

Layer **02-storage**. Implements pillar §3.1 (kernel-bypass storage I/O). Hosts the §6.4 (disaggregated storage) trait reservation.

## Public API (initial)

- `trait PageStore` — random I/O over 8 KB pages, single and vectored.
- `struct CoreStorage` — thread-per-core, `!Send`, `!Sync` implementation backed by `tokio-uring`. Used by the BPM via the trait.
- `struct StorageManager` — boots the storage subsystem, scans the data directory, hands out a `CoreStorage` per worker thread.
- Local error type `StorageError` (converted into `cascade_error::Error` at the boundary).

## Dependencies

- `cascade-types` — `PageId`, `Lsn`, `AlignedBuf`.
- `cascade-error` — top-level error type.
- `cascade-telemetry` — `Span`, metrics.
- `tokio-uring`, `libc`, `crc32fast`.

## v1 reservations honored

- All page I/O flows through the `PageStore` trait — no `RawFd` / `File` escapes this crate (§6.4).
- `PageId` is `(DatabaseId, SegmentId, PageNo)` (§6.4).
- Read/write methods accept `&Span` for tracing (§6.2).
- I/O submissions carry a tenant/database tag (§6.1).

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.1, §6.4.
