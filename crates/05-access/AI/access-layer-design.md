# Cascade DB Access Layer: Contracts, Engines and Migration

**Status:** Draft v0.1 for review, 2026-11-26. This document is the single source of truth for the access layer (L5): the table access contract, the two table engines, secondary indexes, sequences, temporary tables and maintenance. It absorbs the retired WP5 table access API (D1–D15, accepted 2026-11-14; retired from the tree on 2026-11-22 together with HLD v2, recoverable at `381ee5b^` in git history) and re-homes the per-component designs that `crates/02-storage/AI/storage-layer-design.md` §12.7 defers to this layer. The detailed engine designs live beside their crates: [`cascade-btree/AI/btree-design.md`](../cascade-btree/AI/btree-design.md), [`cascade-heap/AI/heap-design.md`](../cascade-heap/AI/heap-design.md), and the already-landed [`cascade-page/AI/storage-page-mutation-design.md`](../cascade-page/AI/storage-page-mutation-design.md).

**Upstream:** [`docs/architecture-design.md`](../../../docs/architecture-design.md) v0.10.2 — §4.5 (transactions), §4.6 (L5 access), §4.8 (core sets and the shared-memory engine), §3.1 rules, §1.1 (changes to the designs of record). Where this document and the architecture disagree, the architecture wins unless this document says it proposes a change. **Storage foundations:** [`crates/02-storage/AI/storage-layer-design.md`](../../02-storage/AI/storage-layer-design.md) v0.5 §12, whose rules (§12.1–§12.6) bind every engine. **Formats:** WP1 F1–F4 (accepted; F1/F2 in `cascade-types` and `cascade-page`, F3 in `cascade-wal-format`, F4 in `cascade-pgtypes`). **Transaction machinery:** WP2 D1–D5 (accepted). **Page format:** the T6.1-locked layout and the page-layer decisions L1–L3 / heap-format decisions D1–D6 in the page mutation design.

**Code baseline:** `main` at `8fadd7a` (2026-11-26). Storage migration steps M0–M6 are landed (log store, `LogSink`/`LogSource` writer, boot and recovery on `LogSource`); M7 (`cascade-redo` and the page store) was in progress in the working tree as this was written — uncommitted, not yet compiled or gated — and M8 (page cache onto `PageSource`) follows it. The runtime M6 vertical slice has landed T6.0–T6.2 (page format, cross-core BPM); the heap access method (T6.4) has not landed yet — this design is its contract.

---

## 1. Purpose and Scope

The access layer is the API between the executor and tables. It speaks tables and rows: scans, point fetches, insert / update / delete / upsert, row locking, index builds, maintenance. Everything above it decides *policy* (isolation matrix, conflict arbitration, FK semantics, batching — the executor and `cascade-txn-coord`); everything below decides *physics* (pages, version chains, undo, WAL, indexes — this layer and the storage beneath it).

It has two faces, and every surveyed system separates them the same way (WP5 §1, kept):

- **Planner face** — capabilities and statistics only. The planner never calls data functions.
- **Executor face** — the row operations. Executed on a core of the table scope's write core set (Section 5).

Two table organizations are first-class behind one contract and one visibility rule:

- **IOT** (index-organized, the default engine) — rows in the leaves of a B-tree ordered by primary key, newest version in the leaf, older versions as undo deltas (HLD v2 §11). `cascade-iot` over `cascade-btree`.
- **Heap** (the compatibility engine) — append-only row versions, HOT chains, prune + vacuum (HLD v2 §12). `cascade-heap` over `cascade-page` kind 1.

Engines differ only below the access API (storage-layer-design §12.1 principle 5): both share the log, the page cache, the transaction manager, snapshots, and the page store.

### 1.1 Goals

- **One API, two engines, one visibility rule.** The executor gains a second engine without code changes (the `AnyTable` enum dispatch payoff, WP5 D1).
- **Every component testable in isolation.** The contract ships a conformance suite and an in-memory multi-version fake with no pages and no log (architecture Rule 6); each engine passes the same suite.
- **No page-level escape hatch.** Every write routes through the scope's mini-transaction, log writer and transaction machinery inside the engine. No engine API touches page bytes from above the layer.
- **Deterministic replay.** Every page change is a redo record group applied through `cascade-redo`; leaders, followers and page stores produce byte-identical pages (storage-layer-design §12.4).

### 1.2 Non-goals for version 1

- Pluggable third-party engines. The `AnyTable` enum has exactly two arms until a third engine justifies reopening WP5 D1's dispatch decision.
- Hash, GIN, GiST, BRIN and vector index kinds. `cascade-index` owns the index access-method seam they land behind (Section 9.2); version 1 ships B-tree secondary indexes only.
- Cross-scope transactions and sharded-table mechanics. Version 1 is one scope per database (architecture D10); the limits that apply after a split are the architecture's (§4.4), not this layer's.
- Bulk load beyond the reserved WAL record type (WP1 F3 type 8).
- Columnar. `cascade-columnar` consumes the same 8 KiB page unit but is its own layer (10-columnar), reached through the log, not through this API.

---

## 2. Summary of Decisions

| # | Decision | Section |
| :--- | :--- | :--- |
| A1 | The mini-transaction is the only write primitive; engines never write pages and never mutate a page without a log record group | §6.1 |
| A2 | Redo ownership: `cascade-redo` is the single dispatch entry; per-family apply modules delegate to the format authorities — heap byte transforms in `cascade-page`, btree transforms over `cascade-page`'s btree typed views; record payload encodings in `cascade-wal-format` | §6.2 |
| A3 | B-tree page formats (branch, IOT leaf, index leaf) live in `cascade-page` as new kinds under the prologue registry — amending the page mutation design's "btree formats live in cascade-btree"; `cascade-btree` owns tree logic, not bytes | §6.2 |
| A4 | Execution model: a scope is a shared-memory engine across its write core set (architecture §4.8). WP5 D8's message-to-owner reads (Option A) is superseded; page-level exclusion is the BPM pin protocol; the contention budget is at most one contended atomic per commit | §5 |
| A5 | Engines are constructed from a `TableDescriptor` built by the scope host from the catalog; engines never read the catalog | §4.10 |
| A6 | Segment and page allocation is a scope-level facility (`ScopePages`): segment id counters, page hints, and the heap's free space map; ids are never reused (S18) | §7 |
| A7 | The key contract (`KeyBytes`/`KeyRange`, WP1 F4 / WP5 D13) stays in `cascade-pgtypes` until `cascade-datum` exists; the 05-access → 07-sql dependency edge is a documented, expiring exception | §3.2 |
| A8 | The visibility seam is a narrow `VisibilitySource` contract (status resolution for uncommitted words) defined in this layer's API crate, implemented today by the runtime's `txnmgr` status table (WP2 D2) and by `cascade-scope-txn` when the L4 split lands | §4.5 |
| A9 | `cascade-btree` is the tree mechanism shared by the IOT primary tree and secondary index trees; it knows nothing about transactions or visibility (architecture §4.6 verbatim) | §8.1, btree design §1 |

Inherited from WP5 without change: D1/D7 (enum dispatch), D2 (opaque statement-scoped locators), D3 (outcomes as values, engine-side bounded waits), D4 (one visibility substrate), D5 (engine-maintained secondary indexes), D6 (read surface and the RC re-check recipe), D9 (infrastructure-only errors, poisoned txns), D10 (statement boundary), D11 (scan protocol), D12 (speculative-insert upsert), D13 (key contract co-owned with WP1), D14 (DDL = physical phases only), D15 (`RowRef` owns its buffer; lazy TOAST). Section 14 maps each to where it now lives.

---

## 3. Crates and Dependencies

### 3.1 The target crate set

| Crate | Status | Contains | Depends on |
| :--- | :--- | :--- | :--- |
| `cascade-access-api` | To create | The contract: `AnyTable`, `TableDescriptor`, scan protocol, outcomes, errors, `VisibilitySource`, `Maintenance`, capabilities, the in-memory fake and conformance suite | `cascade-types`, `cascade-pgtypes` (A7), `cascade-telemetry` |
| `cascade-page` | Landed | The page layer: prologue, kind registry, the heap format (kind 1), and — as the engines land — the btree and FSM typed views (A3) | `cascade-types` |
| `cascade-btree` | Stub | The B+tree mechanism: descent, OLC latching, SMOs, byte-key operations over `cascade-page`'s btree views | `cascade-types`, `cascade-page`, `cascade-bpm`, `cascade-storage-api` (page fetch seam) |
| `cascade-heap` | Stub (T6.4 pending) | The heap engine: DML, visibility walks, prune/vacuum scheduling hooks, FSM use, TOAST | `cascade-types`, `cascade-page`, `cascade-bpm`, `cascade-access-api`, the log/mini-txn seam |
| `cascade-iot` | To create | The IOT engine: leaf row semantics, undo chains with `cascade-undo`, purge | `cascade-access-api`, `cascade-btree`, `cascade-undo`, the log/mini-txn seam |
| `cascade-index` | To create | Secondary indexes for both engines: entry semantics, per-entry visibility, unique checks, the index-kind seam for hash/GIN/GiST/BRIN/vector | `cascade-access-api`, `cascade-btree` |
| `cascade-sequence` | To create | Sequences, `serial`, identity: cached ranges per scope, logged allocation, never reused after failover | `cascade-access-api`, the log seam |
| `cascade-maintenance` | To create | Scheduling of undo purge, heap prune and vacuum within resource budgets; undo-limit enforcement | `cascade-access-api` (`Maintenance`), `cascade-watermark` (when it exists) |
| `cascade-temptable` | To create | Session-private temporary tables implementing the same contract; no log, no version words beyond session-local savepoints | `cascade-access-api` |

Crate creation order follows the migration table (Section 10). `cascade-access-api` is created first: it freezes the surface the engines and the executor code against, exactly as WP5 §8 prescribed for T6.4.

### 3.2 Layering facts this layer must state

- **`cascade-page` lives in this directory but is an L1 format authority.** The architecture's layer map puts every page layout in `cascade-page` (L1, "every page layout and its typed views"); the repo directory is historical (the page mutation design §1 justifies the placement by dependency topology: the heap format sits below both the engine and the storage-side consumers). The repo layer number of the *directory* must not be read as the architecture layer of the *crate*.
- **The key contract edge (A7, expiring).** WP1 F4 landed `KeyBytes`/`KeyRange`/`IndexEntry::Locator` in `crates/07-sql/cascade-pgtypes`. The architecture's long-term home is `cascade-datum` (L1: "values, rows and order-preserving key encoding"), which does not exist yet. Until that move, `cascade-access-api` depends on `cascade-pgtypes` — an upward edge (05 → 07) that violates the repo layering rule and is documented here as deliberate and expiring: it dies exactly when the key contract moves to `cascade-datum`, and the layering CI check (architecture Rule 7) gains this edge as a named exception until then. No access-layer type may name any other `07-sql` item.
- **The visibility seam (A8).** Engines depend on `Snapshot` and `VisibilitySource`, not on the runtime. Today both live behind the runtime's `txnmgr` (WP2 D2); the L4 split (architecture §4.5, `cascade-txn-api`) re-homes them without an engine change, because engines name only the contract.
- **The log/mini-txn seam.** Engines express page changes as record groups plus page closures through one injected type (Section 6.1). Its target owner is `cascade-pagecache` (the mini-transaction type, architecture §4.4); its interim owner is the runtime's `WalWriter` + BPM `write_logged` path. Engines name the seam, never `WalWriter` and never a `LogSink`.
- **`cascade-mvcc` and `cascade-undo` are pre-pivot stubs.** Their `AI/CONTEXT.md` files describe the merged pre-pivot world and are being redesigned in the 04-txn pass; `cascade-heap`'s dependency on them (in today's `Cargo.toml`) is dead weight that the first heap landing drops (Section 10). `cascade-undo` returns as a *dependency of `cascade-iot`*, not of `cascade-heap` — the heap has no undo (WP1 F2; page mutation design §2 boundary).

---

## 4. The Contract: `cascade-access-api`

### 4.1 Dispatch — enum, not `dyn` or generics (WP5 D1, kept)

```rust
pub enum AnyTable { Iot(cascade_iot::IotTable), Heap(cascade_heap::HeapTable) }
```

`AnyTable` re-exposes the whole surface as inherent `async fn`s that `match self` and delegate — two arms, negligible cost, `!Send`-native, matching the runtime's dispatch idiom. Rejected (WP5 §3, still valid): `async-trait` (per-call allocation, erases the `!Send` story), manual boxed-future vtables (unsafe-adjacent boilerplate for two variants). The executor holds mixed collections of tables, which is why generics are wrong here while they are right for the storage seam's per-call `&impl PageStore`.

`Maintenance` gets the same treatment (WP5 D7):

```rust
pub enum Maintenance { Iot(IotUndoPurge), Heap(HeapPruneVacuum) }
impl Maintenance {
    /// One bounded unit of background work, on the calling core.
    pub async fn step(&mut self, budget: Budget) -> Progress;
}
```

The scheduler above it is `cascade-maintenance` (Section 9.4); version 1's driver is the runtime housekeeping tick. No table-wide stops, ever (storage-layer-design §12.1 principle 7).

### 4.2 Substrate types

Owned elsewhere, named here (WP5 §5, updated to the landed types):

```rust
// cascade-types (landed): DatabaseId, ScopeId, Epoch, TxnId, Hlc, VersionWord,
//                         Lsn, PageId, PageNo, SegmentId, OpCtx, AlignedBuf, Datum
// cascade-pgtypes (landed, A7): KeyBytes, KeyRange, Locator (Pk | Heap), IndexEntry
// cascade-page (landed): NextWord, NEXT_* flags, SlotId
pub struct SchemaVersion(pub u64);   // leased schema version (catalog layer)
pub struct IndexId(pub u64);         // stable id (catalog layer assigns; engine-internal use)
pub struct ColId(pub u16);
pub enum StorageKind { Iot, Heap }
pub struct Tid { pub page: PageId, pub slot: u16 }   // heap row address; packs relation-locally
```

`Snapshot` moves to this crate's contract (A8) with the visibility rule (§4.5). `Tid` stays declared in `cascade-heap` (it exists there today) and re-exported by the API crate, so the heap address type keeps one definition.

### 4.3 The surface

```rust
impl AnyTable {
    // planner face
    pub fn handle(&self) -> TableHandle;            // TableHandle { rel, kind }
    pub fn capabilities(&self) -> &AmCapabilities;

    // writes — the calling core must be in the scope's write core set (§5);
    // the engine maintains secondary indexes inside every write (WP5 D5)
    pub async fn insert(&self, txn: &mut TxnCtx, row: &Row) -> Result<WriteOutcome, TamError>;
    pub async fn update(&self, txn: &mut TxnCtx, loc: &RowLocator, new: &Row,
                        changed: &ColumnSet) -> Result<WriteOutcome, TamError>;
    pub async fn delete(&self, txn: &mut TxnCtx, loc: &RowLocator) -> Result<WriteOutcome, TamError>;
    pub async fn lock_row(&self, txn: &mut TxnCtx, loc: &RowLocator,
                          mode: RowLockMode, wait: WaitPolicy) -> Result<Locked, TamError>;

    // upsert — speculative insertion, one arbitration point (WP5 D12)
    pub async fn insert_speculative(&self, txn: &mut TxnCtx, row: &Row, index: IndexId)
        -> Result<SpeculativeResult, TamError>;
    pub async fn complete_speculative(&self, txn: &mut TxnCtx, token: SpecToken)
        -> Result<RowLocator, TamError>;
    pub async fn abort_speculative(&self, txn: &mut TxnCtx, token: SpecToken) -> Result<(), TamError>;

    // reads (WP5 D6, D11)
    pub async fn fetch(&self, snap: &Snapshot, loc: &RowLocator,
                       proj: &Projection) -> Result<Option<RowRef>, TamError>;
    pub async fn fetch_latest_committed(&self, txn: &TxnCtx, loc: &RowLocator)
        -> Result<Option<(RowLocator, RowRef)>, TamError>;
    pub fn seq_scan(&self, snap: &Snapshot, proj: &Projection, opts: SeqScanOpts) -> TableScan;
    pub fn index_scan(&self, snap: &Snapshot, index: IndexId, range: KeyRange, dir: Direction,
                      proj: &Projection, index_only: bool) -> TableScan;

    // DDL physical phases (WP5 D14) and maintenance (WP5 D7)
    pub async fn build_index(&self, txn: &mut TxnCtx, def: &IndexDef) -> Result<(), TamError>;
    pub async fn truncate(&self, txn: &mut TxnCtx) -> Result<(), TamError>;
    pub fn maintenance(&self) -> Maintenance;
    pub fn stats(&self) -> TableStats;
}
```

`TxnCtx` is the session-to-engine transaction context: the scope's `ScopeId` and `Epoch`, the writer's `TxnId`, and the statement context (§4.6). Its construction and internals belong to the transaction layer (WP2 today, `cascade-scope-txn` at the L4 split); this contract pins only what engines consume. The full outcome, conflict, lock, scan and stats shapes are WP5 §5 verbatim (`WriteOutcome`, `Conflict`, `RowLockMode`, `WaitPolicy`, `Locked`, `ScanRow`, `TableScan` with `rescan`/`mark`/`restore`, `AmCapabilities`, `TableStats`) — they were accepted and are not restated here. Two corrections the landed world forces:

- `RowLocator::Tid(Tid)` uses the relation-local packing of WP1 F4's `Locator::Heap { page_no, slot }` on the wire and in index entries; the in-memory `Tid` carries the full `PageId` for BPM addressing. Engines convert at the boundary; nothing else does.
- Every operation takes the caller's `&OpCtx` (tenant, span, deadline — Rule 8) as a parameter on the hot path, per the workspace invariant; the sketch above omits it only for readability, as the storage contracts' sketches do.

### 4.4 Locators, outcomes and waits (WP5 D2, D3 — kept)

`RowLocator` (`Pk(KeyBytes)` / `Tid(Tid)`) is opaque to the executor: stored, passed back, never inspected. Validity is same-statement, same-txn; movement is reported only via `WriteOutcome::Done { locator }`. `StmtCtx.serial` stamps locators in debug builds as a staleness tripwire.

`WriteOutcome` carries conflicts and violations as values; only infrastructure failures are `Err` (§4.9). The wait table is WP5 D3's, with its implementation named: `Wait` blocks bounded on the holder's resolution through the `VisibilitySource` (§4.5) and the lock manager's wait queues (`cascade-lock` at the L4 split; the runtime's status-table wait today). Bound expiry returns `Conflict::InProgress` — bounded waits prevent unbounded stalls. Reads signal not-visible with `Option::None`; writes with `WriteOutcome::NotVisible`; the executor maps them to skip-row and RC re-check respectively.

### 4.5 The one visibility rule (WP5 D4, anchored)

```rust
pub struct Snapshot {
    pub read_ts: Hlc,        // statement snapshot (RC: per statement; RR/SSI: txn's)
    pub own: Option<TxnId>,  // the reader's own txn, whose uncommitted writes are visible
}

pub enum Visibility {
    Visible,
    CommittedAfterSnapshot { commit_ts: Hlc },
    InProgress { holder: TxnId },
    Aborted,
}

pub enum RowVisibility {
    Live,                                   // materialize the version
    NotBornYet,                             // creator invisible → walk to older / skip entry
    Deleted { in_progress: Option<TxnId> }, // walk to older / skip entry
}

impl Snapshot {
    /// The word-level rule: committed commit_ts ≤ read_ts, or own uncommitted write.
    pub fn visible(&self, word: VersionWord, src: &dyn VisibilitySource) -> Visibility;
    /// The ONLY row/entry composition. Engines and index code call this, nothing else.
    pub fn row_visible(&self, creator: VersionWord, deleter: Option<VersionWord>,
                       src: &dyn VisibilitySource) -> RowVisibility;
}

/// Resolution of uncommitted words (A8). Target owner: cascade-scope-txn's status
/// table; interim owner: the runtime txnmgr's TxnStatusTable (WP2 D2).
pub trait VisibilitySource {
    /// The outcome of an uncommitted-tagged word's txn. A committed or empty word
    /// never consults it.
    fn status(&self, xid: TxnId) -> TxnStatus;   // InProgress | Committed { ts } | Aborted
}
```

WP5 D4's composition is kept byte-for-byte in spirit: `row_visible(creator, None)` is exactly the index-entry rule of HLD v2 §13. What this document adds is the seam: `visible` needs the status resolution for uncommitted words, and the source is injected (A8) so engines stay runtime-free and test against a fake. The executor never sees version words, undo pointers, chains, or page bytes; `RowRef` presents decoded, already-visible data (WP5 invariant 4).

### 4.6 Statement boundary (WP5 D10 — kept)

```rust
impl TxnCtx {
    pub fn begin_statement(&mut self) -> &mut StmtCtx;  // RC: fresh Snapshot; RR/SSI: reused
    pub fn end_statement(&mut self);                    // releases statement-scoped state
}
pub struct StmtCtx {
    pub snapshot: Snapshot,
    pub serial: u64,                  // locator tripwire source (WP5 D2)
    pub schema_version: SchemaVersion // validated at write time (WP5 D14)
}
```

Speculative tokens must resolve before `end_statement`; engines abort stragglers at statement teardown (WP5 invariant 9).

### 4.7 Scan protocol (WP5 D11 — kept)

A scan is a pull iterator with join support; items carry locators (`ScanRow { row: RowRef, loc: RowLocator }`) so scanned UPDATE/DELETE targets need no second probe. `TableScan` is enum-dispatched (`Iot(IotScan) | Heap(HeapScan)`), construction is infallible and does no I/O (the first `next()` opens), visibility filtering is engine-internal via `row_visible`, `rescan` re-reads at the same snapshot, `mark`/`restore` support merge-join on ordered scans only, and dropping a scan without `close` releases pins synchronously. No page pin is ever held across an await. `SeqScanOpts`/`IndexScanOpts` carry `ssi: Option<SsiReporter>` — the WP9 hook, reserved.

### 4.8 Speculative insertion (WP5 D12 — kept)

`insert_speculative` / `complete_speculative` / `abort_speculative` with `SpeculativeResult::{Installed, KeyBusy, UniqueViolation}` — one engine operation, one arbitration point; ON CONFLICT composition lives in the executor. The two-branch WAL work happens below the API. Engine-side mechanics per engine: heap (Section heap design §4) and IOT (btree design §5).

### 4.9 Errors and poisoned transactions (WP5 D9 — kept)

```rust
#[non_exhaustive]
pub enum TamError {
    Io(std::io::Error),          // log/storage failure — outcome unknown
    Fenced { scope: ScopeId },   // epoch lost — outcome unknown
    SchemaStale { min_leased: SchemaVersion }, // write under a retired schema version
    Poisoned,                    // this txn already failed once
}
```

Any `Io` or `Fenced` **poisons** `TxnCtx`: every later operation returns `Poisoned` without touching storage. The executor aborts; the failed txn's outcome is recovery's to decide. This is the access-layer half of the log-failure sequence (storage-layer-design §8.1): the epoch check that step 2 fences with is the `TxnCtx`'s `Epoch`, carried on every engine call, and a page-cache access for a fenced scope refuses before touching a page. The poisoning rule is what makes that sequence's step 2 a one-line check instead of a per-engine cleanup.

### 4.10 TableDescriptor and construction (A5)

```rust
pub struct TableDescriptor {
    pub rel: ObjectRef,              // stable UUID (catalog layer assigns)
    pub kind: StorageKind,
    pub scope: ScopeId,              // v1: the database's one scope
    pub columns: Vec<ColumnDef>,     // ColId, TypeOid, datum codec, nullability
    pub key: KeyShape,               // PK columns for IOT; None for a heap table
    pub indexes: Vec<IndexDef>,      // secondary indexes, in build order
    pub toast: ToastPolicy,          // threshold + policy (heap design §8)
}
```

An engine instance is constructed from a descriptor and a `TableEnv` (the injected environment: the mini-txn seam, the page cache/pool handles, `VisibilitySource`, the lock-wait seam, the scope's `ScopePages` (A6)). Engines never call the catalog. The scope host builds descriptors from the catalog; the catalog's own system tables use hard-coded bootstrap descriptors (architecture §4.6). Descriptor construction is infallible except for `UnknownType`/`BadKeyShape` validation errors — descriptor *changes* (DDL) arrive as a new descriptor at the next statement boundary, gated by `StmtCtx.schema_version`.

### 4.11 The in-memory fake and conformance suite (Rule 6)

`cascade-access-api` ships, in one crate with the contract: a **generic conformance suite** (one test function per promise in this Section, written as generic functions over `AnyTable`-like dispatch) and a **fake engine** — a simple in-memory multi-version table with no pages and no log: rows keyed by `KeyBytes` or `Tid`-like handles, versions kept as `(creator, deleter, payload)` triples resolved through the same `row_visible`, conflicts synthesized from a scripted `VisibilitySource`. Both engines and the fake pass the same suite (the fake is also the executor's test double and `cascade-temptable`'s foundation). The suite checks: outcome values under every `WaitPolicy`, RC re-check recipes, locator movement, scan protocol semantics (rescan/mark/restore/drop), speculative arbitration including the two-inserter race, poisoning, and visibility composition against a scripted status source.

---

## 5. Execution Model (A4)

**A scope is a shared-memory engine across its write core set** (architecture §4.8). Every call — read or write — may arrive on any core of the scope's write core set: the union of the core sets of the table's database and, when one exists, the table itself. WP5 D8's question ("who reads across cores?") is answered one level up: `cascade-dataplane` routes whole plan fragments to cores in the fragment's core set, and read-write sessions are proxied whole to the leader node at accept. Inside this layer there is no core ownership, no message-to-owner routing, and no per-scan channel plumbing.

Consequences, pinned:

1. **All engine structures touched by the write core set are `Send + Sync` shared structures**: the log writer, the transaction status, the lock wait queues, the page cache — shared below this layer; the engines' own per-scope state (descriptors, FSM, free-page hints, sequence caches) joins them. Any of these that the commit path uses is model-checked with `loom` (architecture Rule 11).
2. **The contention budget** (architecture §4.8, kept as this layer's design requirement): a commit uses at most one contended atomic operation and takes no lock. Engines meet it because per-row conflicts are resolved through version words (read, compare, stamp — atomics on the row's own bytes under the page's exclusive pin), not through a central lock table; the one contended atomic is the log writer's LSN reservation (storage-layer-design §12.3, one `fetch_add` per scope).
3. **Page-level exclusion is the BPM pin protocol**: mutators hold the frame's exclusive pin; readers take copy-out snapshots; no pin is held across an await. Two writers to the same *page* serialize on the frame; two writers to the same *row* conflict through version words into `WriteOutcome::Conflict`, never through page latching.
4. **Misrouting is a programming error.** A call arriving off the scope's write core set, or on a stale epoch, is a debug assertion (`TxnCtx` carries `ScopeId` + `Epoch`; the engine checks both on entry). It is not a runtime path — the dataplane's promises (architecture §4.8) guarantee placement.
5. **Cross-scope reads** (a statement touching two tables of the same database — one scope in v1, several after a split) take one read timestamp chosen by `cascade-txn-coord` (architecture §4.5); each fragment runs on its own scope's core set. The coordinator's rules, not this layer's.

*Supersession note.* WP5 D8 decided message-to-owner reads (Option A) on 2026-11-14, when the page cache was still per-core (HLD v2 §8). Architecture v0.10.2 (§1.1 row 2, §4.8, D1) and ADR-0003 replaced the per-core cache with the shared per-NUMA pool and made a scope a shared-memory engine; the runtime landed that pool at T6.2. Option A's premise is gone, so the decision is superseded, not merely amended — and with it go the HLD v2 §10 sentence "all calls run on the core that owns the scope" and the page mutation design's "runs serialized on the owning core" phrasing (Section 12).

---

## 6. The Write Path: Mini-Transactions, Redo and Page Formats

### 6.1 The mini-transaction is the only write primitive (A1)

Every page change in every engine composes three steps as one unit:

1. **Reserve and fill** one contiguous record group in the scope's log — the group ends on a group-end flag (S16), so the filled watermark, the durable LSN and every replay boundary fall on mini-transaction edges (storage-layer-design §12.3, §4.3 promise 5).
2. **Apply** the records through the redo path (A2) to the exclusively-pinned frames the change touches.
3. **Stamp** each changed frame's page LSN with the group's end LSN (S17: end LSN, never start).

Order is append-then-apply (HLD v2 "eager local apply", kept); a commit waits for durability separately, and no page whose LSN is above the durable LSN is ever evicted (storage-layer-design §12.5). The heap's undo-free design and the IOT's undo-before-leaf ordering both live inside this unit: an IOT mini-transaction's group is `[undo append][leaf change]` in that order, which makes the §11 invariant ("undo is durable no later than the leaf change that depends on it") hold by construction.

Engines see this as one injected type — the **mini-txn seam**:

```rust
/// The only way an engine changes pages. Target owner: cascade-pagecache's
/// mini-transaction (architecture §4.4). Interim owner: the runtime's
/// WalWriter + BPM write_logged adapter.
pub trait MiniTxnScope: Send + Sync {
    fn begin(&self) -> MiniTxn<'_>;
}
pub struct MiniTxn<'a> {
    /// Record a redo payload for a page this txn will mutate. `apply` runs
    /// at commit of the txn, in record order, on the pinned frames.
    pub fn record(&mut self, page: PageId, payload: RedoPayload);
    /// Mutate a pinned frame now; the mutation is applied to the log's copy
    /// of truth only via `payload` at replay. (Copy-out guard applies.)
    pub fn mutate<R>(&mut self, page: PageId, f: impl FnOnce(&mut [u8; PAGE_SIZE]) -> R) -> R;
    /// Append the group; resolves at durability of the group end (commit-only).
    pub async fn durable(self) -> Result<Lsn, TxnError>;
}
```

The exact shape is the mini-transaction module's to finalize (architecture §4.4: only it can create the capability that permits a page change). This layer pins the *contract engines hold*: engines name `MiniTxnScope`, never `WalWriter`, never `LogSink`, never a `PageStore`. The conformance fake implements the seam in memory (append to a `Vec<Vec<u8>>` log, apply to in-memory pages), which is what lets engines pass their suites with no storage at all.

### 6.2 Redo ownership (A2) and page formats (A3)

The redo contract (storage-layer-design §12.4) is one-way: self-contained per page, deterministic, versioned, idempotent by LSN (apply only if record start ≥ page LSN). Three crates divide it, per the architecture's own assignments (§3.2 L1 table, Rule 5):

- **`cascade-wal-format`** owns the encoding and decoding of every record payload (Rule 5: one owner per byte format). Engine record *families* are designed by the engine's design doc (this layer's child docs) but their bytes live here, golden-tested.
- **`cascade-redo`** owns the single dispatch entry `apply(page, record, lsn)`, today one `match` in `apply_record`, growing to one module per record family as families land. It keeps applying every version it has ever accepted.
- **The format authorities own the byte transforms.** The heap family's transforms are `cascade-page`'s `redo` module (page mutation design D4 — `apply_heap_insert`, `apply_delete_mark`, `apply_relink`, `apply_prune`, `apply_slot_free`); the btree family's transforms operate on `cascade-page`'s btree typed views. `cascade-redo`'s family modules are thin dispatch adapters over them — the byte logic is written once, at the authority. *Status 2026-11-26:* the heap-insert family's apply already calls `PageMut::{init, wrap, append_cell, set_page_lsn}` at the authority, as this rule wants; D4's `redo` module is where those calls get a named home. The free-space arithmetic, however, is currently *mirrored* in `cascade-redo` (`fits`, `SegmentFill`, `MAX_CELL_LEN`) rather than asked of `cascade-page` — which is how the fit-rule defect in §12 reaches both crates. One shared predicate at the format authority closes it.

**A3 — btree page formats live in `cascade-page`.** The architecture's `cascade-page` row names "typed views for index-organized leaves" among its layouts (L1), and `cascade-redo` (L1) cannot depend on `cascade-btree` (L5) without inverting the layer map. So the btree *bytes* — branch nodes, IOT leaf rows, index leaf entries, their headers and their parse — are `cascade-page` kinds under the prologue registry (page mutation design L1), and `cascade-btree` owns the tree *logic* over those typed views, exactly as the architecture's crate table says ("a B-tree over the page cache … knows nothing about transactions"). This amends the page mutation design's §2 boundary line ("the btree leaf/branch formats will live in cascade-btree") and `cascade-btree`'s CONTEXT invariant 3; the section-numbering rule of the architecture (§1.1: it wins) applies, and the page design's own landing plan (§7) already reserves the CONTEXT sync at btree landing. The "cells are never unified across kinds" boundary (page mutation design §2) is untouched: the IOT leaf row and the index entry remain different cell shapes — they just both live in the layer crate.

**Page-kind registry.** Kinds are assigned by each landing authority and frozen with golden tests (page mutation design L1). Assigned today: 1 = heap. Proposed by this layer's child docs, frozen at their landing: btree branch, IOT leaf, index leaf (btree design §2), FSM (heap design §7), undo (the `cascade-iot` design, WP6). An unknown kind is `WrongKind`, never corruption.

---

## 7. Segments and Page Allocation (A6)

Pages are named `PageKey { segment, page_no }` within a scope (storage contracts §4.2) and `PageId { db, segment, page_no }` in the BPM (both exist; engines convert once at the seam). Allocation is a per-scope facility, injected as part of `TableEnv`:

```rust
pub trait ScopePages: Send + Sync {
    /// Allocates a fresh segment for a relation (new table, index build,
    /// TOAST relation). Segment ids come from a scope counter that survives
    /// restarts; never reused (S18).
    fn new_segment(&self, ctx: &OpCtx<'_>) -> SegmentId;
    /// The engine's append hint for a segment: the next page_no to try.
    /// Monotonic per segment; contention on it is one fetch_add (A4's budget).
    fn page_hint(&self, segment: SegmentId) -> PageNo;
    /// Extends a segment's materialized pages past the hint (bulk load, and
    /// the heap's tail-extension path).
    fn extend(&self, ctx: &OpCtx<'_>, segment: SegmentId, count: u32) -> Result<PageNo, TxnError>;
}
```

The heap's free space *within* allocated pages is its own FSM (heap design §7); `ScopePages` knows only segment extents. A truncated or dropped relation retires its segment ids for good (S18) — the log carries a segment-drop record family the page store applies by discarding the segment's files (storage-layer-design §4.2), and a later relation must never receive a retired id. `truncate` (WP5 D14) is: log the segment-drop, allocate a fresh segment, swap the descriptor's segment under the schema-version gate.

---

## 8. The Engines

### 8.1 `cascade-btree` (A9) — the tree mechanism

A B+tree over the page cache with byte keys and byte values, shared by the IOT primary tree and secondary index trees; it knows nothing about transactions or visibility (architecture §4.6, verbatim — version words inside leaf payloads are opaque bytes to it). It owns: descent, the OLC latch protocol (the full latch lands here, succeeding the BPM v1 pin word for tree nodes), split/merge as one-mini-transaction SMOs, ordered iteration with mark/restore, and the byte-level update-in-place operation IOT leaves need. Full design: [`cascade-btree/AI/btree-design.md`](../cascade-btree/AI/btree-design.md).

### 8.2 `cascade-heap` — the compatibility engine

Append-only row versions on kind-1 pages; every update writes a new version; HOT chains never cross a page; prune is plan-carried (the format's D2) and vacuum is incremental per page. The format half (slot states, prune plans, redo primitives) is the page mutation design D1–D6; the engine half — DML paths, visibility walks, FSM, TOAST, vacuum scheduling, and the T6.4 interim slice — is [`cascade-heap/AI/heap-design.md`](../cascade-heap/AI/heap-design.md).

### 8.3 `cascade-iot` — the default engine (contract-level; detailed design is WP6's)

Rows in the leaves of the PK B-tree, newest version in the leaf, updates in place; old versions as delta undo records chained by `roll_ptr`; undo in undo pages of the same scope, through the page cache and the log like any other page (HLD v2 §11). Operations map to the tree as: insert = leaf insert + insert-undo record; update = undo delta append + in-place leaf overwrite (size permitting — btree design §5.3); delete = delete-mark + undo; PK update = delete-mark old key + insert new key, locator moves; read = leaf version, walk `roll_ptr` + apply deltas until `row_visible` says `Live`; rollback = walk the txn's undo chain, restore each row, log a CLR per step, then the abort record; purge = remove delete-marked rows and free undo at segment granularity once no snapshot can need them (HLD §11 invariants, kept). The mini-transaction ordering (§6.1) makes undo-no-later-than-leaf structural.

**What this document fixes for `cascade-iot` now, so its detailed design cannot drift:** it implements `AnyTable` with `RowLocator::Pk`; its secondary-index maintenance goes through `cascade-index` (D5: skip when no indexed column changed — the PK is not secondary); its undo record format is WP6's to freeze (one-way door — WP1 §open-questions row); its purge scheduler is `Maintenance::Iot(IotUndoPurge)`; and its leaf row header is `VersionWord` (F1) + `roll_ptr` + flags, hosted as a `cascade-page` kind (A3). Its undo segment layout, recovery discovery of in-flight undo, and the roll_ptr encoding are WP6 open questions, named here so they are not rediscovered: they need answers before the first undo page kind is frozen.

---

## 9. Indexes, Sequences, Temporary Tables, Maintenance (contract-level)

### 9.1 `cascade-index` — secondary indexes

Entries are `[key bytes][locator][create word][delete word]` (WP1 F4 logical shape; `Locator::Pk` for IOT tables, `Locator::Heap` for heap tables). Entry visibility is `row_visible(create, Some(delete))` — index-only scans need no visibility map, on both engines (HLD §13). Maintenance rules (kept from HLD §13 + WP5 D5): insert row → insert entry (create = writer's txn); indexed-column update → delete-word old entry + insert new; no indexed column changed → no index write; delete row → delete-word every entry. Unique check: a key conflicts if an entry with that key is visible-live or uncommitted-by-another — `Conflict::InProgress` for the former, `UniqueViolation` for the latter; the speculative protocol (D12) rides on it. Cleanup: entries whose delete word (or aborted create) is older than the cleanup watermark are removed by index vacuum, before the heap slot they point at is freed (heap invariant: slot reuse only after index cleanup — page mutation design D1's transition table).

The index *trees* are `cascade-btree`'s (A9) with the index-leaf kind; `cascade-index` owns entry semantics, unique checks, and the per-kind seam: an internal `IndexAccessMethod` contract (architecture §4.12, index kinds row) behind which hash/GIN/GiST/BRIN and the native `pgvector`/`pg_trgm` kinds land in later phases — each carrying its own visibility, as B-tree entries do.

### 9.2 Sequences

`cascade-sequence` allocates monotonically increasing values with cached ranges per scope; range allocation is a logged record (one log record per range, not per value), so a failover never reuses a value — the new owner resumes from the last *durable* range record. It implements a narrow `Sequence` contract consumed by the executor and by `serial`/identity column defaults; values are `i64`/`u64`-shaped, encoded into keys through the F4 tag scheme when they participate in an index.

### 9.3 `cascade-temptable`

Session-private temporary tables implementing the same `AnyTable` contract (the fake engine of §4.11 is its foundation): rows visible only to the owning session, no version words beyond session-local savepoint state, no log, no redo, no scope — so writes to a temporary table never count against the single-scope write rule (architecture §4.6). Rows live in session memory and spill to `cascade-tempstore` past a budget. They disappear when the session ends; nothing in them is ever recovered.

### 9.4 `cascade-maintenance`

The scheduler that drives `Maintenance::step` across relations within resource budgets: an in-memory work list per scope, budgeted per tick, so background cleanup never exceeds its CPU/IO share and never stops a table (§12.1 principle 7). It owns the undo-limit enforcement (product definition §4.1: maximum transaction age and maximum undo size, reporting through `cascade-sysview`) once `cascade-watermark` exists; until then the cleanup watermark is the scope's oldest active read timestamp (single node: the oldest open snapshot in the process — the watermark's cross-scope/replica learning is `cascade-watermark`'s design, architecture §4.5).

---

## 10. Migration from the Current Code

Each step keeps the workspace green (the storage migration's discipline, §9). The table names the design section each step lands.

| Step | What changes | Design ref |
| :--- | :--- | :--- |
| **L0. The API crate** | Create `cascade-access-api`: the §4 surface with the fake and conformance suite; `AnyTable::Heap` arm wired to the stub engine. The executor's debug stand-in keeps running unchanged. | §4 |
| **L1. T6.4 heap interim slice** | `HeapView`-shaped insert/scan through the BPM as the M6 plan defines it (xmin stamped, xmax/next reserved zero, no MVCC, `RowVisibility::Live` only); speculative API reserved as compile-checked signatures. Lands *against* the API crate, not beside it. | heap design §11 |
| **L2. The MVCC milestone** | Slot states (page D1), engine-side prune plans (page D2), visibility with `VisibilitySource`, real `WriteOutcome` conflicts, poisoning, statement boundary. The wire suites pass unchanged until MVCC behavior is deliberately pinned (WP5 §8's own rule). | heap design §11 |
| **L3. Engine hygiene** | Drop `cascade-heap`'s dead `cascade-mvcc`/`cascade-undo` deps; fix the stale crate description; CONTEXT sync (this document's Section 12 list). | §3.2 |
| **L4. Mini-txn seam** | Introduce the `MiniTxnScope` contract; the runtime adapts `WalWriter` + `write_logged` behind it; engines stop calling the BPM write path directly. No behavior change — this is the seam that makes M8 a one-crate change. | §6.1 |
| **L5. Storage M7/M8 (upstream steps)** | The page store lands (`cascade-redo` heap family first); the BPM miss path switches to `PageSource` at the durable LSN; compute writeback and the checkpointer die; engines need no change — they hold the seam. | storage-layer-design §9 |
| **L6. `cascade-btree`** | The tree over the new page kinds (branch/IOT leaf/index leaf), OLC latching, SMOs, with `cascade-page` gaining the frame/heap module split *first* (page design L3 — prerequisite). | btree design §11 |
| **L7. `cascade-iot` + undo** | The IOT engine over the tree; `cascade-undo` redesigned (WP6) for undo pages in the page cache; `cascade-redo` btree + undo families. | §8.3 |
| **L8. `cascade-index`** | Secondary indexes for both engines; entries carry Tids from this point, so HEAP_INSERT's redo record must carry its slot (the page design §7 forward obligation — resolved in heap design §9). | §9.1 |
| **L9. Sequence, maintenance, temptable** | The three small crates behind their §9 contracts. | §9 |

Steps L0–L3 are independent of the storage migration's M7/M8; L4 is the coupling point and must land before M8 touches the BPM.

---

## 11. Testing

| Level | What it checks | How |
| :--- | :--- | :--- |
| Golden bytes | Every page kind's layout, every record payload | Byte-literal tests at the format authority (`cascade-page`, `cascade-wal-format`), one per (kind, version) and (type, version) |
| Contract conformance | Every promise of §4 | One generic suite against the fake and both engines (Rule 6); the fake is the executor's test double |
| Visibility property tests | `row_visible` composition over randomized txn interleavings; RC/RR/SSI outcomes against a scripted status source | Property tests in the API crate, shared by engines |
| Replay equivalence | Redo determinism: for random DML workloads, pages rebuilt by applying the recorded groups to empty images equal the live pages the leader produced | Differential harness over the mini-txn fake's log + `cascade-redo`; the storage layer's own differential test (§10) reuses it end-to-end |
| Engine differential | Both engines agree on observable results (rows, outcomes, visibility) for the same operation sequence | Generic workloads run through `AnyTable` against both arms |
| Concurrency | The shared structures survive interleaving | `loom` on the engines' shared per-scope state (Rule 11): page hints, FSM freelists, sequence caches; the BPM and log writer carry their own |
| MVCC end-to-end | Reader/writer races, prune vs snapshot, vacuum vs index entries, HOT invariants | Seeded interleaving tests through the fake `MiniTxnScope` + fake `VisibilitySource` |
| Crash and fence | Outcome-unknown behavior: `Io`/`Fenced` at each mini-txn step poisons cleanly; recovery replays to the durable prefix | Fault-injected seam fake; the storage layer's crash tests cover the bytes |
| System | Wire behavior unchanged across engine swaps | The runtime's existing query/commit/txn wire suites pin observables |

---

## 12. Corrections Proposed Elsewhere

The architecture-wins rule (architecture §1.1) and the spec↔crate-doc sync discipline require these mirror fixes; they land with the steps that touch each file.

| File | Drift | Fix |
| :--- | :--- | :--- |
| `crates/05-access/cascade-heap/AI/CONTEXT.md` | Describes the pre-pivot engine (in-place updates, undo-ref row header, "embodies Anti-VACUUM") — that is the IOT's design now; heap is version-per-update | Rewritten in this pass, pointing at heap design |
| `crates/05-access/cascade-btree/AI/CONTEXT.md` | Invariant 1 cites the retired owner-core mutator rule; invariant 3 hosts btree bytes in `cascade-btree` | Rewritten in this pass, pointing at btree design (A3, A4) |
| `crates/05-access/cascade-page/AI/storage-page-mutation-design.md` | §2 assigns btree formats to `cascade-btree`; the concurrency paragraph says "serialized on the owning core" | Superseded by A3 and §5; both lines are corrected when the frame/heap module split lands (the page design's own §7 landing plan), not edited mid-flight here |
| `crates/05-access/cascade-page/AI/CONTEXT.md` | Role line and Out-of-Scope name `cascade-btree` as the index-layout home | Same sync point as above |
| `crates/05-access/cascade-page/src/lib.rs` + `AI/CONTEXT.md` | **Fit rule omits the new slot's own entry (landed T6.1, reproduced 2026-11-26).** `append_cell` accepts when `free_offset − len ≥ 32 + 2·slot_count`, then writes the 2-byte slot entry at that address. For the two lengths where the cell would start there or one byte above it (fresh page: 8159- and 8160-byte cells), the entry overwrites the cell's length prefix, `append_cell` returns `Ok`, and `PageView::parse` refuses the image as malformed | **Fixed 2026-11-26 (T6.1 fixup).** Rule `free_offset − len ≥ 32 + 2·(slot_count + 1)`; a fresh-page boundary test and a sweep over every page state the zero-payload fill reaches, both proven to fail on the old rule; the documented formula follows. No valid page changed and no golden moved |
| `crates/02-storage/cascade-redo/src/lib.rs` (in flight, another session) | Mirrored the uncorrected arithmetic: `MAX_CELL_LEN` = 8160 and `fits` accepted both unsafe lengths, and `apply_record` reported `Applied` for an image that does not parse. Separately, `apply_record` reads `slot_count`/`free_offset` *before* initializing a zero-filled page, so an insert into a never-written page returns `SpillRequired` (reproduced for one length; by inspection it holds for every length) while its own `first_insert_initializes…` test expects `Applied`. and `SegmentFill::route` never consumes the cells it places, so the cursor cannot advance past a page (reproduced). The test module does not compile yet, so none of it has been run | **The `+ 2` is applied** (`MAX_CELL_LEN` = 8158, `fits`, `apply_record`) and verified on a scratch copy: route + apply equals replay through the format's own append, including the top of the size range. The crate is uncommitted M7 work, so the edit rides with its commit. Left for the M7 session: initialize before the space check, and make `route` consume what it places |
| `crates/09-runtime/cascade-runtime/AI/runtime-implementation-plan.md` (T6.4) | Accepts that "live layout may differ from replay layout" while Tids never escape | **Decided 2026-11-26 and applied** (heap design H9): LSN-ordered sequential fill, so live layout equals replayed layout — the page store will serve refetched pages to the leader in replayed layout (storage M8). Plan copies outside the repository still carry the old sentence |
| `crates/05-access/cascade-heap/Cargo.toml` | Description says "in-place MVCC rows, undo references"; deps include the pre-pivot `cascade-mvcc`/`cascade-undo` | Step L3 |
| `crates/04-txn/cascade-mvcc/AI/CONTEXT.md`, `cascade-undo/AI/CONTEXT.md` | Pre-pivot framing (out of this pass's scope, per Kenneth's call) | 04-txn design pass |

**Architecture confirmations requested** (none change a contract; each makes an existing sentence match what this layer does):

1. §4.6's `cascade-access-api` sketch names a `TableAccessMethod` trait; WP5 D1 (accepted) and this document finalize enum dispatch. The sketch's "trait" wording reads as the usual starting-point caveat (architecture §4 preamble), so no amendment is proposed — this document is the final API.
2. §3.2's `cascade-page` row ("typed views for index-organized leaves") is the authority for A3; the page mutation design's contrary line is local drift, corrected above.

---

## 13. Open Questions

| Question | Owner / needed before |
| :--- | :--- |
| **Version-word stamping and status-table retention horizon.** WP2 D2 left in-place stamping (rewrite `uncommitted(xid)` → `committed(ts)`) rejected "for this era", with readers resolving words through the status table — but this layer's engines impose a constraint that decides the retention policy: a status entry for xid X must outlive every page word that may still carry `uncommitted(X)`, which is *purge-coupled* (entries retire only below the purge watermark), not read-watermark-coupled. Commit-time vs lazy stamping stays open as WP2 left it | The L4 transaction pass (with `cascade-scope-txn`); must be settled before the MVCC milestone's status-retention policy, engine-neutral via the A8 seam |
| `MiniTxn` exact shape (record/mutate/durable split, reservation across cores of the write set) | The mini-transaction module's design (architecture §4.4); engines need it frozen before L4. Heap design H9 (decided) also requires, in the type-5 era, that the group's LSN be known synchronously at `record` time, so the heap's fill cursor can advance in LSN order |
| Roll_ptr encoding, undo record format, undo segment layout, recovery discovery of in-flight undo | `cascade-iot` detailed design (WP6) — one-way door, before the first undo page kind is frozen |
| Key contract move to `cascade-datum` (A7's expiry) | The 07-sql split that creates `cascade-datum`; until then the layering exception stands |
| Locator validity beyond one statement (cursor pinning) | Executor/cursor design (WP5 carried it; unchanged) |
| SSI predicate-read reporting semantics | WP9 (hook reserved in §4.7) |
| TOAST chunk format, compression, versioning of toasted values | heap design §8 (WP7's successor) |
| Cleanup watermark learning across scopes and replicas | `cascade-watermark` design (architecture §4.5) |
| Bulk-load path (page handoff, record payload) | Reserved types exist (WP1 F3); design at the bulk era |
| Hash variant: sibling crate vs kind inside `cascade-btree` | Decided here: sibling crate behind `cascade-index`'s seam when the kind is needed (btree design §12); reopened only by evidence |
| `TableStats` shape the planner needs | Planner design (WP5 carried it; unchanged) |

---

## 14. Decision Log and Provenance

| Decision | Status |
| :--- | :--- |
| WP5 D1/D7 enum dispatch (no `dyn`/generics/`async-trait`) | Inherited, unchanged (§4.1) |
| WP5 D2 opaque statement-scoped locators | Inherited, unchanged (§4.4) |
| WP5 D3 outcomes as values; engine-side bounded waits | Inherited; wait machinery named (§4.4) |
| WP5 D4 one visibility rule, `row_visible` composition | Inherited; `VisibilitySource` seam added (A8, §4.5) |
| WP5 D5 engine-maintained secondary indexes | Inherited, unchanged (§9.1) |
| WP5 D6 read surface; RC re-check; `lock_row` returns the row | Inherited, unchanged (§4.3) |
| WP5 D8 scope ownership + message-to-owner reads | **Superseded** by A4 — the shared-memory engine over write core sets (architecture §4.8, ADR-0003, T6.2 landed) |
| WP5 D9 infrastructure-only errors; poisoned txns | Inherited; `Fenced` keyed on scope+epoch (§4.9) |
| WP5 D10 statement boundary | Inherited, unchanged (§4.6) |
| WP5 D11 scan protocol | Inherited, unchanged (§4.7) |
| WP5 D12 speculative-insert upsert | Inherited, unchanged (§4.8) |
| WP5 D13 key contract co-owned with WP1 | Inherited; home interim `cascade-pgtypes`, successor `cascade-datum` (A7) |
| WP5 D14 DDL = physical phases; schema versions from `StmtCtx` | Inherited, unchanged (§4.3, §4.10) |
| WP5 D15 `RowRef` owns buffer; lazy TOAST | Inherited; mechanics to heap design §8 |
| A1 mini-transaction is the only write primitive | Decided — from storage-layer-design §12.5 + architecture §4.4; supersedes every direct-BPM-write path |
| A2 redo ownership: dispatch in `cascade-redo`, transforms at format authorities | Decided — composition of architecture §4.2 + the page design D4; resolves the apply-function home question |
| A3 btree page formats in `cascade-page` | Decided — architecture §3.2 layer map wins over the page design §2 local line |
| A4 shared-memory engine over write core sets | Decided — supersedes WP5 D8; contention budget kept as a design requirement |
| A5 `TableDescriptor` construction; engines never read the catalog | Decided — architecture §4.6 |
| A6 `ScopePages` allocation facility; ids never reused | Decided — S18 applied to this layer |
| A7 key contract edge exception (05 → 07), expiring at `cascade-datum` | Decided — documented layering exception |
| A8 `VisibilitySource` seam | Decided — keeps engines runtime-free across the L4 split |
| A9 `cascade-btree` = shared tree mechanism, transaction-blind | Decided — architecture §4.6 verbatim |

**Provenance.** WP5 (D1–D15) accepted 2026-11-14; retired from the tree 2026-11-22 with HLD v2 (`381ee5b`), recovered from git history for this document. The engine semantics of record for heap/IOT/indexes remain HLD v2 §11–§13, as taken over by storage-layer-design §12 and the page mutation design. This document was authored 2026-11-26 against baseline `8fadd7a`, after storage migration M0–M6 and runtime T6.0–T6.2 landed.
