# **Cascade DB Storage Subsystem: High-Level Design v2**

Nov 14, 2026 · @Ken

## **1\. Purpose and scope**

This doc fixes the shape of the Cascade DB storage subsystem so each component can go to detailed design independently. It replaces the first brainstorm; where they differ, this doc wins.

**In scope:** writing scopes, WAL and LSN, clock and transaction IDs, commit/abort, the read rule, page cache, PageStore, the table access API, the IOT and heap engines, undo, vacuum, secondary indexes and index MVCC, catalog storage, encryption, TOAST placement, and recovery boundaries.

**Out of scope:** SQL planning and execution, the network protocol, the cluster scheduler, and backup/PITR tooling.

**Runtime assumptions:** Rust, thread-per-core, \!Send per-core state, io\_uring for I/O. Shared state across cores is limited to what is named here (for example the per-scope LSN counter).

**How to read each component section:** every component lists its *Responsibility*, *Key decisions*, *Invariants*, *Interfaces*, and *Open questions*. A detailed design for that component should expand each heading and must not break an invariant without changing this doc first.

**Terms used throughout:**

| Term | Meaning |
| :---- | :---- |
| Scope | A writing scope: a whole DB, one table, or one table shard. Unit of single-writer ownership. |
| Log domain | The one logical WAL of a scope. |
| LSN | 64-bit byte offset into a log domain's WAL. Orders log records. |
| HLC ts | Hybrid logical clock timestamp. Orders commits and reads. |
| txn\_id | Per-scope 63-bit transaction counter. Identifies a writer, not an order. |
| Durable LSN | End of the contiguous persisted prefix of the WAL. |
| Closed ts | Writer's promise that no future commit in the scope will have commit\_ts ≤ it. |
| PageStore | Disaggregated storage that holds WAL and materializes pages from it. |

## **2\. Requirements and design principles**

Seven requirements drive the design. Each maps to the sections that satisfy it.

| \# | Requirement | Where it is met |
| :---- | :---- | :---- |
| R1 | Unified table access API | §10 |
| R2 | Index-organized storage (PK-clustered B-tree, row in leaf, secondary index points to PK) | §11, §13 |
| R3 | Heap-organized storage (Postgres-like, heap and index separate) | §12, §13 |
| R4 | Undo log for IOT | §11 |
| R5 | Vacuum for heap | §12 |
| R6 | The log is the database: no direct page writes, WAL merged into pages by a separate component, Aurora-style disaggregation, no checkpoint stalls | §5, §8, §9 |
| R7 | MVCC at both data and index level | §7, §11–§13 |

**Principles**

1. **One writer per scope.** All write ordering, LSN allocation and conflict detection are local to a scope. No distributed write transactions (DDL is the single, narrow exception).  
2. **The WAL is the only durable write on the commit path.** Pages are a cache of the log. Nothing else (status tables, undo, page images) is made durable separately.  
3. **Two orders, two clocks.** LSN orders log bytes; HLC orders commits and reads. They are never mixed.  
4. **One read rule.** Every read, single-scope or cross-scope, is served at a read\_ts once the scope's closed ts has reached it.  
5. **Engines differ only below the access API.** IOT and heap share WAL, page cache, txn status, snapshots and PageStore.  
6. **Redo is per page and deterministic.** The PageStore can rebuild any page from a base image plus redo, with no access to other pages or to engine state.  
7. **No global pauses.** No checkpoint, no global vacuum freeze, no stop-the-world. Background work is incremental and per page or per segment.

## **3\. Architecture overview**

Compute nodes own scopes and run transactions; the PageStore owns durability and page materialization. The only thing compute sends to storage on the write path is WAL.

![storage architecture · one scope][image1]

storage architecture · one scope

Engines share the txn manager, WAL writer and page cache. The WAL writer is the only path to durable storage; the page cache refills from the PageStore.

**Components**

| Layer | Component | One-line role |
| :---- | :---- | :---- |
| Compute | Table access API | Engine-neutral row operations used by the executor (§10) |
| Compute | IOT engine | PK-clustered B-tree, in-place update, undo chain (§11) |
| Compute | Heap engine | Append-only versions, HOT, pruning, vacuum (§12) |
| Compute | Secondary index layer | Versioned index entries for both engines (§13) |
| Compute | Txn manager | txn\_id, HLC, commit/abort, status table, snapshots (§6, §7) |
| Compute | WAL writer | LSN allocation, record assembly, durable LSN tracking (§5) |
| Compute | Page cache | Per-core cache of pages at a known page LSN (§8) |
| Compute | Schema cache | Per-core catalog view, invalidated by catalog WAL (§14) |
| Storage | PageStore | Durable WAL, per-page redo apply, page serving at an LSN (§9) |
| Cluster | Metadata store | Small Raft group: DB → scope, location, owner epoch (§4, §14) |

**Write path (one scope)**

1. Executor calls the access API; the engine builds a WAL record (redo, plus undo for IOT).  
2. WAL writer allocates an LSN with fetch\_add and copies the record into the log buffer.  
3. Engine applies the same record to the cached page and sets page LSN.  
4. WAL ships to the PageStore. When the durable LSN passes a commit record, the txn is marked committed and the client is acked.  
5. The PageStore applies redo to its pages lazily or on demand. Compute never writes pages.

**Read path**

1. The txn picks a read\_ts (HLC) and waits until each touched scope's closed ts ≥ read\_ts.  
2. The engine reads pages from the page cache; on a miss it fetches the page from the PageStore at the scope's durable LSN.  
3. Visibility is decided per row version and per index entry against read\_ts using the status table.

## **4\. Writing scopes and ownership**

**Responsibility.** Define the unit of single-writer ownership and how ownership moves between compute nodes safely.

**Key decisions**

* A scope is one of: (a) a whole DB, (b) one table, (c) one table shard. A DB's tables may be spread across writers; a table's shards may be spread across writers.  
* Per scope: one log domain, one LSN space, one durable LSN, one closed ts, one txn\_id counter, one status table, one encryption key, one PageStore partition.  
* A write txn writes to exactly one scope. The engine rejects a write that targets a second scope.  
* The catalog lives in its own scope per DB (the catalog scope).  
* DDL is the only cross-scope write: 2PC with the catalog scope as coordinator and touched table scopes as participants. 2PC is not available to DML.  
* Ownership is held under an epoch. Handoff bumps the epoch in the metadata store; the PageStore rejects WAL from a stale epoch (fencing).

**Invariants**

* At most one node can make WAL durable for a scope at any epoch.  
* Every WAL record of a scope belongs to exactly one epoch, and epochs are monotonic along the LSN.  
* A non-DDL txn never has records in two log domains.

**Interfaces**

* Metadata store: acquire(scope, expected\_epoch) \-\> epoch, lookup(scope) \-\> (owner, epoch, partition).  
* PageStore: open\_writer(scope, epoch) \-\> start\_lsn (fences older epochs, returns the durable LSN to resume from).  
* 2PC records: PREPARE, COMMIT\_PREPARED, ABORT\_PREPARED WAL record types, valid only for DDL txns.

**Open questions**

* Scope split, merge and move lifecycle (how a table becomes shards, how shards rebalance).  
* Shard key and partitioning scheme (hash, range, or both).  
* Failure blast radius: on WAL write failure, fence the whole node or only the affected scope.  
* Pending confirmation: sharded tables get local secondary indexes only (unique only if the key includes the shard key); no cross-scope FK enforcement.

## **5\. WAL and LSN**

**Responsibility.** Produce one totally ordered, durable log per scope. The log is the database: every change to any page, the status table, undo, or catalog is a WAL record.

**Key decisions**

* LSN \= 64-bit, ever-increasing byte offset into one logical WAL per log domain. It never resets per file.  
* Physical storage is fixed-size segment files; segment \= lsn / seg\_size, offset in segment \= lsn % seg\_size.  
* A record's LSN is its start offset. The next record starts at the end of the previous one, so consecutive LSNs have gaps equal to record size.  
* Allocation: one shared atomic fetch\_add(record\_len) per scope. Per-core block reservation is an optimization only if profiling shows contention.  
* Durable LSN \= end of the contiguous persisted prefix. Persisting up to N means every byte below N is persisted. Holes from in-flight copies block the durable LSN until filled.  
* Every record carries: LSN, length, record type, record version, scope epoch, txn\_id (or none), and a checksum. Page-targeted records also carry the page id(s).  
* Record families: **page redo** (per page, physiological), **txn** (begin optional, commit, abort, prepare/commit-prepared/abort-prepared for DDL), **undo** (IOT undo payloads, in undo pages), **CLR** (compensation during IOT rollback), **closed-ts heartbeat**, **HLC high-water**, **reserved: bulk-load** and **reserved: unlogged/temp marker**.  
* WAL is encrypted with the scope key (§15).

**Invariants**

* LSNs are unique and strictly increasing within a scope; record N is fully contained in \[N, N+len).  
* A record is visible to the PageStore only after it is inside the durable prefix.  
* No record references a page in another scope.  
* Redo records are self-contained per page: applying them needs only the page image and the record (§9).

**Interfaces**

* reserve(len) \-\> Lsn, fill(lsn, bytes), durable\_lsn() \-\> Lsn, wait\_durable(lsn) (async, per-core waker).  
* PageStore ingest: append(scope, epoch, lsn\_range, bytes) \-\> ack(durable\_lsn).

**Open questions**

* Exact record header layout and versioning rules (part of the WAL contract one-way door, §9).  
* Segment size and retention once pages are materialized (ties to recovery without checkpoint).  
* Bulk-load record semantics (reserve the type now).

## **6\. Clock, transaction IDs, commit and abort**

**Responsibility.** Give every txn an identity, give every commit a timestamp, and make the WAL commit record the single source of truth for outcome.

**Key decisions**

* **HLC** \= 48-bit physical ms \+ 16-bit logical counter; ts \= max(prev \+ 1, now). Used only for commit\_ts and read\_ts, never as an LSN or txn\_id.  
* Max issued HLC is persisted via an HLC high-water WAL record. A new primary starts at max(persisted \+ 1, now).  
* **txn\_id** \= per-scope 63-bit counter, allocated at first write. Read-only txns get none.  
* **Version word** on rows and index entries is 64 bits. Top bit 1 \= uncommitted, low 63 bits \= txn\_id. Top bit 0 \= committed, value \= commit\_ts (after stamping). This is why the top bit of txn\_id is kept free.  
* **Status table** per scope maps txn\_id → {in-progress, committed(commit\_ts), aborted}. It is stored as pages derived from WAL commit/abort records, not written separately.  
* Commit record carries txn\_id and commit\_ts; its LSN is its position.

**Commit protocol**

1. Assign commit\_ts from the scope HLC and append the commit record.  
2. Wait until the durable LSN covers the commit record.  
3. Mark committed in the in-memory status table and release locks.  
4. Ack the client.

If a WAL write fails, the outcome is unknown. The writer fences itself (or restarts), and recovery decides the outcome from what is in the durable log.

**Abort**

* IOT: walk the txn's undo chain, restore prior row images in place, and log a CLR for each step; then write the abort record.  
* Heap: write the abort record only. Aborted versions become invisible via the status table and are reclaimed by pruning or vacuum.  
* Index entries written by an aborted txn are invisible via the status table and cleaned up with their rows.

**Invariants**

* A txn is committed if and only if its commit record is in the durable WAL.  
* No client ack before the commit record is durable.  
* Within a scope, commit\_ts is unique and increases with commit-record LSN (proposed: assign commit\_ts inside the same critical section as the commit-record reservation; confirm in detailed design).  
* Closed ts never passes the commit\_ts of a commit that is not yet durable (§7).

**Interfaces**

* begin(isolation) \-\> Txn, txn.assign\_id() \-\> TxnId (lazy), commit(txn) \-\> Result\<CommitTs, CommitError\>, abort(txn).  
* status(scope, txn\_id) \-\> TxnStatus, hlc.now(), hlc.observe(ts) (for session tokens and remote timestamps).

**Open questions**

* Txn status stamping (when the version word is rewritten from txn\_id to commit\_ts) and status table retention.  
* Recovery of in-flight IOT txns on a new owner (undo replay and CLRs) without a checkpoint.

## **7\. Snapshots and the read rule**

**Responsibility.** Decide which versions a read sees, for every isolation level, using one rule for single-scope and cross-scope reads.

**Key decisions**

* A snapshot is a single HLC read\_ts plus the reader's own txn\_id (to see its own writes). There are no active-txn lists.  
* A committed version is visible if commit\_ts ≤ read\_ts. An uncommitted version is visible only to its own txn.  
* **Read rule:** a scope serves a read at read\_ts once its closed ts ≥ read\_ts. The same rule applies to the owner node, replicas, and cross-scope reads.  
* **Closed ts:** the writer's promise of no future commit with commit\_ts ≤ closed ts. It never passes a commit that is not yet durable. Idle scopes advance it with heartbeat records.  
* **Cross-scope reads** (for example SELECT over all shards) use ONE read\_ts from the HLC. A max clock-skew bound defines an uncertainty window: a version with commit\_ts in (read\_ts, read\_ts \+ max\_skew\] forces a restart at a higher read\_ts.  
* **Session HLC token:** the client carries the highest commit\_ts it has seen; the next read\_ts is at least that value, giving read-your-writes across scopes.

**Isolation levels (within a scope)**

| Level | read\_ts | Write-write conflict on a row changed after read\_ts |
| :---- | :---- | :---- |
| Read Committed | New per statement | Wait for the other writer, then re-check the latest committed version and re-evaluate the predicate |
| Repeatable Read | One per txn | Serialization failure |
| Serializable (SSI) | One per txn | Serialization failure, plus SSI read-write dependency tracking (PostgreSQL-style) |

**Invariants**

* Once a read at read\_ts has been served by a scope, no later commit in that scope can have commit\_ts ≤ read\_ts.  
* All scopes in one cross-scope read use the same read\_ts.  
* A txn never sees a partial commit: either all of a committed txn's versions in a scope are visible at read\_ts or none are.

**Interfaces**

* snapshot(read\_ts, own\_txn) \-\> Snapshot, scope.closed\_ts(), scope.wait\_closed(read\_ts).  
* Snapshot::visible(version\_word) \-\> Visibility (shared by both engines and by indexes).  
* SSI hooks: ssi.on\_read(key\_or\_range), ssi.on\_write(key), ssi.check\_commit(txn).

**Open questions**

* Cross-scope read guarantee: CockroachDB-level causal (uncertainty restart) vs Spanner-level strict (commit wait).  
* Serializable for a write txn that also reads other scopes.  
* Read replicas per scope and how closed ts is published to them.  
* Cleanup (GC) watermark: the oldest read\_ts still in use, across scopes and replicas.

## **8\. Page cache**

**Responsibility.** Hold recently used pages in compute memory at a known LSN, so writers can apply changes and readers can avoid PageStore round trips. It is the foundation for both engines.

**Key decisions**

* Page LSN \= LSN of the last record applied to the page, committed or not.  
* Write order: append the record to WAL (LSN reserved and filled), then apply it to the cached page and set page LSN. The page is never written back to storage.  
* **No eviction of a page whose page LSN \> durable LSN.** Such a page holds changes the PageStore may not have yet.  
* An evicted page is refetched from the PageStore at the scope's current durable LSN; the PageStore applies any outstanding redo before returning it.  
* Cache is per core, following thread-per-core ownership. A page belongs to the core that owns its scope; other cores reach it by message, not shared locks (detailed design to confirm for cross-core readers).  
* A row cache above the page cache is optional and deferred.  
* Unlogged/temp pages, when added, use a separate non-logged path (open).

**Invariants**

* For every cached page P: all WAL records for P with LSN ≤ page\_lsn(P) have been applied, and none above.  
* A page fetched from the PageStore at LSN L reflects exactly the records for that page with LSN \< L.  
* Dropping any page with page LSN ≤ durable LSN loses nothing.

**Interfaces**

* get(page\_id, mode) \-\> PageGuard (async on miss), apply(page\_guard, record, lsn), new\_page(scope) \-\> PageGuard.  
* Eviction policy hook, with the durable-LSN pin as a hard rule.  
* PageStore client: get\_page(scope, page\_id, at\_lsn) \-\> PageImage.

**Open questions**

* B-tree structure modification (split/merge) atomicity and latching across pages (shared with §11–§13).  
* Cross-core read access to pages owned by another core: messaging vs read-only shared snapshots.  
* Memory pressure when the durable LSN lags (backpressure on writers).

## **9\. PageStore and the redo contract**

**Responsibility.** Make WAL durable, and turn it into pages without any help from compute. This is what removes checkpoints: pages are materialized continuously, per page, in the background.

**Key decisions**

* One PageStore partition per scope. It accepts WAL only from the current epoch's writer.  
* Durability follows the Aurora model: WAL is replicated across storage nodes and acknowledged by quorum. Replica count, quorum sizes and placement are open.  
* For each page the PageStore keeps a base image plus a chain of redo records. A background materializer folds redo into new base images; no global checkpoint exists.  
* get\_page(page\_id, at\_lsn) applies outstanding redo up to at\_lsn on demand and returns the image.  
* The PageStore decrypts and re-encrypts with the scope key to apply redo (§15).  
* Engines are opaque to the PageStore except through redo records: it knows page formats only as far as the redo apply functions need.

**WAL record contract (one-way door)**

1. **Self-contained per page.** A redo record names one page (or a fixed small set for SMOs) and applies with only that page image and the record.  
2. **Deterministic.** The same page image plus the same record always gives the same bytes. No clocks, no randomness, no reads of other pages.  
3. **Versioned.** Each record has a type and a version; each page has a format version. The PageStore must apply every version it has ever accepted.  
4. **Idempotent by LSN.** Applying a record whose LSN ≤ page LSN is a no-op.

This contract is designed together with the row header and index entry formats (§11–§13), because those formats decide what each redo record carries.

**Invariants**

* A page served at LSN L includes exactly the records for that page with LSN \< L.  
* The PageStore never applies a record above the durable LSN.  
* WAL from a fenced epoch is rejected.

**Interfaces**

* open\_writer(scope, epoch) \-\> durable\_lsn, append(scope, epoch, lsn\_range, bytes) \-\> durable\_lsn.  
* get\_page(scope, page\_id, at\_lsn) \-\> PageImage.  
* set\_min\_retained\_lsn(scope, lsn) for WAL and page-version retention.

**Open questions**

* Quorum, replication and placement details (Aurora model, details open).  
* Recovery without checkpoint: how a new owner finds in-flight txns and their undo quickly.  
* Whether to allow blind redo to pages not in the compute cache (the brainstorm's change buffering), and which operations qualify.  
* Bulk-load path: pre-built pages handed to the PageStore under a reserved WAL record type.

## **10\. Unified table access API**

**Responsibility.** Give the executor one interface for rows, whatever the storage organization. Engines hide row location, version chains, index maintenance and cleanup behind it.

**Key decisions**

* RowLocator is opaque to the executor: PK bytes for IOT, TupleId (page, slot) for heap. The executor only stores and passes it back.  
* Writes return a WriteOutcome, not just success. It carries the row's new locator (the locator moves on an IOT PK update and on a non-HOT heap update) and reports write-write conflicts and unique violations as values, not errors.  
* The engine maintains secondary indexes inside insert/update/delete. This lets the heap skip index writes on HOT updates and lets the IOT skip them when no indexed column changed.  
* Sequential scan and index scan are separate entry points. IOT seq scan walks PK-ordered leaves; heap seq scan walks pages in order.  
* Index-only scans are possible on both engines because index entries carry their own visibility (§13). No visibility map is needed.  
* Async without Send bounds, matching the thread-per-core runtime. All calls run on the core that owns the scope.

**Sketch**

pub enum RowLocator { Pk(KeyBytes), Tid(TupleId) }   // opaque to executor

pub enum WriteOutcome {  
    Done { locator: RowLocator },                    // may differ from input locator  
    Conflict(Conflict),                              // write-write conflict  
    UniqueViolation { index: IndexId, key: KeyBytes },  
    NotVisible,                                      // row gone at this snapshot  
}

pub enum Conflict {  
    InProgress { holder: TxnId },                    // caller decides: wait / nowait / skip locked  
    CommittedAfterSnapshot { commit\_ts: Hlc,  
                             latest: Option\<RowLocator\> }, // RC re-checks; RR/SSI fail  
}

pub trait TableAccessMethod {  
    fn kind(\&self) \-\> StorageKind;                   // Iot \| Heap

    // writes (txn must own this table's scope)  
    async fn insert(\&self, txn: \&mut TxnCtx, row: \&Row) \-\> Result\<WriteOutcome\>;  
    async fn update(\&self, txn: \&mut TxnCtx, loc: \&RowLocator, new: \&Row,  
                    changed: \&ColumnSet) \-\> Result\<WriteOutcome\>;  
    async fn delete(\&self, txn: \&mut TxnCtx, loc: \&RowLocator) \-\> Result\<WriteOutcome\>;  
    async fn lock\_row(\&self, txn: \&mut TxnCtx, loc: \&RowLocator,  
                      mode: RowLockMode, wait: WaitPolicy) \-\> Result\<WriteOutcome\>;

    // reads  
    async fn fetch(\&self, snap: \&Snapshot, loc: \&RowLocator,  
                   proj: \&Projection) \-\> Result\<Option\<RowRef\>\>;  
    async fn fetch\_latest\_committed(\&self, txn: \&TxnCtx,  
                   loc: \&RowLocator) \-\> Result\<Option\<(RowLocator, RowRef)\>\>; // RC re-check  
    fn seq\_scan(\&self, snap: \&Snapshot, proj: \&Projection,  
                opts: SeqScanOpts) \-\> impl RowStream;  
    fn index\_scan(\&self, snap: \&Snapshot, index: IndexId, range: KeyRange,  
                  dir: Direction, proj: \&Projection, index\_only: bool) \-\> impl RowStream;

    // DDL and maintenance  
    async fn build\_index(\&self, txn: \&mut TxnCtx, def: \&IndexDef) \-\> Result\<()\>;  
    async fn truncate(\&self, txn: \&mut TxnCtx) \-\> Result\<()\>;  
    fn maintenance(\&self) \-\> \&dyn Maintenance;       // undo purge (IOT) / prune \+ vacuum (heap)  
    fn stats(\&self) \-\> TableStats;  
}  
For an IOT, index\_scan on the primary index returns rows straight from leaves; on a secondary index it looks up rows by PK. For a heap, every index (including the PK index) returns TupleIds that are fetched from the heap.

**Invariants**

* A locator returned by a write is valid for that txn's later calls in the same statement.  
* Every write goes through the scope's txn manager and WAL writer; the API offers no page-level escape hatch.  
* Visibility is decided only by Snapshot::visible (§7), never inside the executor.

**Open questions**

* Batch APIs (multi-row insert, bulk load) and their WAL record shape.  
* How SSI predicate reads are reported for seq scans vs index range scans.  
* TOAST: whether de-toasting is lazy behind RowRef (likely) and how projection pushes down.

## **11\. IOT engine and undo**

**Responsibility.** Store rows in a PK-clustered B-tree with the newest version in the leaf, and keep older versions in undo for MVCC reads and rollback.

**Key decisions**

* Every IOT row has a primary key. The row lives in the leaf of the PK B-tree.  
* The leaf holds the **newest** version (committed or not). Updates are in place.  
* Row header: version word (§6), roll\_ptr to the previous version's undo record, flags (delete-mark, has-TOAST).  
* **Undo records are deltas:** the prior values of changed columns, the prior version word, and the prior roll\_ptr. Records of a row form a chain from newest to oldest.  
* Undo lives in **undo pages** in the same scope. Undo pages go through the page cache and every change to them is WAL redo, so the PageStore materializes them like any other page. Undo is never written separately.  
* Operations:  
  * Insert: new leaf row \+ an insert-undo record (rollback removes the row).  
  * Update: write a delta undo record, then overwrite the leaf row in place.  
  * Delete: set delete-mark \+ undo record. Physical removal waits for purge.  
  * PK update: delete-mark the old key \+ insert the new key. The locator moves.  
* Read: if the leaf version is not visible to the snapshot, walk roll\_ptr and apply deltas until a visible version (or none) is found.  
* Rollback: walk the txn's undo chain, restore each row, log a CLR per step, then write the abort record.  
* **Purge:** removes delete-marked rows and frees undo once no snapshot can need them. Undo is truncated at **segment** granularity: a segment is freed when all txns in it are resolved and older than the cleanup watermark.

**Invariants**

* The leaf row plus its undo chain can reconstruct every version that any active snapshot may read.  
* At most one uncommitted version per row (the leaf), owned by the txn in its version word.  
* An undo record is durable (in WAL) before or with the leaf change that depends on it.  
* Purge never frees undo newer than the cleanup watermark.

**Interfaces**

* Implements TableAccessMethod (§10) with RowLocator::Pk.  
* Undo manager: undo.append(txn, record) \-\> RollPtr, undo.read(roll\_ptr), undo.rollback(txn), undo.truncate\_until(watermark).  
* Redo types (proposed): leaf insert, leaf update-in-place, leaf delete-mark, leaf purge, undo append, CLR, B-tree SMO.

**Open questions**

* B-tree SMO (split/merge) atomicity and latching, and how SMO redo stays page-local.  
* Undo segment layout (per txn, per core, or shared) and how recovery finds in-flight txns' undo without a checkpoint.  
* Long-running readers holding back purge (cleanup watermark across scopes and replicas).  
* Row size limit before TOAST kicks in.

## **12\. Heap engine and vacuum**

**Responsibility.** Store rows as append-only versions in heap pages, with indexes separate (PostgreSQL-like), and reclaim dead versions incrementally.

**Key decisions**

* Tuple header: creator version word, deleter version word (empty if live), next pointer to the newer version (TupleId), flags (HOT-updated, heap-only, has-TOAST).  
* Insert: new tuple. Update: new tuple \+ set the old tuple's deleter word and next. Delete: set the deleter word.  
* **HOT:** if no indexed column changed and the new version fits on the same page, it is a heap-only tuple chained from the old one. No index entries are written; indexes keep pointing at the chain root.  
* Abort: write the abort record only. Aborted tuples are dead by status lookup and removed by pruning or vacuum.  
* **Opportunistic prune (HEAP\_PRUNE):** when a page is accessed and is low on space, remove dead versions older than the cleanup watermark and collapse HOT chains. It is a WAL redo record like any page change.  
* **Background vacuum:** per page and incremental, never table-wide stop. It removes dead tuples, removes their index entries, and updates the free space map.  
* **No freeze and no wraparound vacuum.** txn\_ids are 63-bit per scope and commit\_ts is an HLC, so nothing wraps.  
* Free space map and any heap bookkeeping pages are ordinary WAL-logged pages.

**Invariants**

* A tuple is removed only when its deleter (or its aborted creator) is older than the cleanup watermark.  
* An index entry pointing at a TupleId is removed before or with that slot's reuse, so a slot is never reused under a live index entry.  
* A HOT chain never crosses a page.

**Interfaces**

* Implements TableAccessMethod (§10) with RowLocator::Tid.  
* Maintenance: prune\_page(page), vacuum\_step(budget) \-\> Progress, fsm.find(size), fsm.update(page, free).  
* Redo types (proposed): heap insert, heap update (old \+ new, same or different page), heap delete-mark, HEAP\_PRUNE, vacuum slot free, FSM update.

**Open questions**

* Vacuum scheduling and how it learns the cleanup watermark across scopes and replicas.  
* Whether index cleanup during vacuum can rely on index entries' own delete\_ts (§13) instead of TID lookups.  
* Unlogged/temp heap tables (non-logged path).

## **13\. Secondary indexes and index MVCC**

**Responsibility.** Provide B-tree secondary indexes for both engines where each entry carries its own visibility, so an index scan can decide visibility without visiting the row.

**Key decisions**

* Entry \= (index key, locator, create word, delete word). Locator \= PK for IOT tables, TupleId for heap tables. Create and delete words use the same version-word format as rows (§6).  
* For a heap table the PK index is just a unique secondary index. For an IOT the PK B-tree is the table itself and versions live in the row \+ undo (§11), not in index entries.  
* Insert row: insert entry with create word \= writer's txn\_id (uncommitted).  
* Update that changes an indexed column: set the delete word on the old entry, insert a new entry. Update that changes no indexed column: no index write (HOT for heap; same PK for IOT).  
* Delete row: set the delete word on every entry of the row.  
* Visibility: an entry is visible if create is visible and delete is not, under Snapshot::visible.  
* Version words in an index page are delta-encoded against a page base value to save space (from the brainstorm).  
* **Unique check:** a key conflicts if any entry with that key is visible-live to the latest state or uncommitted by another txn. Uncommitted-other leads to Conflict::InProgress; committed-live leads to UniqueViolation.  
* Index entries are cleaned up when their delete word (or an aborted create) is older than the cleanup watermark, by index page pruning or vacuum.  
* Sharded tables (pending confirmation): local secondary indexes only; unique only if the key includes the shard key.

**Invariants**

* For every row version visible at read\_ts there is exactly one visible index entry per index pointing at it (or its HOT chain root).  
* An index entry never becomes visible before the row version it points to.  
* At most one uncommitted change per entry, owned by the txn in its version word.

**Interfaces**

* index.insert(txn, key, locator), index.mark\_deleted(txn, key, locator), index.check\_unique(txn, key) \-\> Result\<(), Conflict \| UniqueViolation\>.  
* index.scan(snap, range, dir) \-\> stream of (key, locator); index\_only returns covered columns from the key.  
* Redo types (proposed): entry insert, entry set-delete-word, entry prune, B-tree SMO.

**Open questions (index MVCC costs)**

* Cost of unique checks when many versioned entries share a key.  
* Write cost of setting the delete word on the old leaf for every indexed-column update and delete.  
* Deferred index writes (buffer index changes until commit or into the PageStore) and how they interact with unique checks.  
* Final index entry format: decided together with the row header and the WAL contract (next topic).

## **14\. Catalog and schema**

**Responsibility.** Store each DB's metadata inside that DB, using the same storage machinery as user tables, and keep per-core schema caches correct.

**Key decisions**

* The DB is the boundary for catalog and security. No cross-DB transactions.  
* Each DB owns its full catalog, roles included, as MVCC IOT tables in the DB's catalog scope (its own WAL).  
* Objects are referenced by stable UUID, not a local OID, so they survive scope moves and splits.  
* The cluster layer is only a small Raft metadata store: DB name → scope/log domain id, location, state, owner epoch, and cluster-admin principals.  
* Schema cache is per core per DB. It is invalidated by tailing catalog WAL records; there is no shared invalidation queue.  
* Writers on other nodes hold F1-style schema leases so that at most two adjacent schema versions are live at once.  
* DDL that touches table scopes uses 2PC with the catalog scope as coordinator (§4).

**Invariants**

* A table scope never accepts a write under a schema version older than the oldest leased version.  
* Catalog reads follow the same read rule as user reads (closed ts of the catalog scope).

**Interfaces**

* schema\_cache.get(db, uuid, read\_ts) \-\> TableDef, schema\_cache.on\_catalog\_wal(record).  
* lease.acquire(db, version), lease.renew().  
* Metadata store API (§4).

**Open questions**

* Sequences and identity columns on sharded tables.  
* Lease duration and behavior on lease loss.

## **15\. Encryption, TOAST, durability and recovery**

**Encryption**

* Page-level encryption. One key per writing scope covers that scope's pages and its WAL.  
* The smallest unit a user can encrypt separately is a table shard (a shard-level scope gets its own key).  
* The PageStore must be able to use the scope key, because it applies redo to pages.  
* Open: key rotation without rewriting all pages; key hierarchy and KMS integration.

**TOAST / overflow**

* Both engines must support out-of-line storage for large values.  
* TOAST data lives in the same scope as its row, so a write txn stays single-scope and TOAST pages share the row's WAL and key.  
* Open: chunk format, compression, whether TOAST is a hidden IOT keyed by (value id, chunk no), and versioning of toasted values on update.

**Durability and HA**

* Follow the Aurora model: quorum-replicated WAL in the PageStore, compute nodes are stateless apart from caches.  
* Failover \= a new node acquires the scope with a higher epoch (fencing the old one), learns the durable LSN, and starts with an empty page cache that fills on demand.  
* Open: quorum sizes and placement, read replicas per scope, failure blast radius (fence the node or only the scope).

**Recovery (no checkpoint)**

* Pages never need replay on compute: the PageStore serves them at the durable LSN.  
* Txn outcomes come from the WAL: commit or abort record present means decided; otherwise the txn was in flight and is rolled back (IOT via undo \+ CLRs; heap needs no page work).  
* Open: how the new owner finds in-flight txns and rebuilds the status table, HLC high-water and closed ts quickly, without scanning a long WAL tail.

## **16\. Decision log**

"Decided" items are settled and detailed designs must follow them. "Proposed" items are introduced in this doc to make the design hang together and need a quick confirm. "Pending" items await confirmation.

| Area | Decision | Status |
| :---- | :---- | :---- |
| Writer model | One writer per scope; scope \= DB, table, or table shard; no distributed write txns | Decided |
| Ownership | Per-scope epochs and fencing on handoff | Decided |
| DDL | Only cross-scope txn; 2PC restricted to DDL; catalog scope coordinates | Decided |
| Boundary | Cluster \= pool of compute; DB \= catalog and security boundary; no cross-DB txns | Decided |
| Catalog | Per-DB MVCC IOT catalog incl. roles; UUID refs; Raft metadata store; per-core schema cache via catalog WAL; F1 leases | Decided |
| LSN | 64-bit byte offset, one logical WAL per scope, fixed-size segments, fetch\_add, contiguous durable prefix | Decided |
| Clock | HLC (48-bit ms \+ 16-bit logical) for commit\_ts and read\_ts only; persisted high-water | Decided |
| Txn ID | Per-scope 63-bit counter; top bit \= uncommitted tag; status table txn\_id → commit\_ts | Decided |
| Commit | Append → durable → mark committed → ack; WAL failure \= fence; WAL commit record is sole truth | Decided |
| Abort | IOT: undo \+ CLRs; heap: abort record only | Decided |
| Read rule | Serve at read\_ts once closed ts ≥ read\_ts; one read\_ts for cross-scope reads; skew bound \+ uncertainty restart; session HLC token | Decided |
| Page cache | Page LSN \= last applied; WAL then apply; no eviction above durable LSN; refetch at durable LSN | Decided |
| Isolation | RC, RR, SSI within a scope | Decided |
| Encryption | Page-level; one key per scope for pages and WAL; PageStore uses key | Decided |
| TOAST | Required; same scope as row | Decided |
| HA | Aurora model | Decided (details open) |
| IOT | Newest version in leaf, in-place update, delta undo chained by roll\_ptr, segment-level undo truncation | Decided (from brainstorm) |
| Heap | Append-only versions, HOT, HEAP\_PRUNE, incremental vacuum | Decided (from brainstorm) |
| Redo | Physiological, self-contained per page, deterministic, versioned | Decided (format is next topic) |
| Version word | 64-bit: top bit 1 \= uncommitted txn\_id, top bit 0 \= commit\_ts | Proposed |
| commit\_ts order | commit\_ts increases with commit-record LSN within a scope | Proposed |
| Access API | WriteOutcome with Conflict / UniqueViolation; locator moves; separate seq and index scans; engine maintains indexes | Proposed |
| Heap freeze | No freeze or wraparound vacuum (63-bit txn\_id) | Proposed |
| Sharded indexes | Local secondary indexes only; unique only with shard key | Pending |
| Foreign keys | No cross-scope FK enforcement | Pending |

**Superseded from the first brainstorm**

* Eager local apply is kept as the write order (WAL first, then apply to the cached page). Change buffering is no longer a default and is now an open option (blind redo, §9).  
* Brainstorm visibility fields on rows and index entries are now version words (txn\_id while uncommitted, commit\_ts once stamped), tied to the per-scope status table.  
* The PageStore materializer is kept, now partitioned per scope and fenced by epoch.

## **17\. Open questions**

These are deliberately undecided. Each names the section it blocks and when it must be answered.

| Question | Affects | Needed before |
| :---- | :---- | :---- |
| WAL record contract \+ row header \+ index entry format (one-way door) | §5, §9, §11–§13 | Any engine detailed design (next topic) |
| Index MVCC costs: unique checks with versioned entries, delete-word write cost, deferred index writes | §13 | Index entry format |
| B-tree SMO atomicity and latching | §8, §11, §13 | IOT and index detailed design |
| Txn status stamping and status table retention | §6, §11–§13 | Row header format |
| Recovery without checkpoint (finding in-flight txns, rebuilding status, HLC, closed ts) | §6, §9, §15 | Failover design |
| Cleanup (GC) watermark across scopes and replicas | §7, §11–§13 | Purge and vacuum design |
| Cross-scope read guarantee: causal (CockroachDB) vs strict (Spanner commit wait) | §7 | Cross-scope read path |
| Serializable for a write txn that reads other scopes | §7 | SSI design |
| Read replicas per scope and closed-ts publishing | §7, §15 | Replica design |
| Failure blast radius: fence node vs scope | §4, §15 | Failover design |
| Scope split / merge / move lifecycle | §4 | Sharding design |
| Shard key and partitioning scheme | §4 | Sharding design |
| Sequences / identity on sharded tables | §14 | Sharding design |
| Bulk-load path (reserve WAL record type now) | §5, §9, §10 | WAL contract (reserve only) |
| Non-logged temp / unlogged storage path | §8, §12 | Can follow v1 |
| Blind redo / change buffering for non-cached pages | §9, §13 | Can follow v1 |
| PageStore quorum, replication and placement | §9, §15 | PageStore detailed design |
| Pending confirmation: local-only secondary indexes on sharded tables; no cross-scope FKs | §4, §13 | Sharding design |

## **18\. Detailed design work packages**

Detailed design should start with WP1, because its formats are one-way doors that every engine depends on. WP2–WP4 can then run in parallel.

| WP | Detailed design | Depends on | Must produce |
| :---- | :---- | :---- | :---- |
| WP1 | WAL record contract, row header, index entry format | This doc | Record header layout; versioning rules; page format versions; version-word stamping policy; per-type redo list |
| WP2 | WAL writer and txn manager | WP1 | LSN reservation and fill; durable LSN tracking; HLC; commit/abort state machine; status table pages; closed ts and heartbeats |
| WP3 | Page cache | WP1 | Page layout and guards; eviction with durable-LSN pin; cross-core access; backpressure |
| WP4 | PageStore | WP1 | Ingest and fencing; per-page redo chains; materializer; get\_page at LSN; retention; quorum model |
| WP5 | Table access API | WP2 | Final trait, outcome types, scan streams, maintenance hooks |
| WP6 | IOT engine and undo | WP1–WP3, WP5 | B-tree ops and SMO; undo segments; rollback with CLRs; purge |
| WP7 | Heap engine and vacuum | WP1–WP3, WP5 | Tuple layout; HOT; HEAP\_PRUNE; vacuum; FSM |
| WP8 | Secondary indexes | WP1, WP6/WP7 | Entry ops; unique checks; index-only scan; index cleanup |
| WP9 | Snapshots and isolation | WP2, WP8 | RC re-check; RR; SSI tracking; cross-scope read\_ts and uncertainty restart |
| WP10 | Catalog and schema | WP6, WP9 | Catalog tables; schema cache tailing; leases; DDL 2PC |
| WP11 | Recovery and failover | WP2, WP4, WP6 | Owner handoff; in-flight txn rollback; state rebuild without checkpoint |

Each WP doc should keep this doc's headings (responsibility, decisions, invariants, interfaces, open questions) and close or explicitly carry forward the open questions it inherits from §17.

[image1]: <data:image/png;base64,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>