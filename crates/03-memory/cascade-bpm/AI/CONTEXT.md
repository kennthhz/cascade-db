# cascade-bpm — AI Design Context

## Authoritative design

The BPM architecture is governed by [`docs/runtime-architecture.md`](../../../../docs/runtime-architecture.md), in particular §2.5 (NUMA affinity), §4 (Tier B scoping), and §8.1 (BPM concurrent access), and — since storage migration step M8 — by the storage layer design ([`crates/02-storage/AI/storage-layer-design.md`](../../02-storage/AI/storage-layer-design.md)) §12.5 (page cache durability rules). This file is per-crate context only.

## Role

The buffer pool manager: a **read cache over `PageSource`** (storage §4.5) between the page store and every higher layer that touches data pages (access methods, executor, columnar). Since M8 the log is the only durable write, only the page store writes pages, and this crate never writes a page anywhere — no flush path, no write-back, no checkpoint.

## Architecture (resolved from prior open questions)

- **One BPM per NUMA node.** Independent page tables, independent physical memory, no shared state across NUMAs. Per-NUMA sizing is configurable independently.
- **Within a NUMA**, the BPM is shared by all cores on that NUMA (Tier B). Sharded hash map for `PageId → Frame`, per-frame pin-word latch (the v1 latch; the OLC read-write latch lands with the btree).
- **Eviction is the minimal greedy ladder within each NUMA pool** (ADR-0003 item 6): free frame → fresh frame → clean unpinned victim → durable-covered unpinned victim → `PoolFull`. The shipped ladder is deliberate; LRU-K vs CLOCK-sweep stays open.

## The cache rules (storage M8, §12.5)

- **The miss path reads at the scope's durable LSN** (§4.5): the pool asks its injected [`DurableLsn`] view (`cascade-storage-api`) for the number and fetches the page exactly as of it. The frame remembers the *image's* page LSN — the end of the last record applied (S17) — carried in from the fetched image, raised by `PageGuardMut::write_logged` on the apply path.
- **A page is never evicted while its page LSN is above the durable LSN**: the page store may not have its changes yet. Once durability covers the stamp, the page DROPS — no write, no I/O; the page store rebuilds it. Dropping any page whose page LSN is at or below the durable LSN loses nothing (§12.5's invariant).
- **Unlogged changes are never evictable in v1**: a frame modified through plain `write` (no stamp) cannot be rebuilt from the log, so eviction would lose the image outright. Such frames are skipped by the ladder and refused by `evict_page` — the caller's contract (like PG's unlogged tables), with eviction taking the crash semantics' place. The cost shows up as pool pressure: the honest backpressure §12.5 leaves open.
- The durable-LSN view is a constructor argument (`Bpm::arced(config, accounting, durable)`): load-bearing on every miss and every eviction. The BPM never depends on the log store — the runtime glues the two (the `DurableLsn` adapter over the log store lands with the heap-AM wiring task, which also builds the runtime's `WalDurability`-era walgate replacement; the old `walgate.rs` died with the write-back).

## Page placement — driven by DB NUMA affinity

| DB affinity (from catalog) | Page placement | Lookup path |
|---|---|---|
| `single` (bound to NUMA *k*) | Allocate from NUMA *k*'s BPM only. | `bpm[k].lookup(page_id)` — NUMA-local. |
| `cross` | Distribute across all NUMAs via `hash(PageId) mod num_numas` (decided — runtime spec §2.5.1 / §8.1: placement is derivable from `PageId`). | Compute home NUMA via the placement function; single direct lookup; cross-NUMA load accepted. |

The BPM does **not** decide affinity; it reads the policy from the catalog (Tier C). The catalog publishes `pg_database.datnumaffinity` + `datnumanode` per DB.

## Pillar Reservations

- **§3.5:** the pillar this crate *is*.
- **§6.1 (Tenant governance):** per-DB page accounting must exist from day one, even if no global cap is enforced.
- **§6.4 (Disaggregated storage):** the BPM is unaware of *where* pages come from — it speaks only to `PageSource`. A future S3 page service plugs in below without touching this crate.

## Hard Invariants

1. **The log is the only durable write; pages are never written back.** The pool reads pages (`PageSource::get_page` at the durable LSN) and applies mini-transaction groups to cached frames (`write_logged` stamps the page LSN, S17). There is no flush, no write-back, no dirty eviction. (Supersedes the old WAL-before-data writeback invariant — the page store replays the same log, so a written-back cache would be redundant *and* wrong.)
2. **A page whose page LSN is above the scope's durable LSN is never evicted** (§12.5); unlogged-dirty frames are never evicted in v1 either (invariant 1's consequence: nothing can rebuild them).
3. **Page pinned ⇒ page not evicted.** Eviction must respect pin counts (and claims). No exceptions.
4. **Per-DB residency is real, not synthetic.** Counters increment on actual pin/unpin, not on best-effort sampling.
5. **All reads go through `PageSource`.** No raw file I/O; the pool owns no store — every call names its own `&impl PageSource` (§8.1's no-file-handle-across-reactors rule, structural since the pool owns nothing).
6. **`PageGuard`/`PageGuardMut` are RAII.** Forgetting to drop one is a pin leak; the type system catches it via `must_use` and the borrow checker.
7. **NUMA-affinity contract honored.** `single`-affinity DBs' pages live in exactly one NUMA pool. Hot-path cross-NUMA traffic for these DBs is an invariant violation (runtime spec I4b).

## Out of Scope

- Page layout (lives in `cascade-page`).
- Index-specific traversal hints (lives in `cascade-btree`; BPM stays generic).
- The `O_DIRECT` mechanics and page materialization (lives in `cascade-pagestore` over `cascade-blockdev`).
- Retention and WAL recycling (the log store owns it, storage §5.6 — the runtime recycler that consumed the old frontier retired with M8).
- Resource *enforcement* (lives in `cascade-governance`; this crate just provides the counters).
- Affinity *decisions* (live in the catalog + runtime; BPM only enforces).

## Existing Code (the cross-core pool — T1.9 + T6.2 + M8)

The per-NUMA shared pool is real in `src/lib.rs` (ADR-0003): `Bpm` is `Send + Sync`, shared by every core on its NUMA behind `Arc<Bpm>` (`Bpm::arced` is the only constructor — guards recover the handle through the pool's self-`Weak`, no pointer games). It owns NO store: every read call takes the caller's own `&impl PageSource` per call (the contract is dyn-compatible, but per-call generics keep the §8.1 rule structural). Frames live in an in-crate `mmap`+`mbind` mapping (`BpmConfig.numa`; refused binds degrade to first-touch with a warn — I4a), never `PoolRegistry` memory (the demo heap aliases that by raw address). The page table is the ADR's hand-rolled sharded `RwLock<HashMap>` (`next_pow2(2 × cores_on_numa)` shards; `get_or_insert` under one shard lock contains the double-install race; sharding is internal and unfrozen — only the placement hash is).

**Frame access** is the ADR item-3 model: per-frame `AtomicU32` pin word (bit 31 EXCL writer / bit 30 CLAIM load-or-eviction reservation / low 30 bits shared count — the three successes are mutually exclusive single-word CAS states, which makes snapshots coherent without a lock across the memcpy) + a per-frame `Mutex` guarding bytes and metadata (page id, scope, dirty, page_lsn) together. Guards are `'static` copy-outs (an owned 8 KiB `AlignedBuf` — one memcpy per touch, accepted): `page()` → shared `PageGuard`; `page_exclusive()` → `PageGuardMut` whose `write`/`write_logged` closures mutate the real frame under the mutex and never span an await. Pins refuse with `BpmError::Contended` (fail fast — the caller owns the retry); hits pin under the shard read lock so a concurrent eviction cannot swap the frame between lookup and pin. Eviction claims the frame first — pins refused — re-verifies the §12.5 rule under the claim, then unmaps.

**What M8 deleted**: `WalDurability` (→ `DurableLsn` in cascade-storage-api), `set_gate`, `flush_page`/`flush_guarded`/`flush_claimed`, the write ticket, `mod_count`, the generation re-verification, `BpmError::WalGate`, `dirty_pages()` (the checkpointer's scan set), and the gate-wait in eviction. The runtime's `checkpointer.rs`/`recycler.rs`/`walgate.rs` died with it. `dirty` now means exactly "the image may carry changes the log does not cover" — load-bearing only for the unlogged case and diagnostics.

`home_numa`/`page_hash` (the frozen FNV-1a placement, ADR-0003 item 5) live here; the runtime's `pools.rs` wrapper delegates and keeps the runtime-side parity goldens. `PerDbAccounting` counts live pins per DB (invariant 4).

## Tests (`tests/pool.rs`)

The harness is the storage contract's fakes — `MemLogStore` + `MemPageStore` (toy redo from the conformance module) + `MemDurableLsn` — so the whole suite runs without glommio: every future resolves inline, `block_on` polls (the write-back era's gate-wait tests needed an executor; the M8 pool has no waits). The suite covers: pin/unpin basics, panic release, publish races, pool-full, the T6.2 four-reactor double-install, cross-thread guards, the M8 rules (miss at the durable LSN; never-evict above durable, by name and by ladder; drop-below-durable re-served by the store; unlogged refuse; clean evict does no I/O), and placement (first-touch degrade + the `#[ignore]` bare-metal `numa_maps` check). A hang here is an honest deadlock — no ring, no watchdog.

## Open Design Questions

- Eviction algorithm within each NUMA pool: LRU-K vs CLOCK-sweep. Lean LRU-K for OLTP; CLOCK lower overhead. (The shipped greedy ladder is ADR-0003 item 6's deliberate minimal form; its dirty rung now drops durable-covered pages instead of writing them.)
- In-flight load dedup: concurrent same-page misses each read the source outside all locks (by design — no I/O under locks); exactly-once applies to the table install, not the read. An in-flight-load registry would dedupe the reads; deferred until measured.
- The v1 latch is a pin word, not a full OLC read-write latch — the OLC latch (with version-validated RCU-style reads) lands with the btree.
- Memory pressure when the durable LSN lags (§12.5's open question): the ladder's refusal is v1's whole answer. Backpressure into writers, or a bounded undurable-window size, needs a design of its own.
- How the columnar engine (§3.7) plugs in — same per-NUMA BPM with a separate page-kind tag, or its own pool layer.
