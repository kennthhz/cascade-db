# cascade-bpm

Userspace Buffer Pool Manager.

## Role

Layer **03-memory**. Implements pillar §3.5 — the per-NUMA shared page pool: page pin/unpin, the sharded page table, greedy eviction, NUMA-bound frame memory, and the WAL-before-data flush gate.

## Public API

- `Bpm` (`Bpm::arced`) — the pool: `Send + Sync`, shared by all cores of its NUMA behind an `Arc<Bpm>`. Owns no store — every call passes the caller's own `&impl PageStore` (ADR-0003 item 2).
- `Bpm::page` / `Bpm::page_exclusive` — FR-23 hit-or-load; the returned guards own a coherent 8 KiB copy-out snapshot (`'static`, ownable across awaits and threads).
- `PageGuard` / `PageGuardMut` — RAII pins (drop = unpin, panic-safe); `PageGuardMut::write_logged` stamps the WAL boundary covering the modification.
- `Bpm::flush_page` — the T3.2 writeback gate: waits for WAL durability, writes under a per-frame write ticket with generation re-verification (no stale-image clobber cross-core).
- `Bpm::evict_page` + the greedy in-miss eviction ladder (ADR-0003 item 6; CLOCK-sweep/LRU-K deferred).
- `WalDurability` — the injected gate contract (`Arc<dyn WalDurability>`; the runtime's `walgate.rs` implements it over the WAL writer).
- `home_numa` / `page_hash` — the frozen placement hash (ADR-0003 item 5); `PerDbAccounting` per-DB live pin counts (§6.1 reservation).

## Dependencies

- `cascade-types`, `cascade-telemetry`, `cascade-storage` (the `PageStore` contract it never owns), `libc` (mmap/mbind frame memory)

## v1 reservations honored

- **§6.1:** every pin / fetch operation tags pages with their `DatabaseId`; the registry keeps per-DB residency counts. Enforcement is a no-op now; the counters are real.
- **§6.4:** the BPM speaks to `PageStore`, never to raw files (its own frame mapping is memory, not files).
- **I4a:** frames bind to the pool's NUMA node via `mbind`; a refused bind degrades to first-touch with a warning.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.5, §6.1, §8.1.
