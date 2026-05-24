# cascade-bpm — AI Design Context

## Role

The buffer pool manager. Sits between the storage layer (`cascade-storage`) and every higher layer that touches data pages (access methods, executor, columnar).

## Pillar Reservations

- **§3.5:** the pillar this crate *is*.
- **§6.1 (Tenant governance):** per-DB page accounting must exist from day one, even if no global cap is enforced.
- **§6.4 (Disaggregated storage):** the BPM is unaware of *where* pages come from — it speaks only to `PageStore`. A future S3 page service plugs in below without touching this crate.

## Hard Invariants

1. **WAL-before-data.** A dirty page must not be written to `PageStore` until the WAL flush LSN ≥ the page's PageLSN. Enforced inside the BPM.
2. **Page pinned ⇒ page not evicted.** Eviction must respect pin counts. No exceptions.
3. **Per-DB residency is real, not synthetic.** Counters increment on actual pin/unpin, not on best-effort sampling.
4. **No raw file I/O.** All I/O goes through `PageStore`.
5. **`PageGuard` is RAII.** Forgetting to drop one is a pin leak; the type system catches it via `must_use` and the borrow checker.

## Out of Scope

- Page layout (lives in `cascade-page`).
- Index-specific traversal hints (lives in `cascade-btree`; BPM stays generic).
- The `O_DIRECT` mechanics (lives in `cascade-storage`).
- Resource *enforcement* (lives in `cascade-governance`; this crate just provides the counters).

## Open Design Questions

- Eviction algorithm: LRU-K vs CLOCK-sweep. Lean LRU-K for OLTP, but CLOCK is friendlier to thread-per-core (less synchronization).
- Shard-per-core vs single global pool with per-shard locks. Thread-per-core (§3.2) strongly suggests sharded — each core owns its slice of the pool.
- How does the columnar engine (§3.7) plug in — same BPM with a separate page-kind tag, or its own pool?
