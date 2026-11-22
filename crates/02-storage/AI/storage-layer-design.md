# Cascade DB Storage Layer: Contracts, Internal Design and Migration

**Status:** Draft v0.5 for review, 2026-11-22. **This document is the single source of truth for the storage layer.** v0.5 takes over the storage foundations of HLD v2, which Kenneth retired on 2026-11-22 (Section 12); HLD v2's transaction sections moved to WP2, its engine, index and catalog sections to the per-component design documents in `components/`, and its access API and TOAST sections to WP5. v0.4 fixed the adversarial review (Appendix C), v0.3 made the mini-transaction the unit of durability, v0.2 fixed the first review (Appendix A) and recorded the Aurora-style alternative (Appendix B). Earlier versions are in `history/`.
**Upstream:** [`architecture-design.md`](../../../docs/architecture-design.md) v0.10.2, §2.1 to §2.4, §4.3, §4.12 and §7. Where this document and the architecture disagree, the architecture wins unless this document says it proposes a change to it (Section 11).
**Replaces:** HLD v2 (retired; kept in `docs/history/storage-subsystem-hld-v2.md`) for everything about the log, redo, page cache rules, storage and recovery, and the `cascade-storage` crate's `CONTEXT.md`. Companion documents: the per-component design documents in [`docs/components/`](../../../docs/components/) for engines, indexes and catalog, WP5 for the access API and TOAST, and [`docs/storage-wp2-txn-manager.md`](../../../docs/storage-wp2-txn-manager.md) for clocks, commit and the read rule. The differences that this document resolves are listed in [`docs/architecture-vs-storage-docs.md`](../../../docs/architecture-vs-storage-docs.md).
**Code baseline:** `main` at `898d530` (2026-11-22). Work that exists only on Kenneth's machine is not reflected.

---

## 1. Purpose and Scope

This document is the detailed design for the storage layer (L2) of Cascade DB. It has three parts:

1. **The contracts** in `cascade-storage-api`: what the rest of the system may call, the data that crosses each boundary, the errors, and the promises callers may rely on. This is the final API that the architecture's sketches in §4.3 were a starting point for.
2. **The internal design** of the two storage components that implement those contracts, the log store (`cascade-logstore`) and the page store (`cascade-pagestore`), together with the block-device contract they both write through.
3. **The migration** from the code that exists today, `cascade-storage` and `cascade-wal`, to the new crates, as a sequence of small steps that keep the build and the existing tests green.

The archive (`cascade-archive`), the temporary store (`cascade-tempstore`) and the network clients and servers (`cascade-storage-remote`) are later phases. This document defines only the contract features they rely on, so that adding them later does not change the contracts.

### 1.1. Goals

- **Compute never writes pages.** Compute sends log to the log store and reads pages from the page store, in both deployment modes.
- **One set of contracts for both editions.** The single server wires the log store and page store in process. The managed service wires the same components behind network clients. Nothing above L2 can tell the difference.
- **Every component is testable in isolation.** Each contract ships an in-memory fake and a conformance suite. The log store is tested without a page store, and the page store is tested against a fake log source.
- **Density.** An idle scope costs memory for its metadata only, and 10,000 open scopes on one log store node must be affordable (§2.4).
- **No noisy neighbors on the commit path.** One scope's burst of large writes must not slow the commits of the other scopes that share a log store flush.

### 1.2. Non-goals for version 1

- Keeping older page versions. A single server has no followers, and a leader never needs an older version than the page store holds (Section 4.5, promise 5). The contract already includes a read floor so that the managed service can add page versions without a contract change.
- Branch creation and point-in-time restore. The contract includes the hooks (lineage on attach, and a timestamp-to-LSN lookup) but the implementation arrives with `cascade-archive`.
- Compression of log or pages.

---

## 2. Summary of Decisions

| # | Decision | Section |
| :--- | :--- | :--- |
| S1 | `cascade-storage-api` holds the three data-path contracts (`LogSink`, `LogSource`, `PageSource`), two admin contracts (`LogScopes`, `PageScopes`), and the two contracts that the storage components consume (`KeyProvider`, `BlockDevice`) | §3, §4 |
| S2 | The contracts are object-safe and return boxed futures, so callers hold `Arc<dyn LogSink>` and similar. They are not generic over the implementation | §4.1 |
| S3 | The shared contracts are `Send + Sync`. Their futures, subscriptions and per-core block handles are not `Send`: they are used only on the calling core | §4.1 |
| S4 | Every call takes an explicit `&OpCtx`, which carries tenant, database, tracing span and deadline (Rule 8) | §4.2 |
| S5 | The log store understands record framing (length, LSN, epoch, CRC) through `cascade-wal-format`, but never record payloads | §4.3 |
| S6 | `append` is idempotent for identical bytes at the same LSN, and rejects gaps and conflicting overlaps | §4.3 |
| S7 | Each append carries the highest commit timestamp in its bytes, so the log store can find the append that holds a restore point without parsing payloads | §4.3, §4.4 |
| S8 | Retention holders are named and registered. With no holder registered, the log store discards nothing | §4.4 |
| S9 | `PageSource` serves pages exactly as of an LSN. Readers report a read floor, and a request for a page version the page store no longer holds fails with `TooOld` (a proposed addition to the architecture) | §4.5 |
| S10 | Scopes are created and dropped on the log store and attached to the page store through separate admin contracts, never as a side effect of `open_writer` or `get_page` | §4.6 |
| S11 | Encryption at rest happens inside the storage components. Bytes cross the contracts in plaintext, protected in transit by TLS between nodes | §4.7, §7 |
| S12 | The log store writes many scopes into shared segment files as 4 KiB-aligned flush frames, with one shared flush | §5 |
| S13 | Fair sharing of each flush uses deficit round robin across scopes, with a per-scope quantum per flush | §5.4 |
| S14 | The page store writes pages in place through a doublewrite area, which protects against torn pages without keeping the log forever | §6 |
| S15 | The glommio `O_DIRECT` implementation of `BlockDevice` lives in the runtime adapter, as Rule 4 requires. Until the runtime is split, it lives in a module of `cascade-storage` | §4.8, §9 |
| S16 | **The mini-transaction is the unit of durability.** The last record of every mini-transaction carries a group-end flag, every append ends on a group end, and the durable LSN always falls on a mini-transaction boundary (confirmed by Kenneth, 2026-11-22) | §4.3, §5.4, §9 |
| S17 | **The page LSN is the end LSN of the last applied record**, as in PostgreSQL and in today's code. A page may be evicted when its page LSN is at or below the durable LSN, and a record applies to a page only if its start is at or above the page LSN | §4.2, §4.5 |
| S18 | `ScopeId` and `SegmentId` are never reused. Both are allocated from counters that survive restarts | §4.2 |
| S19 | A failed sync is permanent. It fails the whole stream, which recovers only through a fresh scan that trusts nothing written since the last successful sync | §4.8, §5.7 |
| S20 | The `LogSink` promises describe the quorum's view. Each replica receives the writer's durable LSN with every append and serves reads only below it, and a newer epoch may replace a replica's non-durable tail | §4.3.1 |
| S21 | Dropping a scope destroys its encryption key after a recovery window, which makes the scope's data unrecoverable everywhere, including the archive (crypto-shredding). Until then the drop can be undone (decided by Kenneth on Claude's recommendation, 2026-11-22) | §7 |

---

## 3. Crates and Dependencies

| Crate | Layer | Contains | Depends on |
| :--- | :--- | :--- | :--- |
| `cascade-storage-api` | L2 contract | The contracts, shared types and errors in Section 4; the in-memory fakes (feature `fake`); the conformance suites (feature `conformance`) | `cascade-types`, `cascade-wal-format` (framing only), `cascade-telemetry` |
| `cascade-logstore` | L2 | The log store (Section 5). Implements `LogSink`, `LogSource` and `LogScopes` for one replica | `cascade-storage-api`, `cascade-wal-format`, `cascade-rt-api` |
| `cascade-pagestore` | L2 | The page store (Section 6). Implements `PageSource` and `PageScopes` | `cascade-storage-api`, `cascade-wal-format`, `cascade-redo`, `cascade-page`, `cascade-rt-api` |
| The runtime adapter | Adapter | The glommio implementation of `BlockDevice` | `cascade-storage-api` |
| `bin/cascade-server` | Root | Wires a local log store and page store into the same process on a single server | Everything |

**Two layers of log API.** Compute code never calls `LogSink` directly. The engines, mini-transactions and the transaction manager call the L3 `Log` contract in `cascade-log-api`, which works in records: reserve an LSN, fill the record, wait for durability, and tap the stream for log shipping. Its implementation, `cascade-log` (today's `WalWriter`), owns LSN reservation across the scope's cores, the filled watermark and group-commit batching, and sends filled byte ranges down to `LogSink`. `LogSink` works in bytes and epochs, and is implemented by the local log store or by the quorum client. Because only `cascade-log` sees `LogSink`, switching between the single server and the managed service changes nothing above L3.

The log store and the page store never depend on each other. The page store sees only a `LogSource`, and the log store sees only retention calls from its holders.

**Where `BlockDevice` lives.** The architecture names "an injected block-device contract" but no crate for it. Putting it in `cascade-storage-api` keeps the architecture's component table unchanged, and the temporary store, which is also L2, can use it from there. The runtime adapter implements it, which adapters are allowed to do.

---

## 4. The Contracts: `cascade-storage-api`

### 4.1. Dispatch and thread affinity

The architecture states that `LogSink`, `LogSource` and `PageSource` are `Send + Sync`, because every core in a scope's core set calls them. Callers must be able to hold either the local implementation or the quorum client without becoming generic over it. The contracts are therefore object-safe traits whose async methods return `LocalBoxFuture<'_, T>`, the same pattern the buffer pool's `WalDurability` gate uses today.

The cost is one heap allocation per call. On the commit path the log writer calls `append` and `wait_durable` once per group-commit batch, not once per record, so the cost is small next to a disk flush. Page reads happen only on a cache miss, where the I/O dominates.

The futures are not `Send`. A call made on core 3 is polled to completion on core 3, which is what a thread-per-core runtime expects. `LogSubscription` is also not `Send`. The conformance suites check these properties with static assertions (Rule 10).

### 4.2. Shared types

These types are added to `cascade-types` as a prerequisite (Section 9, step M0), because the architecture puts them there:

```rust
pub struct ScopeId(pub u64);      // a writing scope: a database in v1
pub struct Epoch(pub u64);        // ownership epoch; 0 is never issued
pub struct NodeId(pub u32);
pub struct OpCtx<'a> {
    pub tenant: TenantContext,    // DatabaseId plus optional TenantId
    pub span: &'a Span,
    pub deadline: Option<Instant>,
}
```

**Identifiers are never reused (S18).** A dropped scope's `ScopeId` and a dropped or truncated segment's `SegmentId` are retired for good, and new ones come from counters that survive restarts. Reusing either would let stale data come back: the page store could return a truncated table's old page where the new table expects zeros, and the log store's recovery scan could attach a dropped scope's old frames to a new scope with the same ID. A segment drop or truncate is also a logged record, which the page store applies by discarding the segment's files.

**The page LSN is an end LSN (S17).** A page's LSN is the end of the last record applied to it, which is how `cascade-page` and `cascade-bpm` already stamp pages. Every rule then reads the same way: the cache may evict a page when its page LSN is at or below the durable LSN; redo applies a record to a page only if the record's start is at or above the page LSN; a page served as of `at_lsn` has a page LSN at or below `at_lsn`. HLD v2 §8, WP1's idempotence rule and the architecture's §4.4 describe the page LSN as the record's start, and must be corrected to match (Section 11).

The contract crate defines the rest:

```rust
/// A page within a scope. A scope owns its pages, so the database ID in
/// today's `PageId` is replaced by the scope.
pub struct PageKey { pub segment: SegmentId, pub page_no: PageNo }   // SegmentId is never reused (S18)

/// Whole log records, in LSN order, with no gaps.
pub struct LogChunk {
    pub start: Lsn,               // LSN of the first record
    pub bytes: Bytes,             // one or more whole records
    pub end: Lsn,                 // start + bytes.len(); the next chunk starts here
    pub durable: Lsn,             // the scope's durable LSN when the chunk was read
}

pub struct PageImage {
    pub buf: AlignedBuf,          // PAGE_SIZE bytes, decrypted, CRC already checked
    pub page_lsn: Lsn,            // END LSN of the last record applied (S17); 0 if never written
}

pub struct AppendMeta {
    pub max_commit_ts: Option<Hlc>,   // highest commit timestamp in these bytes
    pub writer_durable: Lsn,          // the writer's current durable LSN (S20); replicas serve reads only below it
}

#[non_exhaustive]
pub enum RetentionHolder {
    PageStore { instance: u64 },
    Follower { node: NodeId },
    Cdc { slot: u64 },
    Archive,
}
```

### 4.3. `LogSink`: compute writes the log

```rust
pub trait LogSink: Send + Sync {
    /// Takes ownership of `scope` at `epoch`. Fences every older epoch, so
    /// their appends fail from now on. Durably records the new epoch before
    /// returning, and returns the durable LSN, which is where the new
    /// writer's first append must start.
    fn open_writer<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, epoch: Epoch)
        -> LocalBoxFuture<'a, Result<Lsn, StoreError>>;

    /// Appends whole records starting at `start`. Returns once the bytes are
    /// accepted, not once they are durable.
    fn append<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, epoch: Epoch,
                  start: Lsn, bytes: &'a [u8], meta: AppendMeta)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;

    /// The scope's durable LSN: every byte below it is durable. Never blocks.
    fn durable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// Resolves once the durable LSN is at or above `lsn`. Waiting is what
    /// triggers a flush, so this is where group commit happens.
    fn wait_durable<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, lsn: Lsn)
        -> LocalBoxFuture<'a, Result<Lsn, StoreError>>;
}
```

**Promises.**

1. *Fencing.* After `open_writer(scope, e)` returns, any `append` or `open_writer` with an epoch below `e` fails with `Fenced { current }`. Opening also discards the scope's accepted but not yet flushed bytes from older epochs, since they were never acknowledged, and the new writer starts at the returned durable LSN. A second `open_writer` with the same epoch is treated as a retry and returns the durable LSN again. `open_writer` is serialized with the scope's flushes: the fence is written only after any frame already in flight has completed, and the durable LSN it returns is computed at that moment. Otherwise an old-epoch frame could become durable after the fence, at the same LSNs the new writer is about to use. The epoch check happens before the idempotence check in promise 3, so a re-send of identical bytes from a fenced epoch fails with `Fenced`. The log store does not check which node holds an epoch: the metadata store already promises that at most one node holds a scope at any epoch.
2. *Contiguity.* An append whose `start` is above the scope's appended end fails with `Gap { expected }`. The log never has holes.
3. *Idempotence.* An append whose range overlaps bytes already accepted succeeds only if the overlapping bytes are identical, and then has no further effect. Otherwise it fails with `Conflict`. The log writer relies on this when an abandoned flush leader's batch is re-sent, which today's `StagedBatch` already does against the file.
4. *Framing.* `bytes` must be whole records in the `cascade-wal-format` frame, each with a valid CRC, a header LSN equal to its position, and a header epoch equal to the append's epoch. The last record of `bytes` must carry the group-end flag, so that every append ends on a mini-transaction boundary (S16). Otherwise the append fails with `BadFrame`. The log store checks only the frame and never interprets a payload. This catches a buggy writer at the boundary instead of on recovery.
5. *Monotonic durability.* The durable LSN never moves backwards, across restarts too, and always falls on a mini-transaction boundary: either every record of a mini-transaction is durable, or none is. Followers, the page store and recovery therefore never see half of a mini-transaction, such as two of the four records of a B-tree split.
6. *Acknowledged means durable.* Once `wait_durable` has returned `Ok` for an LSN, every record below it survives any crash that the deployment is designed to survive: a process or power failure on a single server with its disk intact, and the loss of N − W replicas in the managed service.
7. *Failure is final for the epoch.* After an I/O error on a flush, the log store reports `Failed` to every waiter and refuses further appends in that epoch, for **every scope on the failed stream**, because a frame holds extents of many scopes (S19). The writers must stop, and a new `open_writer` recovers after the stream has been recovered (Section 5.7), which is the architecture's rule that a sink failure stops the writer. A tenant that needs a smaller blast radius can be given its own stream.

#### 4.3.1. The quorum view and the replica view (S20)

The promises above describe what the log writer sees: on a single server the local log store, and in the managed service the quorum client, which presents N replicas as one `LogSink`. A single replica cannot keep all of them by itself, because it knows only what it has persisted, not what a write quorum has. Two rules close that gap, and the replica-level contract (`LogReplica`, owned by `cascade-storage-remote`'s design) must keep them:

- *Replicas serve only quorum-durable log.* Every append carries the writer's current durable LSN in `AppendMeta::writer_durable`, as Raft's commit index does. A replica's `LogSource` serves records only below the highest such value it has received. Without this, a page store reading from a replica could apply records that a failover later discards, and its pages would be built from a history that never happened.
- *A newer epoch may replace a non-durable tail.* A replica may hold bytes from an old epoch above the quorum's durable LSN. An append from a newer epoch at an LSN inside that tail truncates it and replaces it. The replica's own persisted end may therefore move backwards inside its non-durable region; the quorum's durable LSN never does.
- *Reopening a quorum.* The quorum client's `open_writer` reads the persisted ends of at least N − W + 1 replicas, so that it overlaps every write quorum, takes the longest log among them, replicates that suffix to a write quorum under the new epoch, and only then returns it as the durable LSN. The full algorithm belongs to the storage-remote design.

**Why `append` does not wait.** The log writer streams filled bytes to the sink as they become available and waits only when a commit needs durability. Separating the two lets the log store choose flush boundaries for fairness and batching across scopes (Section 5.4), and lets the quorum client pipeline appends to its replicas.

### 4.4. `LogSource`: anyone who reads the log

```rust
pub trait LogSource: Send + Sync {
    /// Returns durable records starting at `from`, up to about `max_bytes`.
    /// Always returns at least one whole record if any is durable at `from`,
    /// even if it is larger than `max_bytes`. Returns an empty chunk if
    /// `from` equals the durable LSN.
    fn read_log<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, from: Lsn, max_bytes: usize)
        -> LocalBoxFuture<'a, Result<LogChunk, StoreError>>;

    /// An ordered stream of durable chunks starting at `from`. It waits
    /// when it reaches the durable LSN, and ends with an error if the scope
    /// is dropped or `from` falls below the retained floor.
    fn subscribe(&self, ctx: &OpCtx<'_>, scope: ScopeId, from: Lsn) -> Box<dyn LogSubscription>;

    /// Records that `holder` no longer needs log below `lsn`. A holder's
    /// value only moves forward. A new holder must register at or above the
    /// current floor.
    fn set_retained_from<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId,
                             holder: RetentionHolder, lsn: Lsn)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;

    fn release_holder<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, holder: RetentionHolder)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;

    /// The lowest LSN that is still readable.
    fn retained_floor(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// For point-in-time restore: the start of the earliest append whose
    /// highest commit timestamp is above `ts`. Every commit at or below
    /// `ts` ends before the end of that append. The caller scans forward
    /// from the returned LSN with `cascade-wal-format` to find the exact
    /// commit boundary. Fails with `Truncated` if the answer is below the
    /// retained floor, in which case the archive answers instead.
    fn append_before_time<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, ts: Hlc)
        -> LocalBoxFuture<'a, Result<Lsn, StoreError>>;
}

pub trait LogSubscription {               // !Send
    fn next(&mut self) -> LocalBoxFuture<'_, Result<LogChunk, StoreError>>;
}
```

**Promises.**

1. *Only durable records.* Nothing above the durable LSN is ever returned, so a reader can apply everything it receives.
2. *Order and completeness.* Chunks are in LSN order with no gaps and no duplicates. A subscription's chunks are contiguous from `from`.
3. *Record boundaries.* `from` must be a record boundary that the reader learned from the log itself, such as the end of a previous chunk, a durable LSN, or a page LSN. A `from` inside a record fails with `NotRecordBoundary`.
4. *Retention.* The log store discards log only below the minimum over all registered holders. With no holder registered it discards nothing, so a misconfigured server keeps too much log rather than losing log that a page store still needs. A read below the floor fails with `Truncated { floor }`.
5. *Holders persist first.* A holder must make its own restart point durable before it moves its retention value past it. For example, the page store records its materialized LSN durably and only then reports it. Otherwise a holder that crashes in between restarts from an older point whose log is already gone.
6. *Holders cannot keep log forever.* Each holder has an owner and a lease. The log store reports each holder's lag and age, an operator or `cascade-cluster` can release a holder, and a holder that exceeds a configured size limit is invalidated, after which its reader must rebuild from the archive. A lost page store or a removed follower would otherwise keep all log above its last value until the disk fills, which is PostgreSQL's abandoned replication slot problem.

**Why the time lookup is not exact.** The log store never reads payloads, so it knows commit timestamps only per append, from `AppendMeta`. Because commit timestamps increase with commit-record LSN within a scope (WP2 D1), the commits at or below a timestamp form a prefix of the log, and the boundary lies inside the earliest append whose highest timestamp passes it. The caller, which can decode commit records, finds the exact point within that one append.

### 4.5. `PageSource`: compute reads pages

```rust
pub trait PageSource: Send + Sync {
    /// The page exactly as of `at_lsn`: every record for the page with an
    /// LSN below `at_lsn` is applied, and none at or above it.
    fn get_page<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, page: PageKey, at_lsn: Lsn)
        -> LocalBoxFuture<'a, Result<PageImage, StoreError>>;

    /// Every page on disk reflects the log up to here.
    fn materialized_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// The oldest `at_lsn` this page store can still serve for the scope.
    fn oldest_readable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// A reader promises it will not ask for pages below `lsn`. The page
    /// store may discard page versions below the minimum over all readers.
    fn set_read_floor<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId,
                          reader: ReaderId, lsn: Lsn)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;
}
```

**Promises.**

1. *Exact as of an LSN.* The returned image reflects exactly the scope's records for that page that end at or below `at_lsn`, and its page LSN is at or below `at_lsn`. Because `at_lsn` is a record boundary, this is the architecture's promise that the image reflects the records below `at_lsn`. The differential test in Section 10 checks this against replaying the log through `cascade-redo`.
2. *Never ahead of durability.* `at_lsn` above the durable LSN fails with `BeyondDurable { durable }`. No non-durable record is ever applied.
3. *Catch-up on demand.* If the page store has not yet ingested the log up to `at_lsn`, the call waits for it and applies the missing records for that page, instead of failing. A page store that lags slows only its own misses, never commits.
4. *Unwritten pages.* A page that no record below `at_lsn` has touched comes back zero-filled with `page_lsn` 0, which matches today's `CoreStorage` behavior for reads past the end of a file. For a branch, such a page is read from the parent as of the fork LSN instead.
5. *Old versions.* Any request at or above `oldest_readable_lsn` succeeds. A request below it succeeds if the page store still holds a version of that page old enough, and otherwise fails with `TooOld { oldest }`. It never returns a version that is newer than requested.

**Why a leader never sees `TooOld`.** A page store without page versions keeps only the newest image of each page, so `oldest_readable_lsn` equals its materialized LSN, and a request below it can be served only if the stored image's page LSN is at or below `at_lsn`. A leader's request always satisfies this, even when its durable LSN is stale: every change to a page goes through the leader's page cache, and a page the leader evicted receives no new records until the leader fetches it again. The newest stored version is therefore exactly the version the leader asked for. This argument holds as long as the system has no blind redo to pages outside the cache, which HLD v2 left as a later option. Followers and read replicas, by contrast, can lag behind the page store, so in the managed service they report their oldest applied LSN through `set_read_floor`, and the page store keeps page versions back to the minimum floor (Section 6.5).

The read floor and `TooOld` are a proposed addition to the architecture's `PageSource` sketch (Section 11, item 1). Without them, a follower that fills a miss "at its applied LSN" could receive a page that is newer than its snapshot.

### 4.6. Admin contracts: creating and attaching scopes

Creating a scope is not a side effect of writing to it, because that would hide a lifecycle decision inside the data path. Two small contracts make it explicit. `cascade-scope` and the local controller (on a single server) or `cascade-cluster` (in the managed service) call them.

```rust
pub trait LogScopes: Send + Sync {
    /// Creates an empty log for `scope`. Its durable LSN starts at 0, and
    /// `open_writer` with `initial` or higher may follow.
    fn create_scope<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, initial: Epoch, key: KeyRef)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;
    /// Fences every epoch and frees the scope's log once no holder remains.
    fn drop_scope<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;
}

pub trait PageScopes: Send + Sync {
    /// Starts materializing `scope` from a log source. With a lineage, the
    /// scope is a branch: a page it has not written is read from the parent
    /// as of the fork LSN.
    fn attach<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId, lineage: Option<Lineage>, key: KeyRef)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;
    fn detach<'a>(&'a self, ctx: &'a OpCtx<'_>, scope: ScopeId)
        -> LocalBoxFuture<'a, Result<(), StoreError>>;
}

pub struct Lineage { pub parent: ScopeId, pub fork_lsn: Lsn }
```

`Lineage` is the branching hook that the architecture's §4.12 asks for. In version 1, `attach` with a lineage returns `Unsupported`.

### 4.7. `KeyProvider`: keys for encryption at rest

```rust
pub struct KeyRef { pub key_id: KeyId }          // identifies a scope's key chain
pub struct KeyVersion(pub u32);

pub trait KeyProvider: Send + Sync {
    fn current<'a>(&'a self, ctx: &'a OpCtx<'_>, key: KeyRef)
        -> LocalBoxFuture<'a, Result<(KeyVersion, ScopeKey), KeyError>>;
    fn get<'a>(&'a self, ctx: &'a OpCtx<'_>, key: KeyRef, version: KeyVersion)
        -> LocalBoxFuture<'a, Result<ScopeKey, KeyError>>;
}
```

There are two planned implementations. A local key file, protected by a master key that the operator supplies, serves the open-source single server with no control plane. An adapter to an external key management service serves the managed service. `ScopeKey` zeroes its memory on drop and never appears in logs or debug output.

### 4.8. `BlockDevice`: the storage components' only access to disk

```rust
pub trait BlockDevice: Send + Sync {
    /// A handle for the calling core. File handles never cross cores.
    fn local(&self) -> Box<dyn BlockHandle>;
}

pub trait BlockHandle {                                  // !Send
    fn open(&self, ctx: &OpCtx<'_>, name: &FileName, create: bool) -> LocalBoxFuture<'_, Result<FileId, IoError>>;
    fn read_at(&self, ctx: &OpCtx<'_>, f: FileId, off: u64, buf: AlignedBuf) -> LocalBoxFuture<'_, Result<AlignedBuf, IoError>>;
    fn write_at(&self, ctx: &OpCtx<'_>, f: FileId, off: u64, buf: AlignedBuf) -> LocalBoxFuture<'_, Result<AlignedBuf, IoError>>;
    fn sync(&self, ctx: &OpCtx<'_>, f: FileId) -> LocalBoxFuture<'_, Result<(), IoError>>;   // data durability
    fn len(&self, f: FileId) -> LocalBoxFuture<'_, Result<u64, IoError>>;
    fn allocate(&self, ctx: &OpCtx<'_>, f: FileId, off: u64, len: u64) -> LocalBoxFuture<'_, Result<(), IoError>>;
    fn punch_hole(&self, ctx: &OpCtx<'_>, f: FileId, off: u64, len: u64) -> LocalBoxFuture<'_, Result<(), IoError>>;
    fn remove(&self, ctx: &OpCtx<'_>, name: &FileName) -> LocalBoxFuture<'_, Result<(), IoError>>;
    fn rename(&self, ctx: &OpCtx<'_>, from: &FileName, to: &FileName) -> LocalBoxFuture<'_, Result<(), IoError>>;  // atomic
    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<FileName>, IoError>>;
    fn sync_namespace(&self) -> LocalBoxFuture<'_, Result<(), IoError>>;    // makes creates, removes and renames durable
}
```

**Promises and rules.**

- *Alignment.* Offsets and lengths are multiples of 4 KiB, and buffers are `AlignedBuf`. The real implementation uses `O_DIRECT` for all files, the log included, so the Linux page cache is never involved. Today's WAL uses buffered I/O with `fdatasync`; moving it to `O_DIRECT` is what makes the shared, padded flush frames in Section 5 possible.
- *Crash semantics.* A write that has not been followed by a successful `sync` may be lost after a crash, in whole or per 4 KiB block, in any combination. A synced write survives. A rename is atomic and survives once `sync_namespace` returns. To replace a file atomically, the caller writes the new file, syncs it, renames it over the old one, and then calls `sync_namespace`.
- *A failed sync is permanent (S19).* After `sync` fails, the contents of everything written to that file since the last successful sync are unknown. The kernel may already have discarded the dirty data, and a later `sync` can report success without having written it, which is the "fsyncgate" problem PostgreSQL met in 2018. Reading the data back proves nothing, because the read may come from a volatile device cache. The caller must not retry the sync and trust the result. The implementation reports every later operation on that file as failed, and the fake models this by forgetting unsynced blocks after a failed sync.
- *Names, not paths.* `FileName` is a flat name inside the device's namespace. No caller computes a path, which keeps today's `CONTEXT.md` invariant that file paths and offsets never leave the storage layer.
- *Tenant tag on every I/O.* Every submission carries the tenant from `OpCtx`, which closes the §4.12 item for the storage components. The runtime adapter records it for per-database I/O accounting and, later, I/O limits.
- *The crash fake.* The fake keeps two images per file, synced and unsynced. `crash(seed)` keeps a random subset of the unsynced 4 KiB blocks and discards the rest, which is how the log store and page store test crash consistency at random cut points.

### 4.9. Errors

```rust
#[non_exhaustive]
pub enum StoreError {
    Fenced { current: Epoch },
    Gap { expected: Lsn },
    Conflict { at: Lsn },
    BadFrame { at: Lsn, reason: FrameError },
    BeyondDurable { durable: Lsn },
    Truncated { floor: Lsn },
    NotRecordBoundary { at: Lsn },
    TooOld { oldest: Lsn },
    UnknownScope(ScopeId),
    ScopeExists(ScopeId),
    Failed { scope: ScopeId },             // the scope's log failed in this epoch; reopen to recover
    Corruption { what: CorruptionSite },   // a checksum or decryption failure in durable data
    Io(IoError),
    Key(KeyError),
    Unavailable,                           // remote only: not enough replicas reachable
    DeadlineExceeded,
    Unsupported(&'static str),
}
```

`cascade-error` maps these to SQLSTATE codes. `Fenced`, `Failed` and `Io` on the commit path are "outcome unknown" failures, which poison the transaction as WP5 D9 describes.

### 4.10. Fakes and conformance suites

The contract crate provides, behind the `fake` feature:

- `MemLogStore`, which implements `LogSink`, `LogSource` and `LogScopes` in memory. It has a `crash()` method that discards everything not yet durable, and switches that inject delays, I/O errors and fencing.
- `MemPageStore`, which implements `PageSource` and `PageScopes` over any `LogSource`. It takes an injected redo function, so that the contract crate does not depend on `cascade-redo`; tests pass the real `cascade-redo::apply`.
- `MemBlockDevice` with the crash behavior in Section 4.8, and `MemKeyProvider`.

Behind the `conformance` feature, each contract has a suite written once as generic async functions over a harness trait that can create, crash and restart an instance. The suites cover every numbered promise above. They run against the fakes in this crate, against the local implementations, and later against the remote clients over the simulated network.

---

## 5. The Log Store: `cascade-logstore`

### 5.1. Responsibilities

The log store makes each scope's log durable and keeps it in order. It enforces epochs, serves log ranges and subscriptions, honors retention holders, and shares flushes fairly among scopes. On a single server it is the only log store (N = 1). In the managed service each instance is one replica, and the quorum client in `cascade-storage-remote` presents N of them as one `LogSink`.

### 5.2. On-disk layout

A log store instance owns one `BlockDevice` namespace with these files:

- **Segment files** (`seg-<stream>-<seq>`), each 64 MiB and preallocated. A segment holds flush frames from many scopes. A *stream* is a chain of segments that are written in order. An instance has one shared stream by default, and can give a heavy scope its own stream (Section 5.4).
- **The manifest** (`MANIFEST`, replaced by atomic rename), which records for each scope its current epoch, its key reference, its retention holders, the durable LSN as of the manifest, and a compact extent index (Section 5.3). It also records, per stream, the segment and frame at which recovery must start scanning.

A **flush frame** is the unit of one shared flush:

```text
frame header (64 bytes):
  magic u32 | version u16 | flags u16 | stream u32 | segment_seq u64 | frame_seq u64
  | incarnation u64 | payload_len u32 | extent_count u32 | header_crc u32 | reserved
extent, repeated extent_count times:
  scope u64 | epoch u64 | start_lsn u64 | len u32 | key_version u32 | kind u8 | reserved u8 x7
  | bytes[len]  (encrypted log records, or a control payload)
frame trailer: crc32 of everything above
padding to the next 4 KiB boundary
```

`frame_seq` increases by one for every frame in a stream, and `segment_seq` must equal the sequence number in the segment's file name. Recovery relies on both to tell a new frame from a stale frame left in a reused segment (Section 5.7). `incarnation` increases every time the log store opens, is recorded in the manifest before any frame is written, and makes encryption nonces unique even when a torn frame's position is written again after a crash (Section 7).

An extent's `kind` is either *records*, which carries log bytes for one scope, or *control*, which records an epoch change, a scope creation or a scope drop. Control extents make fencing durable: `open_writer` writes a control extent with the new epoch and returns only after that frame is synced.

Padding each frame to 4 KiB costs at most 4 KiB per flush, shared by every scope in the flush. Under load, frames are large and the padding is negligible. When the system is idle, flushes are rare.

### 5.3. In-memory state

For each open scope, the log store keeps a small record: its epoch, appended end, durable LSN, a queue of accepted but not yet flushed bytes, its waiters, and an *extent index*. The extent index maps LSN ranges to (stream, segment, offset) locations. It is stored as a list of runs, because consecutive extents of a busy scope are usually in consecutive frames, so a scope that has written gigabytes still needs only a few hundred entries. An idle scope holds no queue, no task and no timer, only this record (§2.4).

A recent-log cache keeps the last few megabytes of each active scope's durable log in memory, because followers catching up and the page store's subscription almost always read the tail.

### 5.4. Write path and fair flushing

1. `append` checks the epoch, contiguity and framing, copies the bytes into the scope's pending queue, and returns.
2. `wait_durable` registers a waker and, if no flush is scheduled, schedules one.
3. One **flusher task** per stream runs on a core chosen at boot, through the injected `Rt`. When a flush is due, it builds a frame from the pending queues using **deficit round robin**: each scope with pending bytes has a deficit counter that grows by a quantum (default 64 KiB) per flush, and it may contribute up to its counter. Bytes beyond its share stay queued for the next flush. The flusher cuts a scope's queue only at mini-transaction boundaries (records with the group-end flag), so every extent holds whole mini-transactions; a mini-transaction larger than the quantum is sent whole once the scope's counter allows it. Because frames are made durable whole, the durable LSN always lands on a group end, which is promise 5 in Section 4.3. Within one flush, scopes with waiting commits are served before scopes that only have streamed bytes with no waiter.
4. The flusher encrypts each extent with the scope's current key, writes the frame, and syncs. It then advances each included scope's durable LSN and wakes its waiters. Each stream has at most one frame written but not yet synced, which is what lets recovery treat a bad frame at the end of a stream as a torn write rather than corruption.
5. Group commit timing: the flusher waits up to a short window (today's `COMMIT_DELAY`, 300 µs) for more waiters when the frame is small, and flushes immediately when the frame reaches its size cap (default 1 MiB).
6. **Limits.** Pending bytes are bounded per scope and per stream, and `append` waits when its scope's bound is reached, which pushes back on the log writer instead of growing memory without limit when the disk is slow. A mini-transaction may be at most 1 MiB, the frame size cap, and the log writer contract enforces it, so large operations are split into several mini-transactions that each leave the pages consistent.
7. **Disk space.** Space is reserved for control frames, which data appends cannot use. When free space falls below a low-water mark, new data appends fail with a clear error while fencing, creating and dropping scopes still work, because dropping a scope is how an operator frees space.

This gives the behavior that the architecture's §2.4 asks for. A quiet scope's commit waits for at most one flush, and the cost of a heavy scope's large writes falls on that scope, whose excess is carried into later flushes. The log store reports bytes per scope through `UsageSink`. When a scope stays above its share for a configured time, the log store moves it to its own stream, so its flushes no longer share frames with anyone. In the managed service, `cascade-cluster` can also move its log group to other log store nodes.

The pending queues and the durable-LSN publication are shared structures on the commit path, so they are model-checked with `loom` (Rule 11).

### 5.5. Read path and subscriptions

`read_log` serves from the recent-log cache when it can, and otherwise looks up the extent index, reads the frames through the calling core's `BlockHandle`, checks the frame CRC, decrypts, and returns whole records. Every extent holds whole mini-transactions (Section 5.4), so an extent boundary is always a record boundary and a group boundary. A read that starts inside an extent finds its record boundaries with `cascade-wal-format`'s frame walker.

A subscription is a cursor plus a registration for durable-LSN advances of its scope. It does not hold a task of its own. Its `next` call reads what is durable and, at the end, waits for the next advance.

### 5.6. Retention and segment reuse

A segment can be reused when every extent in it is below its scope's retained floor, or belongs to a dropped scope, **and** a manifest that no longer references the segment is durable, **and** no read of the segment is still in flight (tracked with a per-segment reader count). Every read also checks the extent header it reads (scope, epoch, start LSN and frame sequence) against what the extent index promised, and fails with `Truncated` on a mismatch, so a read that races with reuse returns a clean error instead of another scope's log. A per-segment count of live extents makes this check cheap. Reuse renames the oldest free segment to the next sequence number and keeps its preallocation. The rule in today's `recycle.rs`, "strictly below the frontier, whole windows only", carries over at segment granularity, together with its boundary tests.

`append_before_time` uses a sparse per-scope index of (append start LSN, highest commit timestamp so far), built from `AppendMeta::max_commit_ts` and stored with the extent index.

### 5.7. Recovery

If a stream's sync has failed (S19), the stream is recovered by this same procedure, from the last durable manifest position, with nothing written since the last successful sync trusted. On a single server this is a restart of the log store. In the managed service, the replica instead drops out of its quorums and repairs from its peers.

On restart the log store reads the manifest, increments its incarnation, writes the manifest back, and then scans each stream forward from the recorded position. A frame is accepted only if its header and trailer CRCs are valid, its `segment_seq` matches the segment, and its `frame_seq` is exactly one more than the previous frame's. The scan stops at the first frame that fails these checks:

- If nothing valid follows it, it is the end of the stream: either never-written space, a stale frame from the segment's earlier use, or the torn last flush, which was never acknowledged. The store resumes writing at that position.
- If a valid frame with the next expected sequence number follows it, the bad frame was synced and later damaged. That is corruption, and the store refuses to open the scopes that have extents in or after it. Because a damaged header no longer gives the frame's length, the scan looks for the successor at every 4 KiB boundary up to the end of the following segment.

This is the same distinction today's WAL makes between `TornTail` and `Corruption`, extended to reused segments, which today's single growing log file does not have.

**The ambiguous last frame.** If the very last frame was synced and acknowledged, and later damaged on disk, nothing valid follows it, so it looks exactly like a torn flush, and truncating it would silently lose acknowledged commits. One copy cannot fully resolve this, but two measures narrow it. On a clean shutdown, the manifest records the exact end of the log, so after a clean shutdown any bad frame before that end is corruption. And every frame header records the sequence number of the last frame whose commits had been acknowledged when it was built, so after an unclean shutdown only the final frame can be ambiguous. The remaining risk on a single server is covered by the archive. In the managed service a replica that finds a bad tail repairs it from its peers instead of truncating it.

The scan rebuilds each scope's extent index, epoch and durable LSN, then writes a fresh manifest. Recovery time is bounded by how far the manifest lags, not by the size of the log, because the manifest is rewritten every few hundred megabytes of log.

---

## 6. The Page Store: `cascade-pagestore`

### 6.1. Responsibilities

The page store subscribes to each attached scope's log, applies records to pages with `cascade-redo`, writes pages to disk in the background, serves pages as of an LSN, and reports how far it has written pages as its retention holder. Commits never wait for it.

### 6.2. Ingest

For each attached scope there is one subscription, driven from a shared scheduler rather than a task per scope. Each chunk is split into records with `cascade-wal-format`. Page redo records are appended to a per-page **redo chain** held in memory, keyed by `PageKey`. Records that change no page, such as commits and heartbeats, only advance the scope's ingested LSN.

Redo chain memory is bounded. When the bound is reached, ingest pauses and the log simply stays in the log store longer. A leader's miss at a recent LSN then waits for ingest to catch up, so the bound is set with that latency in mind, and the page store reports its lag.

### 6.3. Materialization

A background materializer picks pages with redo chains, oldest first, reads each base image (from the page store's own small cache or from disk), applies the chain with `cascade-redo`, and writes the result. Writes go through a **doublewrite area**: a batch of pages is written to a fixed region and synced, then written to their home locations and synced. If a crash tears a page at its home location, recovery restores it from the doublewrite area and reapplies the log, which redo's idempotence makes safe.

`materialized_lsn` is the lowest LSN that still has an unwritten record in any chain, or the ingested LSN when every chain is empty. After each batch the page store, in this order, syncs the pages, durably records the new materialized LSN, and only then calls `set_retained_from(scope, PageStore { instance }, materialized_lsn)` on the log store (Section 4.4, promise 5). Log is therefore released only when the pages that need it, and the page store's own restart point, are on disk. Restored doublewrite copies are checked by CRC and used only where the home copy is bad.

**Page files.** Each scope has a directory-like prefix in the page store's namespace, with one file per segment, the same file-per-segment layout as today's `CoreStorage`. Each page keeps the 4-byte CRC at its start, as today. Encryption at rest adds a key version, a 12-byte nonce and a 16-byte authentication tag to each page, so the page format owner, `cascade-page`, must reserve space for them (Section 11, item 3).

### 6.4. Serving reads

`get_page(scope, page, at_lsn)` checks that `at_lsn` is at or below the durable LSN, waits until ingest has reached `at_lsn`, and takes the base image. Reads and writes of the same page are serialized: while a doublewrite batch is in flight, reads of its pages are served from the batch's memory, so a read never sees a page half overwritten by the materializer. If the base image's page LSN is above `at_lsn`, the image is newer than requested, and without page versions the call fails with `TooOld`. Otherwise it applies the page's chain records that start at or above the image's page LSN and end at or below `at_lsn`, and returns the result. It does not write that result: materialization stays the background task's job, so a read never changes disk state.

### 6.5. Page versions (managed service, Phase F)

With followers and read replicas, a request can be below the latest materialized image. The plan is a per-scope **version log**: before the materializer overwrites a page whose old image may still be needed (the read floor is below the new image's LSN), it appends the old image to the version log. A request below the current image finds the newest older image at or below `at_lsn` and applies chain records from there. Version log space is released as readers raise their floors. None of this is built for the single server, where the read floor always equals the durable LSN.

### 6.6. Recovery

On restart the page store reads each scope's recorded materialized LSN, repairs torn pages from the doublewrite area, and resubscribes from that LSN. Records already applied to a page on disk are skipped by redo's LSN check.

---

## 7. Encryption at Rest

- **Where.** The log store encrypts each extent, and the page store encrypts each page. Bytes are plaintext inside the contracts, so the log writer, the page cache and every component above them are unaffected. Between nodes, `cascade-net-api` provides TLS.
- **Algorithm.** AES-256-GCM, which must never reuse a nonce under one key. For log extents, the nonce is derived from (incarnation, stream, frame sequence, extent index). The incarnation is what keeps it unique when a torn frame's position is rewritten after a crash with different contents. For pages, the nonce is derived from (incarnation, a per-incarnation write counter), and the nonce and tag are stored in the page. The page store starts a new incarnation before the write counter could wrap. Each page's CRC covers the ciphertext and header rather than the plaintext, so background scrubbing can detect corruption without the key; the authentication tag detects tampering and corruption on every decrypting read.
- **Rotation.** New writes use the scope's current key version, and each extent and page records the version it was written with, so old data stays readable. The log rotates naturally as segments are reused. The page store re-encrypts lazily when it rewrites a page, and a background sweep can force it. A key version may be retired only when the log store, the page store and the archive all report that nothing they hold still uses it; retiring it earlier makes that data unreadable. **Crypto-shredding on drop (S21).** Dropping a scope schedules its key for destruction at the end of a recovery window, and destroying the key makes every copy of the scope's data unreadable: the log store's segments, the page store's pages and the archive's objects, including copies that are hard to find and erase one by one. This is the reliable way to honor a departing tenant's deletion request. The window exists because an accidental `DROP DATABASE` should be recoverable from the archive for a while. The default window is the archive's restore window in the managed service (7 days unless configured). On a single server without an archive there is nothing to restore from, so the key is destroyed as soon as the drop completes. An operator can shorten the window to zero for an explicit secure delete. Destruction is irreversible and is recorded in the metadata store, and once it happens the scope's remaining files can be removed lazily, since they are unreadable.
- **The single server.** The local key file implementation of `KeyProvider` needs no control plane. Encryption is optional in the open-source edition and on by default in the managed service.

---

## 8. Threads and Cores

- The log store and the page store are each one `Send + Sync` object per process, shared by all cores.
- Their background work (log flushers, page ingest and materialization) runs as tasks on cores chosen at boot, spawned through the injected `Rt`. No component spawns threads or opens files directly (Rule 4).
- Every core that performs I/O uses its own `BlockHandle`, obtained from `BlockDevice::local`, so file handles stay on their reactor, as `CoreStorage` and `WalHandle` require today.
- On a single server, compute calls the log store and page store in process. A call from a compute core runs the validation and copy on that core, and the flush happens on the flusher's core, which wakes the caller across cores through the waker.

### 8.1. What happens when the log fails

This sequence crosses several components. The storage layer owns only step 1; the rest is owned by the log writer, the page cache, the scope host and the transaction manager, and is recorded here so the whole sequence is in one place. Their detailed designs must keep it.

A mini-transaction applies its changes to cached pages right after appending its records to the log writer, without waiting for durability. Only a commit waits for durability, and a page cannot be evicted until its changes are durable. So when a flush fails, pages in the cache already hold changes that may never become durable. They are not reverted one by one, because other transactions may have changed the same pages since. Instead:

1. **The sink fails the scope.** The log store reports `Failed` for the epoch to every waiter and refuses further appends in that epoch (Section 4.3, promise 7). A flush failure affects every scope on the failed stream, so steps 2 to 5 run for each of them.
2. **Fence first.** The log writer marks the scope's current epoch on this node as failed, at once. Every transaction context and every page-cache access carries the scope's epoch, so from this moment every operation of a transaction bound to the failed epoch returns `Poisoned` before it touches a page, and the page cache refuses pages of that scope for the failed epoch. This applies to every open write transaction in the scope, not only to those waiting on a commit.
3. **Then discard.** The scope host drops the scope's cached pages, status table, locks and closed timestamp on this node.
4. **Reopen.** The scope is opened again with a higher epoch, by the same node on a single server or by a follower in the managed service. `open_writer` returns the durable LSN, and pages are fetched again from the page store as of that LSN. Because of S16, the durable log ends on a mini-transaction boundary.
5. **Roll back in-flight transactions** from the durable log: the index-organized engine through its undo records, the heap through the status table.

**What clients see.** A transaction that had not reached `COMMIT` is aborted, since no commit record for it can be durable, and every later statement in it fails, like PostgreSQL's "current transaction is aborted" (SQLSTATE 25P02). A transaction that was waiting on `COMMIT` gets "outcome unknown", because its commit record may have become durable before the failure was noticed; the durable log decides.

**Why fence before discarding.** If the cache refetched a page before a transaction was poisoned, that transaction could read the page again and see its own earlier write disappear. The epoch check on every access closes that window, and it costs no more than reading a value that is almost always already in the CPU cache.

---

## 9. Migration from the Current Code

Each step is a small series of commits that keeps the workspace building and the existing tests passing.

| Step | What changes | What is reused | Tests |
| :--- | :--- | :--- | :--- |
| **M0. Shared types** | Add `ScopeId`, `Epoch`, `NodeId`, `TenantContext` and `OpCtx` to `cascade-types`. Add `TxnId` as the new name for `Xid`, with `Xid` kept as a deprecated alias for now. Add a minimal `cascade-rt-api` with the parts the storage components need (`spawn_local`, `now`, `sleep`), implemented by the runtime; the rest of that contract arrives with Phase A. | Everything | Existing suites unchanged |
| **M1. Split the record format** | Move `cascade-wal/src/format.rs` into a new `cascade-wal-format` crate, and expose a frame walker (header, length, LSN, epoch, CRC, group-end flag) that does not decode payloads. Define format version 3: the first of the header's three reserved bytes becomes a flags byte, whose bit 0 is the group-end flag (S16). The header length stays 36 bytes. Version 2 records still decode, each treated as a group of one. `cascade-wal` re-exports the moved items so callers do not change yet. | The v2 format and its golden-byte tests | v2 golden tests kept; new v3 golden tests for the flag |
| **M2. Contracts** | Create `cascade-storage-api` with the contracts in Section 4, the fakes and the conformance suites. Nothing calls it yet. | None | The fakes pass every conformance suite |
| **M3. Block device** | Implement `BlockDevice` on glommio's `DmaFile`, taken from `CoreStorage` (file opening, `O_DIRECT` reads and writes) and `WalHandle` (the watchdog and retry around each operation). It lives in a `blockdev` module of `cascade-storage` until the runtime adapter is split out in Phase A, then moves there. | `CoreStorage` I/O code, `WalHandle` watchdog | Block-device conformance against the fake and the real device |
| **M4. Log store** | Build `cascade-logstore` on `BlockDevice` (Section 5). | Torn-tail and corruption classification from `cascade-wal`, the group-commit window, and the recycle boundary rule from `recycle.rs` | `LogSink` and `LogSource` conformance, crash tests at random cut points, a noisy-neighbor test, `loom` tests for the pending queue |
| **M5. Log writer onto `LogSink`** | Change `WalWriter`'s flush leader to send its staged batch with `LogSink::append` and wait with `wait_durable`, instead of writing a file through `WalHandle`. Reservation, fill and the filled watermark do not change. A mini-transaction reserves its whole group as one range and sets the group-end flag on its last record, so the filled watermark, and therefore every batch sent to the sink, ends on a group end. The epoch comes from `open_writer` instead of `new_in_epoch`. | All of `WalWriter` except the file I/O | The existing 900 lines of writer tests, run against `MemLogStore` |
| **M6. Runtime boot and recovery** | Boot creates the local log store and calls `open_writer` for each database. Recovery reads the log with `LogSource::read_log` instead of `DebugWalReader`. The clean marker, `truncate_log` and `log_len` are removed, because the log store handles its own tail at open. Data directories written by today's single-file WAL are not converted: there are no production users, so a development data directory is recreated. | `recovery.rs`'s replay walk | Runtime recovery and commit tests, with the log on the local log store |
| **M7. Redo and page store** | Requires `cascade-redo` from Phase B, with at least the heap-insert family that the code emits today. Build `cascade-pagestore` (Section 6). | `CoreStorage`'s page file layout and CRC handling | `PageSource` conformance, a differential test against replaying the log, crash tests |
| **M8. Page cache onto `PageSource`** | The buffer pool's miss path calls `get_page` at the durable LSN. Its flush, write-back and dirty-eviction paths are removed, and eviction follows the durable-LSN rule. `WalDurability` becomes the `DurableLsn` contract. The runtime's `checkpointer.rs` and `recycler.rs` are removed. | The pool's page table, pin protocol and NUMA frames | Pool tests minus the write-back tests, plus durable-LSN eviction tests |
| **M9. Remove the old trait** | Delete the `PageStore` trait and `CoreStorage`, and the `cascade-storage` crate once the block device has moved to the runtime adapter. Update `CONTEXT.md` files and the storage docs in `crates/02-storage/cascade-storage/AI/`. | None | Full workspace suite |

After M9, the single server runs on the target storage architecture: compute writes only log, and only the page store writes pages. The archive, the temporary store and the network clients follow in their own phases without contract changes.

**Order relative to the phases document.** M0 is a slice of Phase A, M1 is a slice of Phase B, and M2 to M9 are Phase C. M7 depends on `cascade-redo`, so Phase B's redo work must reach the heap-insert family before M7 starts. M2 to M6 can proceed in parallel with it.

**Unpushed work.** The transaction manager and heap-insert work on Kenneth's machine touch `cascade-wal` and the runtime. If they are not yet on `main`, they should be pushed before M1, because M1 and M5 move and change the same files.

---

## 10. Testing

- **Conformance suites** for every contract, run against the fakes, the local implementations and, later, the remote clients (Rule 6).
- **Crash tests.** The log store and page store run on `MemBlockDevice` and are crashed at random points with a seeded random generator. After each crash they must recover with no acknowledged record lost and no unacknowledged record half-present.
- **Differential page test.** For random workloads, every page the page store serves at an LSN must equal the page built by replaying the same log from empty through `cascade-redo`.
- **Noisy-neighbor test.** One scope appends large batches continuously while many small scopes commit. The small scopes' commit latency must stay within a bound, which is the test that architecture v0.10.1 added for fair log sharing.
- **Model checking** of the log store's pending queues and durable-LSN publication with `loom` (Rule 11).
- **Simulation.** From Phase C onward, both components run under the deterministic simulator with injected I/O delays and failures.

---

## 11. Changes Proposed to the Architecture, and Open Questions

These need Kenneth's confirmation. Items 1 and 2 change contracts that the architecture sketches.

1. **Read floor in `PageSource`.** Add `oldest_readable_lsn`, `set_read_floor` and the `TooOld` error (Section 4.5), and add a §4.12 row for page versions owned by `cascade-storage-api` and `cascade-pagestore`.
2. **Admin contracts and extra methods.** Add `LogScopes` and `PageScopes` (Section 4.6), and `release_holder`, `retained_floor` and `append_before_time` on `LogSource`. Add `AppendMeta` to `append`. These are additions, not changes to what the sketches already promise.
3. **Page format space for encryption.** `cascade-page` must reserve room for a key version, a 12-byte nonce and a 16-byte authentication tag in every page, about 32 bytes. This is a format decision and should be made before the first page store writes pages.
4. **Mini-transaction group boundaries in the record format.** Decided: adopted as S16 (Kenneth, 2026-11-22). The group-end flag is added in format version 3 at step M1.
5. **Segment and frame sizes** (64 MiB segments, 1 MiB frame cap, 64 KiB fairness quantum) are starting values to be tuned by benchmark.
6. **Doublewrite versus out-of-place page writes.** Doublewrite is simple and proven (InnoDB), at the cost of writing each page twice in the background. An out-of-place, log-structured page store would avoid the double write and give page versions for free, at the cost of a page map and compaction. This design chooses doublewrite for version 1 and keeps the choice internal to the page store.
7. **Correct the page LSN wording elsewhere (S17).** Done in v0.5 for this document's §12, which replaces HLD v2. WP1's idempotence rule (in the repository) and the architecture's §4.4 still need the same correction.
8. **Crypto-shredding.** Decided as S21: the key is destroyed after a recovery window (Section 7).

---

## 12. Storage Foundations (taken over from HLD v2)

These are the system-wide rules that HLD v2 fixed for the log, redo, the page cache and recovery. They apply to every component that writes or reads pages and log, not only to L2, and this document is now where they live. The architecture's overrides and this document's decisions (S16 to S21) are applied.

### 12.1. Principles

1. **One writer per scope.** All write ordering, LSN allocation and conflict detection are local to a scope. There are no distributed write transactions; DDL across scopes, needed only once a database is split, is the single narrow exception.
2. **The log is the only durable write on the commit path.** Pages are a cache of the log. Nothing else (status tables, undo, page images) is made durable separately.
3. **Two orders, two clocks.** LSNs order log bytes, and HLC timestamps order commits and reads. They are never mixed.
4. **One read rule** (WP2 §2.2).
5. **Engines differ only below the access API.** Both engines share the log, the page cache, the status table, snapshots and the page store.
6. **Redo is per page and deterministic.** The page store can rebuild any page from a base image plus redo, with no access to other pages or to engine state.
7. **No global pauses.** No checkpoint, no global vacuum freeze, no stop-the-world. Background work is incremental and per page or per segment.

### 12.2. Scopes, epochs and fencing

- A writing scope is a whole database, one table, or one table shard. In version 1 every database is one scope, which also holds its catalog (architecture D10).
- Each scope has one log, one LSN space, one durable LSN, one closed timestamp, one transaction ID counter, one status table, one encryption key, and its own pages in the page store.
- A write transaction writes to exactly one scope, and the engine rejects a write that targets a second one. Temporary tables do not count (architecture §4.6).
- Ownership is held under an epoch. A handoff raises the epoch in the metadata store, and the log store rejects log from an older epoch (`LogSink` promise 1).
- *Invariants.* At most one node can make log durable for a scope at any epoch. Every record of a scope belongs to exactly one epoch, and epochs only increase along the LSN. A transaction other than cross-scope DDL never has records in two scopes' logs.
- *Open questions.* The lifecycle of splitting, merging and moving scopes; the shard key and partitioning scheme. Both belong to the sharding design.

### 12.3. The log and LSNs

- An LSN is a 64-bit byte offset into one scope's log, which only grows and never resets. A record's LSN is its start offset, and the next record starts where the previous one ends.
- LSNs are reserved with one shared atomic `fetch_add` per scope, by the log writer (`cascade-log`). Per-core block reservation is an optimization to consider only if profiling shows contention.
- The durable LSN is the end of the contiguous durable prefix of the log: everything below it is durable. Holes left by unfinished copies block it until they are filled. Since S16 it always falls on a mini-transaction boundary.
- Physical storage is the log store's shared segment files (Section 5). HLD v2's one-segment-file-per-scope layout is replaced.
- Every record carries its LSN, length, type, format version, scope epoch, transaction ID (or none), a group-end flag (S16) and a checksum (WP1 F3, format version 3). Page redo records also name the page or pages they change.
- Record families: page redo (per page, physiological); transaction records (optional begin, commit, abort, and prepare, commit-prepared and abort-prepared for DDL across scopes); undo payloads in undo pages; compensation records during rollback; closed-timestamp heartbeats; HLC high-water records; and reserved types for bulk load and for an unlogged or temporary marker. Records carry full row images for change data capture (architecture §4.2).
- The log is encrypted at rest with the scope's key (Section 7).
- *Invariants.* LSNs are unique and strictly increasing within a scope, and record N lies entirely within [N, N + length). A record becomes visible to readers of the log only once it is inside the durable prefix. No record refers to a page in another scope. Redo records are self-contained per page (§12.4).

### 12.4. The redo contract

`cascade-redo` owns the apply functions, shared by leaders, followers and page stores (architecture §4.2). This contract is a one-way door:

1. **Self-contained per page.** A redo record names one page, or a small fixed set for a B-tree structure change, and applies using only that page image and the record.
2. **Deterministic.** The same page image and the same record always produce the same bytes: no clocks, no randomness, no reads of other pages.
3. **Versioned.** Each record has a type and a version, and each page has a format version. Every component must keep applying every version it has ever accepted.
4. **Idempotent by LSN (S17).** A record is applied only if its start LSN is at or above the page LSN, which is the end LSN of the last record applied. Otherwise applying it does nothing.

The contract is designed together with the row header and index entry formats (WP1, `components/cascade-iot.md`, `components/cascade-heap.md`, `components/cascade-index.md`), because those formats decide what each redo record carries. *Open question:* whether to allow blind redo to pages that are not in the compute cache, and for which operations. Allowing it would invalidate the argument that a leader never sees `TooOld` (Section 4.5), so it needs a design of its own.

### 12.5. Page cache durability rules

`cascade-pagecache` owns the cache. These rules are what the storage layer relies on:

- A page's LSN is the end LSN of the last record applied to it, committed or not (S17).
- Pages change only through mini-transactions: append the group of records to the log writer, then apply them to the latched pages (architecture §4.4). Pages are never written back to storage.
- **A page is never evicted while its page LSN is above the durable LSN**, because the page store may not have its changes yet.
- An evicted page is fetched again at the scope's durable LSN on a leader, or at the applied LSN on a follower or read replica.
- The cache is shared per NUMA node and reachable by every core of the scope's core set (architecture D1), which replaces HLD v2's one cache per core.
- *Invariants.* For every cached page P, every record for P that ends at or below P's page LSN has been applied, and none above. A page fetched at LSN L reflects exactly the records for that page that end at or below L. Dropping any page whose page LSN is at or below the durable LSN loses nothing.
- *Open questions.* Latching for B-tree splits and merges across pages; memory pressure when the durable LSN lags, and the backpressure it puts on writers.

### 12.6. Durability, failover and recovery without checkpoints

- Durability follows the log store design: local flush on a single server, a write quorum of log stores in the managed service (architecture §2.2, §7).
- Pages never need replay on compute. The page store serves them as of the durable LSN.
- Transaction outcomes come from the log: a transaction with a commit or abort record is decided, and any other was in flight and is rolled back (index-organized engine through undo and compensation records; the heap needs no page work).
- In the managed service, warm followers maintain the status table and the in-flight set from the applied log, so promotion needs no log scan (architecture §4.5, §6). On a single-server restart, the transaction manager rebuilds that state from the log; how to bound that scan is an open question for WP2 and WP11.
- When the log fails while the scope is running, Section 8.1 applies.

### 12.7. Work packages

| WP | Detailed design | Where |
| :--- | :--- | :--- |
| WP1 | Record contract, row header, index entry, key encoding (accepted) | Repository, `storage-wp1-formats.md`; format version 3 adds the group-end flag (step M1) |
| WP2 | Log writer and transaction manager (accepted) | `storage-wp2-txn-manager.md` |
| WP3 | Page cache | To be written; must keep §12.5 |
| WP4 | Log store and page store | This document, Sections 4 to 7 |
| WP5 | Table access API (draft) | `storage-wp5-table-access-api.md` |
| WP6 | Index-organized engine and undo | `components/cascade-iot.md` |
| WP7 | Heap engine and vacuum | `components/cascade-heap.md` |
| WP8 | Secondary indexes | `components/cascade-index.md` |
| WP10 | Catalog and schema | `components/cascade-catalog.md` |
| WP9 | Snapshots and isolation | `storage-wp2-txn-manager.md` §2.3 |
| WP11 | Recovery and failover | To be written, with `cascade-scope` and `cascade-failover`; must keep §12.6 and Section 8.1 |

---

## Appendix A. Review of v0.1

Kenneth asked for a review of v0.1 for correctness, consistency and readability. v0.2 fixes these findings.

**Correctness.**

1. *Torn frames and encryption nonces.* After a crash, the log store rewrites the position of a torn frame with different contents. v0.1 derived nonces from the frame's position alone, so the same nonce could encrypt two different plaintexts, which breaks AES-GCM completely. v0.2 adds an incarnation number, increased on every open, to the frame header and to every nonce (Sections 5.2, 7).
2. *Stale frames in reused segments.* v0.1's recovery could read frames left over from a segment's earlier use as if they were new. v0.2 requires each frame's segment and frame sequence numbers to continue the stream, and allows only one unsynced frame per stream, so a torn write and corruption can be told apart (Sections 5.2, 5.4, 5.7).
3. *Extents and fair flushing.* v0.1 claimed that every extent is one whole append, but fair flushing splits a heavy scope's queue across flushes. v0.2 states that the flusher splits only at record boundaries, so every extent still holds whole records (Sections 5.4, 5.5).
4. *`TooOld` on a single server.* v0.1 said a single server never needs page versions because the leader asks at the durable LSN. That missed the case where the leader's durable LSN is stale and the page store has already written a newer image. v0.2 explains why the leader still gets the right page (every change to a page passes through the leader's cache, so an evicted page cannot change until it is fetched again), and has `get_page` fail with `TooOld` only when the stored image is genuinely newer than requested (Sections 1.2, 4.5, 6.4).
5. *Repeated `open_writer`.* v0.1 rejected a second open with the same epoch, which would make a retry after a lost reply fail. v0.2 treats it as a retry, relies on the metadata store for epoch uniqueness, and states that opening discards unflushed bytes from older epochs (Section 4.3).
6. *The timestamp lookup.* v0.1's `lsn_at_time` promised an exact LSN, which the log store cannot compute without reading payloads. v0.2 renames it `append_before_time`, returns the append that contains the boundary, and leaves the exact point to the caller (Section 4.4).
7. *Missing prerequisites.* The log store and page store depend on `cascade-rt-api`, which does not exist yet. M0 now adds a minimal version of it. M6 states that development data directories in today's WAL format are not converted.

**Consistency.** The summary rows S3, S7 and S9 now match Sections 4.1, 4.4 and 4.5. Section 11 now uses the renamed method and lists the full encryption overhead per page (key version, nonce and tag).

**Readability.** The reasons behind the `TooOld` rule and the inexact time lookup now have their own short explanations instead of being implied by the promises.

**Changes in v0.3.** Decision S16 makes the mini-transaction the unit of durability. It touches `LogSink` promises 4 and 5 (Section 4.3), the flusher (Section 5.4), the read path (Section 5.5), steps M1 and M5, and closes Section 11 item 4. Section 8.1 records the log-failure sequence: fence the scope's epoch first, then discard and reopen.

---

## Appendix B. Design Alternative: Aurora-Style Out-of-Order Log Delivery

This appendix records an alternative to the contiguous log delivery in Sections 4.3 and 5.4, so that the choice is explicit and can be revisited.

### B.1. What both designs share

In both designs, pages are built only from log that is complete and durable. Aurora's storage nodes merge records into pages only up to the point where their log has no holes. Our page store applies nothing above the scope's durable LSN, and the log store returns only durable records through `LogSource`. Commit latency has the same lower bound in both: a commit cannot be durable until every LSN before it is durable.

### B.2. How Aurora delivers the log

The Aurora database instance sends log records to the storage nodes as soon as it produces them, without waiting for earlier LSNs to be filled. The volume is divided into slices of about 10 GB, and each slice is replicated on six storage nodes, each of which holds both the log and the pages for its slice. A storage node therefore receives only the records for its own pages, so gaps in the LSN sequence are normal for it. Each record carries a back-link to the previous record for the same slice. A storage node accepts records out of order, buffers them, gossips with its peers to fill holes, and merges records into pages only up to the point where its chain of back-links has no holes. The database instance computes the durable point from the quorum acknowledgements it receives.

### B.3. How this design delivers the log

The log writer sends only the contiguous prefix of completely filled records (the filled watermark), and `LogSink` rejects an append that would leave a gap (Section 4.3, promise 2). The log store keeps each scope's whole log in order. A separate page store pulls only durable log from it. This shape is closer to SQL Server Hyperscale and Neon, which pair a log landing service with separate page servers, than to Aurora.

### B.4. Comparison

| | Aurora-style out-of-order delivery | Contiguous delivery (this design) |
| :--- | :--- | :--- |
| Where gaps are handled | On every storage node, by buffering and gossip | On the compute node, by sending only the filled prefix |
| What a slow record holds back | Nothing on the wire; only the durable point waits for it | Sending of the records behind it, for as long as the copy into memory takes, which is microseconds against a network round trip of hundreds of microseconds |
| Pipelining | Natural, since records are sent as produced | Kept, because `append` does not wait for acknowledgement, so contiguous batches are sent back to back |
| Record format | Needs a per-slice back-link in every record | No addition; records need only their LSN and length |
| Storage node complexity | Out-of-order buffers, per-slice completeness tracking and gossip repair on the normal path | An ordered stream per replica, with resend from the last acknowledgement on reconnect. Repair from peers is needed only when a replica falls far behind |
| Coupling of log and pages | Log and pages for a slice live on the same nodes | Log store and page store are separate, so each is sized and scaled for its own job: commit latency for the log, read capacity for pages |
| Single server | Would need the same gap machinery locally, or a separate code path | The same components run in process with no extra machinery |

### B.5. Decision and how to revisit it

Version 1 uses contiguous delivery. Its latency cost is small, because the hole on the compute node lasts only as long as a memory copy, and it keeps the log store, the quorum client and the record format simpler.

If measurements later show that contiguous delivery limits throughput or tail latency, the change to the contract is small. Promise 2 of `LogSink` would be relaxed so that a log store may accept and hold ranges beyond its contiguous end, while the durable LSN remains a contiguous prefix and `LogSource` still returns only durable records in order. The log writer could then send filled records without waiting for earlier reservations. Nothing above L3 would change. Sharding a scope's log across storage nodes by page range, which is what makes gaps unavoidable in Aurora, would be a larger change, because it would merge the log store and page store roles again.

---

## Appendix C. Changes in v0.4

v0.4 addresses the eighteen findings of `reviews/storage-layer-v0.3-adversarial-review.md`:

| Finding | Change in v0.4 |
| :--- | :--- |
| 1. Page LSN off by one | S17: the page LSN is the end LSN everywhere; §4.2, §4.5 and §6.4 use it consistently; Section 11 item 7 lists the other documents to correct |
| 2. Failed sync not permanent | S19 and the `BlockDevice` crash semantics (§4.8); stream recovery in §5.7 |
| 3. Fence races with an in-flight frame | §4.3 promise 1: `open_writer` is serialized with flushes |
| 4. Replicas and non-durable log | S20 and §4.3.1: `writer_durable` on appends, replica reads bounded by it, tail replacement by a newer epoch, the quorum reopen rule |
| 5. Holder released before its restart point is durable | §4.4 promise 5 and the order in §6.3 |
| 6. Reads of a segment being reused | §5.6: extent header validation and a per-segment reader count |
| 7. Identifier reuse | S18 and §4.2 |
| 8. Ambiguous last frame | §5.7: exact end recorded on clean shutdown, last acknowledged frame recorded in every frame header, residual risk stated |
| 9. Corrupted header hides the next frame | §5.7: search at 4 KiB boundaries |
| 10. Torn reads during materialization | §6.4: reads served from the in-flight batch |
| 11. Ordering of manifest and segment reuse | §4.8 atomic replace order; §5.6 reuse only after a durable manifest |
| 12. Dead holders | §4.4 promise 6 |
| 13. Full disk | §5.4 item 7 |
| 14. Unbounded memory | §5.4 item 6 and §6.2 |
| 15. Mini-transaction size | §5.4 item 6: at most 1 MiB |
| 16. Blast radius of a flush failure | §4.3 promise 7 and §8.1 step 1 |
| 17. Key retirement | §7 and Section 11 item 8 |
| 18. Smaller encryption and check-order gaps | §7 (CRC over ciphertext, nonce counter) and §4.3 promise 1 (fence before idempotence) |
