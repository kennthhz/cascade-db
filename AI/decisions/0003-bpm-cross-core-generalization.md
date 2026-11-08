# ADR 0003 — BPM cross-core generalization (A17 and the decisions it drags in)

**Status:** Accepted
**Date:** 2026-11-08

## Context

The M1-era `Bpm<S: PageStore>` is single-reactor by construction: `!Send + !Sync`
(compile-enforced), a `RefCell<HashMap<PageId, u32>>` page table (the A17
placeholder), frames of individually-allocated `AlignedBuf`s (boot-thread
first-touch — the wrong NUMA), and an exclusive-pin access model whose soundness
argument is "`!Send` means no cross-thread observer; the pin==1 assert excludes
same-thread writers." That argument does not survive sharing. The spec (§8.1)
commits to **one BPM per NUMA, shared by that node's cores**, and M6 (the
cascade-heap slice) is the first consumer that needs it for real: heap tuples on
pages, checkpoint passes over live pools, a frontier that leaves 0.

A17 asked one question (flurry vs `scc::HashMap` vs hand-rolled for the page
table); making the BPM cross-core forces five more decisions in the same breath:
store ownership, frame access safety, frame memory NUMA-locality (I4a), the
placement hash's home crate, and eviction. They are decided together here because
their soundness arguments interlock.

## Decision

1. **Page table: hand-rolled sharded `RwLock<HashMap<PageId, u32>>` array.**
   `next_pow2(2 × cores_on_this_numa)` shards; `PageId → shard` is a pure
   function, so the double-install race is contained entirely inside one shard's
   lock — a `get_or_insert` under the write lock resolves it with no CAS loops
   and no epoch machinery. Sharding is internal and deliberately NOT frozen
   (only the placement hash is frozen); escalate to flurry only if a measured
   contention counter or stall-detector regression says so.

2. **The BPM owns no store.** `Bpm<S>` becomes `Bpm` with generic per-call
   `&impl PageStore` parameters. `PageStore` is AFIT (`async_fn_in_trait`) and
   therefore not dyn-compatible, so `Arc<dyn PageStore>` is not an option; and
   §8.1's rule — each reactor opens its own file handles on its own ring, "no
   BPM object carries a file handle across reactors" — becomes structural: no
   code path can touch a page without naming a store, and the only stores in
   scope are the caller's own.

3. **Frame access: per-frame `std::Mutex` + copy-out guards + write-flag pin
   protocol.** Readers pin (fetch_add, failing on the write flag) and receive a
   `PageGuard` that owns an `AlignedBuf` snapshot copied under the frame mutex;
   `write(f)` mutates the real frame under the mutex inside the sync closure
   (the API cannot hold the lock across an await — the I11 story by
   construction), then refreshes the copy. Exclusive pin is CAS `0 →
   0x8000_0001`; `page_lsn` becomes `AtomicU64` with 0 as the `None` sentinel.
   No `UnsafeCell` remains. One 8 KiB memcpy per touch is the accepted cost —
   PostgreSQL itself copies tuples out under a short latch; this is the
   page-granularity version. The I11 lock table: shard `RwLock` (bounded ~ns,
   never across an await, only on same-shard collision), frame `Mutex` (bounded:
   one 8 KiB memcpy, ~µs, never across an await), tail hints and in-flight slots
   (atomics). The pin write-flag protocol is the v1 latch; the RW-latch with
   optimistic read (OLC) lands with the btree.

4. **Frames: an in-crate mbind mapping per pool, never `PoolRegistry` memory.**
   `cascade-bpm` (layer 3) grows its own `mbind`-based frame constructor with
   the pools.rs warn-and-degrade behavior (I4a), so boot can build each pool on
   its own node. The `PoolRegistry` mapping is contract-plus-demo-fixture:
   `demoheap` writes bench pages into it by raw address, and BPM frames there
   would alias them. A later milestone may hand the registry's memory over once
   the demo heap retires.

5. **`home_numa(page_id, num_numas)` — the frozen FNV-1a placement hash —
   moves into `cascade-bpm`**, beside the code that consumes it (page residency
   and the future eviction sharding). The runtime's `pools.rs::pool_for_page`
   becomes a delegating wrapper; the golden literals move into cascade-bpm's
   tests and a parity test stays in the runtime.

6. **Eviction: minimal greedy now, CLOCK-sweep deferred.** On a full pool:
   reuse a pinned-free clean frame, else evict one pinned-free dirty page
   through the WAL gate (no second writeback path), else `PoolFull`. Scans make
   this mandatory from day one — a table larger than the pool must not turn the
   next miss into a wire error — but a reference-bit sweep is unjustified until
   measured.

## Consequences

- The M1 shapes were already production-shaped at the API level (the crate's
  AI/CONTEXT called the generalization "additive"); what changed is the
  soundness argument, not the contracts: pins are still atomic, dirty is still
  atomic, the gate is still the only writeback path.
- One 8 KiB memcpy per page touch; accepted and stated (see 3).
- `PageGuard` stops borrowing the BPM — guards are ownable across awaits, which
  the heap's insert/scan paths use.
- `WalDurability` gains `Send + Sync` supertraits and the runtime gate becomes
  `Arc<dyn WalDurability>` (`WalGate` over `Arc<WalWriter>` converts
  mechanically).
- Frontier semantics now depend on the drained-pass rule and the in-flight-apply
  registry (FR-48/FR-49), not on stamps captured at scan time — the
  min-over-dirty-at-start floor the M3 pass used is only sound while recovery
  replays from 0 and nothing trusts pages. Cross-reference: plan §4 M6, T6.5.
- A17 closes as Resolved; the eviction-algorithm question in
  `cascade-bpm/AI/CONTEXT.md` stays open (CLOCK-sweep vs LRU-K) until measured.

## Alternatives considered

- **`flurry` for the page table.** Rejected: crossbeam-epoch reclamation is a
  new workspace dependency whose guard lifetimes must not cross an await (same
  discipline as our locks, less obvious), reads pay epoch pin/unpin, and the
  escalation path is preserved if contention ever measures.
- **`scc::HashMap`.** Rejected: heavier write path and async-backoff features
  that are dead weight in a thread-per-core runtime with no spare threads.
- **In-place frame slices guarded by a latch held across awaits.** Rejected:
  exposes blocking-across-await and writer starvation for zero API benefit;
  copy-out is what PG-style engines do at page granularity anyway.
- **Keep `Bpm<S>` per (reactor, NUMA).** Rejected: N-cores-per-NUMA duplication
  of pools fragments residency (the same page pinned in several pools) and
  violates §8.1's shared-per-NUMA model outright.
- **`Arc<dyn PageStore>` for the de-owned store.** Rejected: the AFIT trait is
  not dyn-compatible; generic methods are the only sound shape (and the
  ring-ownership rule falls out structurally).
- **BPM frames in `PoolRegistry` memory.** Rejected: aliases the demo heap's
  raw-address pages; the registry is a sizing/placement contract, not the BPM's
  owner.
