# cascade-bpm

Userspace Buffer Pool Manager.

## Role

Layer **03-memory**. Implements pillar §3.5 — the per-NUMA shared page pool: page pin/unpin, the sharded page table, NUMA-bound frame memory, and the §12.5 read-cache rules over `PageSource`. Since storage M8 the pool never writes pages anywhere: the log is the only durable write, only the page store writes pages.

## Public API

- `Bpm` (`Bpm::arced`) — the pool: `Send + Sync`, shared by all cores of its NUMA behind an `Arc<Bpm>`. Owns no store — every read call passes the caller's own `&impl PageSource` and an `OpCtx` (ADR-0003 item 2; the page store lives in `cascade-pagestore`).
- `Bpm::page` / `Bpm::page_exclusive` — FR-23 hit-or-load; misses fetch the page exactly as of the scope's durable LSN (§4.5), and the returned guards own a coherent 8 KiB copy-out snapshot (`'static`, ownable across awaits and threads).
- `PageGuard` / `PageGuardMut` — RAII pins (drop = unpin, panic-safe); `PageGuardMut::write_logged` stamps the page LSN with the WAL boundary covering the applied group (S17) — the apply half of the mini-transaction rule; the log holds the same bytes.
- `Bpm::evict_page` + the greedy in-miss eviction ladder (ADR-0003 item 6, M8 form): free → fresh → clean victim → durable-covered victim dropped in place → `PoolFull`. A page whose LSN is above the durable LSN is never evicted (§12.5); unlogged-dirty frames refuse too.
- `DurableLsn` (from `cascade-storage-api`) — the injected view of the scope's durable LSN (constructor argument): what misses read at and what eviction defers to.
- `home_numa` / `page_hash` — the frozen placement hash (ADR-0003 item 5); `PerDbAccounting` per-DB live pin counts (§6.1 reservation).

## Dependencies

- `cascade-types`, `cascade-telemetry`, `cascade-storage-api` (the `PageSource`/`DurableLsn` contracts it never owns), `libc` (mmap/mbind frame memory)

## v1 reservations honored

- **§6.1:** every pin / fetch operation tags pages with their `DatabaseId`; the registry keeps per-DB residency counts. Enforcement is a no-op now; the counters are real.
- **§6.4:** the BPM speaks to `PageSource`, never to raw files (its own frame mapping is memory, not files).
- **I4a:** frames bind to the pool's NUMA node via `mbind`; a refused bind degrades to first-touch with a warning.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.5, §6.1, §8.1; storage design §12.5 (the cache durability rules).
