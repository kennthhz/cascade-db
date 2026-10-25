# cascade-storage — AI Design Context

## Role

The bottom of the I/O stack. Owns the `PageStore` trait + a local `O_DIRECT` / `io_uring` implementation.

## Pillar Reservations

- **§3.1:** kernel-bypass I/O. `O_DIRECT` is mandatory for data pages; the Linux page cache is bypassed.
- **§6.4:** the `PageStore` trait is the disaggregated-storage hook. Future backends (S3, page service) implement the same trait without changing callers.
- **§6.1:** every I/O carries a tenant/db tag so future quotas can throttle.
- **§6.2:** read/write methods accept `&Span`.

## Hard Invariants

1. **No raw FDs escape.** No `RawFd` / `File` may leave this crate. Callers receive only `PageStore` trait objects (or static-dispatched generics).
2. **`O_DIRECT` for data pages.** Always. The buffer pool is the only cache.
3. **`PageId` is backend-agnostic.** Never compute a file path or offset above this layer. Mapping `PageId → (file, offset)` happens *inside* this crate.
4. **Vectored I/O paths exist from day one** (`read_pages`, `write_pages`), even if naively implemented — the trait shape must support them.
5. **CRC32 on every page.** Written on flush, validated on read.
6. **`CoreStorage` is `!Send + !Sync`.** It is owned by exactly one worker thread; sharing it across cores defeats §3.2.

## Out of Scope

- WAL I/O — that lives in [`cascade-wal`](../../cascade-wal). Separate trait, separate impl, may live on different storage tiers (§6.4).
- Buffer pool memory management — that's [`cascade-bpm`](../../../03-memory/cascade-bpm).
- Page *layout* (headers, slot arrays) — that's [`cascade-page`](../../../05-access/cascade-page).

## Existing Code

Migrated from `storage/src/{core_storage.rs,traits.rs}` (Phase 1 sketch). As of T1.9 the glommio implementation is real for `read_page`/`write_page` (file-per-table `<data_dir>/db_<db>/space_<seg>.dat`, page = `[CRC32][payload]` inside 8 KiB, past-EOF reads are zeroes); `read_pages`/`write_pages`/`allocate_extent`/`free_extent` keep the trait shape with `todo!()` bodies until the scan/bulk/space milestones.

## Resolved Design Questions

- Async runtime: **glommio** (the runtime executor, plan R3). `tokio-uring` was removed from this crate in T1.9 — it cannot run on glommio's reactor.
- Page format: `[4-byte CRC32 of the remainder][payload]` inside one 8 KiB block (keeps `O_DIRECT` offsets aligned); the reserved leading bytes seed the page header `cascade-page` will own.

## Open Design Questions

- Vectored I/O on `io_uring`: submit-N-and-await vs. true SQE chaining. Benchmark needed before committing.
- WAL flush LSN coordination with data-page writes — WAL crate drafted in M1 (T1.10).
