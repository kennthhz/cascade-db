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
| `cross` | Distribute across all NUMAs via `hash(PageId) mod num_numas` (decided — runtime spec §2.5.1 / §8.1: placement is derivable from `PageId`). | Compute home NUMA via the placement function; single direct lookup; cross-NUMA load accepted. |

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

## Existing Code (the cross-core pool — T1.9 + T3.2 gate + T6.2 generalization)

The per-NUMA shared pool is real in `src/lib.rs` (ADR-0003): `Bpm` is `Send + Sync`, shared by every core on its NUMA behind `Arc<Bpm>` (`Bpm::arced` is the only constructor — guards recover the handle through the pool's self-`Weak`, no pointer games). It owns NO store: every I/O call takes the caller's own `&impl PageStore` per call (the AFIT trait is not dyn-compatible — §8.1's "no file handle across reactors" is structural: nothing can touch a page without naming a store). Frames live in an in-crate `mmap`+`mbind` mapping (`BpmConfig.numa`; refused binds degrade to first-touch with a warn — I4a), never `PoolRegistry` memory (the demo heap aliases that by raw address). The page table is the ADR's hand-rolled sharded `RwLock<HashMap>` (`next_pow2(2 × cores_on_numa)` shards; `get_or_insert` under one shard lock contains the double-install race; sharding is internal and unfrozen — only the placement hash is).

**Frame access** is the ADR item-3 model: per-frame `AtomicU32` pin word (bit 31 EXCL writer / bit 30 CLAIM load-or-eviction reservation / low 30 bits shared count — the three successes are mutually exclusive single-word CAS states, which is what makes snapshots coherent without a lock across the memcpy) + a per-frame `Mutex` guarding bytes and metadata (page id, dirty, page_lsn, `mod_count`) together. Guards are `'static` copy-outs (an owned 8 KiB `AlignedBuf` — one memcpy per touch, accepted): `page()` → shared `PageGuard`; `page_exclusive()` → `PageGuardMut` whose `write`/`write_logged` closures mutate the real frame under the mutex and never span an await. Pins refuse with `BpmError::Contended` (fail fast — the caller owns the retry); hits pin under the shard read lock so a concurrent eviction cannot swap the frame between lookup and pin.

**The writeback gate** (T3.2, invariant 1) is cross-core now: `flush_page` holds NO pin — writers proceed during the gate wait (blocking them for a WAL stall would stall every session touching the page). The store write is serialized per frame by a write ticket and re-verified against the snapshot's *generation* (`page_id, page_lsn, mod_count`) before it lands, and dirty clears only on generation match — a page re-modified, evicted, or reloaded while the flush sat on the gate can never have its newer store image clobbered. A stamped page with no gate refuses writeback outright; unlogged dirty pages (plain `write`) are the caller's contract and write ungated. **Eviction** (`evict_page` + the greedy in-miss ladder: free list → fresh frame → clean unpinned victim → one dirty unpinned victim through the gate → `PoolFull`) claims the frame first — pins refused — and routes through the same gate code: no second writeback path. Behavior change from the single-reactor slice: flushes no longer refuse PINNED pages (eviction still refuses them).

`home_numa`/`page_hash` (the frozen FNV-1a placement, ADR-0003 item 5) live here; the runtime's `pools.rs` wrapper delegates and keeps the runtime-side parity goldens. `PerDbAccounting` counts live pins per DB (invariant 3).

## Open Design Questions

- Eviction algorithm within each NUMA pool: LRU-K vs CLOCK-sweep. Lean LRU-K for OLTP; CLOCK lower overhead. (The shipped greedy ladder is ADR-0003 item 6's deliberate minimal form.)
- In-flight load dedup: concurrent same-page misses each read the store outside all locks (by design — no I/O under locks); exactly-once applies to the table install, not the read. An in-flight-load registry would dedupe the reads; deferred until measured.
- The v1 latch is a pin word, not a full OLC read-write latch — the OLC latch (with version-validated RCU-style reads) lands with the btree.
- How the columnar engine (§3.7) plugs in — same per-NUMA BPM with a separate page-kind tag, or its own pool layer.
