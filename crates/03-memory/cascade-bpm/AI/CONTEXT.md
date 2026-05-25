# cascade-bpm — AI Design Context

## Authoritative design

The BPM architecture is governed by [`docs/runtime-architecture.md`](../../../../docs/runtime-architecture.md), in particular §2.5 (NUMA affinity), §4 (Tier B scoping), and §8.1 (BPM concurrent access). This file is per-crate context only.

## Role

The buffer pool manager. Sits between the storage layer (`cascade-storage`) and every higher layer that touches data pages (access methods, executor, columnar).

## Architecture (resolved from prior open questions)

- **One BPM per NUMA node.** Independent page tables, independent physical memory, no shared state across NUMAs. Per-NUMA sizing is configurable independently.
- **Within a NUMA**, the BPM is shared by all cores on that NUMA (Tier B). Concurrent hash map for `PageId → Frame`, per-page RW-latch with OLC.
- **Eviction is sharded within each NUMA pool** by `hash(PageId) mod cores_on_this_numa`. Each core owns eviction decisions for its shard of its NUMA's pool.

## Page placement — driven by DB NUMA affinity

| DB affinity (from catalog) | Page placement | Lookup path |
|---|---|---|
| `single` (bound to NUMA *k*) | Allocate from NUMA *k*'s BPM only. | `bpm[k].lookup(page_id)` — NUMA-local. |
| `cross` | Distribute across all NUMAs (round-robin or `hash(PageId) mod num_numas` — TBD, see runtime spec §11). | Check expected NUMA by placement function; cross-NUMA load accepted. |

The BPM does **not** decide affinity; it reads the policy from the catalog (Tier C). The catalog publishes `pg_database.datnumaffinity` + `datnumanode` per DB.

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
6. **NUMA-affinity contract honored.** `single`-affinity DBs' pages live in exactly one NUMA pool. Hot-path cross-NUMA traffic for these DBs is an invariant violation (runtime spec I4b).

## Out of Scope

- Page layout (lives in `cascade-page`).
- Index-specific traversal hints (lives in `cascade-btree`; BPM stays generic).
- The `O_DIRECT` mechanics (lives in `cascade-storage`).
- Resource *enforcement* (lives in `cascade-governance`; this crate just provides the counters).
- Affinity *decisions* (live in the catalog + runtime; BPM only enforces).

## Open Design Questions

- Eviction algorithm within each NUMA pool: LRU-K vs CLOCK-sweep. Lean LRU-K for OLTP; CLOCK lower overhead.
- Concrete concurrent hash map: `flurry` vs `scc` vs hand-rolled sharded array. ADR when implementation lands.
- `cross`-affinity placement function: round-robin vs hash. Tradeoff: hash gives deterministic page-to-NUMA mapping (better for repeat access); RR gives uniform pool fill.
- How the columnar engine (§3.7) plugs in — same per-NUMA BPM with a separate page-kind tag, or its own pool layer.
