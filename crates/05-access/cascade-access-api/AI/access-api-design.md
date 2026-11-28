# cascade-access-api design — the table access contract, its fake and its suite

**Status:** Draft v0.2 for review, 2026-11-27. This is the detailed implementation design for layer Step 1 (layer §12): the exact Rust types, traits and errors of the contract, the dispatch enums, the in-memory fake engine, and the conformance suite. v0.1 settled the crate layout and C1–C13, which Kenneth accepted on 2026-11-27 (C1, C5, C6 and C10 first, the rest through register D-10). v0.2 adds the detail and proposes C14–C23 (§13), which register D-18 tracks.

**Scope (Kenneth, 2026-11-27).** This document designs only the 05-access component. Where it depends on another component, it says what it **assumes** and **requires** from it and does not design it. Those dependencies are collected in §11. In particular `cascade-txn-api` is an L4 crate: the Rust shown for it here (§4) is the interface 05-access consumes, stated as a requirement, and the transaction layer's own design owns it.

**Upstream:** the [layer design](../../AI/access-layer-design.md) v0.2 — §6 (the contract), §6.13 (the fake and the suite), §8 (execution model), §12 (Step 1). [`docs/architecture-design.md`](../../../../docs/architecture-design.md) v0.10.2 — Rules 1, 2, 6, 8, 9, 10; §4.5; §4.6. The [decision register](../../AI/decision-register.md). Citation prefixes follow layer §2.2; "api §" means this document.

**Code baseline:** `main` at `2535d4a`. Reused as they are: `cascade-types` (`TxnId`, `Hlc`, `VersionWord`, `Epoch`, `ScopeId`, `DatabaseId`, `SegmentId`, `PageId`, `PageNo`, `OpCtx`, `LocalBoxFuture`), `cascade-pgtypes` (`KeyBytes`, `KeyRange`, `Locator`, `NulInText`, the key encoders, `concat`), `cascade-telemetry` (`Span`, `MetricsRegistry`). Conventions copied from `cascade-storage-api`: `fake` and `conformance` cargo features, on by default; suites as generic `async fn`s over a harness trait; `futures-lite::block_on` to drive them; `thiserror` for error enums.

---

## 1. What Step 1 delivers

1. The contract of layer §6 as compiled Rust, with no engine behind it yet (api §5, §6).
2. The dispatch types the executor holds, `Table`, `TableScan` and `Maintenance`, with the fake as their only arm until the heap lands in Step 3 (api §7).
3. An in-memory fake engine that keeps every promise of the contract. It is the executor's test double and the reference model for the engines (api §8).
4. One conformance suite, run against the fake now and against each engine as it lands (api §9).

Nothing in Step 1 touches a page, the log or the page cache. It needs none of D-2, D-4, D-7 or D-8. It does need `cascade-txn-api` to exist with the interface of api §4 and its scripted fake (§11, X1).

## 2. Crates (C1)

| Crate | Layer | Owner | Contains | Depends on |
| :--- | :--- | :--- | :--- | :--- |
| `cascade-txn-api` | L4 | Transaction layer (**external**, §11 X1) | The seams the engines consume (api §4). Feature `fake`: `ScriptedWorld` | `cascade-types` |
| `cascade-access-api` | L5 | 05-access | The contract types (api §5), the `TableAccess`, `RowScan` and `MaintenanceStep` traits (api §6), key helpers. Features `fake` (the fake engine, api §8) and `conformance` (the suite, api §9), on by default | `cascade-types`, `cascade-txn-api`, `cascade-pgtypes` (A7, D-9d), `cascade-telemetry`, `thiserror`; optional `futures-lite` |
| `cascade-access-dispatch` | L5, above the engines | 05-access | `Table`, `TableScan`, `Maintenance`, `abort_unresolved` (api §7) | `cascade-access-api`, each engine crate as it lands |

**File layout of `cascade-access-api`:**

```text
src/lib.rs          re-exports; #![deny(missing_docs)]
src/ids.rs          RelId, IndexId, ColId, StorageKind, TableHandle, Tid        (api §5.1)
src/row.rs          Datum, ColumnType, Row, Projection, ColumnSet, RowRef     (api §5.2)
src/locator.rs      RowLocator, lock keys                                     (api §5.3)
src/outcome.rs      WriteOutcome, Conflict, RowLockMode, WaitPolicy, Locked,
                    SpeculativeResult, SpecToken                              (api §5.4)
src/scan.rs         ScanRow, Direction, SeqScanOpts, IndexScanOpts,
                    ScanParams, ResumeHint, SsiReporter                       (api §5.5)
src/caps.rs         AmCapabilities, TableStats, Budget, Progress              (api §5.6)
src/descriptor.rs   TableDescriptor, ColumnDef, KeyShape, IndexDef, ToastPolicy (api §5.7)
src/error.rs        TamError, FailCause, sqlstate()                           (api §5.8)
src/keys.rs         encode_key, unique_probe_key                              (api §5.9)
src/access.rs       TableAccess, RowScan, MaintenanceStep                     (api §6)
src/fake/           FakeTable, FakeScan, FakeMaint            (feature fake,  api §8)
src/conformance/    TableHarness, S1–S13, run_all            (feature conformance, api §9)
```

## 3. Rules every type follows

- **Exhaustive where the executor decides (Rule 9).** `WriteOutcome`, `Conflict`, `SpeculativeResult`, `RowLockMode`, `WaitPolicy` and `Direction` are matched exhaustively by the executor on purpose: a new outcome must be a compile error at every match.
- **`#[non_exhaustive]` only on `TamError`, `FailCause`, `Datum` and `ColumnType` (C18).** These grow and are matched with a fallback arm. Structs that engines build (`AmCapabilities`, `TableStats`, `RowRef`, `ScanRow`, `Progress`) have public fields and are *not* `#[non_exhaustive]`, because that attribute would stop the engine crates from constructing them. v0.1 said "everything else is `#[non_exhaustive]`"; that is corrected here. The whole workspace is one repository, so a new field is a compile error everywhere it matters, which is what we want.
- **Ids are `Copy` newtypes** with `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash`.
- **No type holds a reference into a page.** Everything a caller receives is owned (layer §6.4).
- **Nothing here is `Send`-bounded.** Futures are `!Send` by design (layer §6.1, §8).

## 4. Required interface of `cascade-txn-api` (external, L4)

05-access does not design this crate. This section states the exact interface Step 1 compiles against, so that the transaction layer's owner can build it or confirm it. Every item is also listed in §11 X1 with its required semantics. Items marked **new in v0.2** are not in layer §6.6–§6.7 and need the transaction layer's agreement (C14, C15).

```rust
// ---- visibility (layer §6.6) ----
pub struct Snapshot { pub read_ts: Hlc, pub own: Option<OwnView> }          // Clone, Copy, Debug
pub struct OwnView { pub top: TxnId, pub horizon: TxnId }                  // Clone, Copy, Debug
pub enum TxnStatus { InProgress { top: TxnId }, Committed { ts: Hlc }, Aborted }
pub trait VisibilitySource: Send + Sync { fn status(&self, xid: TxnId) -> TxnStatus; }
pub enum Visibility { Visible, CommittedAfterSnapshot { commit_ts: Hlc }, InProgress { holder: TxnId }, Aborted }
pub enum RowVisibility { Live, NotBornYet, Deleted { own: bool } }
impl Snapshot {
    pub fn visible(&self, word: VersionWord, src: &dyn VisibilitySource) -> Visibility;
    pub fn row_visible(&self, creator: VersionWord, deleter: Option<VersionWord>,
                       src: &dyn VisibilitySource) -> RowVisibility;
}

// ---- waiting (layer §6.5, C8) ----
pub struct LockKey { pub space: u64, pub key: Vec<u8> }                     // Clone, Eq, Hash
pub enum WaitFor { Txn(TxnId), SharedLocks(LockKey), Key(LockKey) }
pub enum Waited { Resolved, TimedOut, Deadlock, LockTimeout }
pub trait WaitSource: Send + Sync {
    fn wait<'a>(&'a self, ctx: &'a OpCtx<'_>, waiter: TxnId, what: WaitFor,
                deadline: Option<Instant>) -> LocalBoxFuture<'a, Waited>;
    fn signal(&self, key: &LockKey);
}

// ---- row locks held in the lock manager (layer §6.10, A11) — new in v0.2, C14 ----
pub enum LockMode { KeyShare, Share, NoKeyUpdate, Update }                  // Copy, Eq
pub struct LockHolder { pub xid: TxnId, pub mode: LockMode }
pub trait RowLockTable: Send + Sync {
    /// Non-blocking. Ok when granted; otherwise the conflicting holders that are still live.
    fn try_acquire(&self, key: &LockKey, holder: TxnId, mode: LockMode) -> Result<(), Vec<LockHolder>>;
    /// Live holders of the key whose mode conflicts with `mode`, excluding `me`'s own transaction.
    fn conflicting(&self, key: &LockKey, me: TxnId, mode: LockMode) -> Vec<LockHolder>;
    /// Records on `to` every live lock held on `from` (locks follow the row, layer §6.10).
    fn copy_locks(&self, from: &LockKey, to: &LockKey);
}

// ---- the transaction context (layer §6.7, C5, C6) ----
pub struct TxnServices {
    pub clock: Arc<dyn TxnClock>, pub status: Arc<dyn VisibilitySource>,
    pub waits: Arc<dyn WaitSource>, pub epochs: Arc<dyn EpochSource>,
    pub locks: Arc<dyn RowLockTable>,                                       // new in v0.2, C14
}                                                                           // Clone
pub struct SchemaVersion(pub u64);
pub struct SpecId(pub u64);
pub struct StampPos { /* opaque to L4: rel, and the engine's position bytes */ }
pub enum Liveness { Fenced { scope: ScopeId }, Poisoned, Failed }
pub struct Fenced { pub scope: ScopeId }
pub enum Poison { Io, Fenced }
pub struct StmtCtx { pub snapshot: Snapshot, pub serial: u64, pub schema_version: SchemaVersion }

impl TxnCtx {                                         // !Send; built only by the transaction layer
    pub async fn begin_statement(&mut self) -> Result<&mut StmtCtx, Fenced>;
    pub fn end_statement(&mut self) -> Vec<(u64 /* rel */, SpecId)>;
    pub fn stmt(&self) -> &StmtCtx;                   // panics outside a statement
    pub async fn fresh_snapshot(&mut self) -> Result<Snapshot, Fenced>;
    pub fn check_live(&self) -> Result<(), Liveness>;
    pub fn poison(&self, why: Poison);
    pub fn mark_failed(&self);                        // round-4 W2
    pub fn xid(&self) -> Option<TxnId>;               // the top-level id, once assigned
    pub fn write_xid(&mut self) -> TxnId;             // this statement's subtransaction id
    pub fn wait_deadline(&self) -> Option<Instant>;   // the configured wait bound, from now
    pub fn register_spec(&mut self, rel: u64) -> SpecId;
    pub fn resolve_spec(&mut self, rel: u64, id: SpecId);
    pub fn record_stamp(&mut self, pos: StampPos);    // D-1
    pub fn services(&self) -> &TxnServices;
    pub fn read_view(&self) -> ReadView;              // new in v0.2, C15
}

/// What a scan keeps instead of a borrow of the context (C15). Clone, !Send.
/// Shares the context's poison and failed flags (Rc<Cell<..>> inside).
impl ReadView {
    pub fn snapshot(&self) -> &Snapshot;
    pub fn check_live(&self) -> Result<(), Liveness>;
    pub fn poison(&self, why: Poison);
    pub fn services(&self) -> &TxnServices;
}
```

`end_statement`, `register_spec` and `resolve_spec` take the relation id as a plain `u64`, because L4 cannot name `RelId`. `cascade-access-api` converts at the boundary.

**Why C14 is needed.** Layer §6.10 puts shared row locks, and an earlier subtransaction's exclusive lock (W3), in the lock manager, and A11 says writers "consult the lock manager". But v0.1's `WaitSource` can only sleep and signal. It has no way to register a lock or list the holders. So engines, and the fake, have no interface for half of A11. `RowLockTable` is the smallest interface that covers what the access layer does: take a lock without blocking, ask who conflicts, and carry locks to a row's new version. Release at transaction end and on rollback to a savepoint stays inside L4 (layer §6.6).

**Why C15 is needed.** v0.1 had a scan keep `&TxnCtx` for its whole life (round-3 V25). But `UPDATE … WHERE` drives a scan and calls `update(&mut TxnCtx, …)` on each row it yields, and the borrow checker forbids holding `&TxnCtx` and `&mut TxnCtx` at once. A `ReadView` is an owned, cheap clone of what a read needs: the statement snapshot, the services and the shared poison flag. So a scan can be alive while the executor writes through the context. A scan created in one statement holds that statement's snapshot, which is what `rescan` requires (layer §6.8).

## 5. `cascade-access-api`: the contract types

### 5.1 `ids`

```rust
/// A relation's stable id, assigned by the catalog and never reused (§11 X7).
pub struct RelId(pub u64);
/// An index's stable id, assigned by the catalog and never reused.
pub struct IndexId(pub u64);
/// A column's position in the table's column list (0-based), stable for the descriptor's life.
pub struct ColId(pub u16);

/// Which engine stores a table. The temporary engine is chosen by persistence, not by this
/// field (open question, api §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageKind { Iot, Heap }

/// What `handle()` returns: the planner's name for one table in one scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableHandle { pub rel: RelId, pub scope: ScopeId }

/// A heap row address, in memory. On disk and in index entries it is packed relation-locally
/// as `Locator::Heap { page_no, slot }`; the segment comes from the table's descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tid { pub page: PageId, pub slot: u16 }

impl Tid {
    pub fn packed(&self) -> Locator;                                 // Locator::Heap { page_no, slot }
    pub fn from_packed(db: DatabaseId, segment: SegmentId, loc: &Locator) -> Option<Tid>; // None for Pk
}
```

### 5.2 `row`

```rust
/// One column value. Grows with the type layer (§11 X4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Datum { Null, Int64(i64), Bool(bool), Text(String) }

/// A column's declared type. Mirrors `Datum`; local so that the A7 exception stays limited
/// to the key contract (C22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnType { Int64, Bool, Text }

/// One value per column, in column order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row(pub Vec<Datum>);

/// The columns a read returns, in the order it returns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection(pub Vec<ColId>);

/// The columns an update changed. Engines use it to decide HOT, key change and index work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSet(pub Vec<ColId>);

/// A decoded, owned copy of the projected columns of one visible row version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowRef { pub values: Vec<Datum> }   // values[i] is the column proj.0[i]

impl Projection { pub fn all(n_cols: u16) -> Projection; }
impl ColumnSet  { pub fn contains(&self, c: ColId) -> bool; }
impl Row        { pub fn project(&self, p: &Projection) -> RowRef; }
```

The out-of-line value handle that heap §8 needs is added to `RowRef` with the heap's row codec in Step 3 (api §12).

### 5.3 `locator`

```rust
/// The executor's handle on a row version. Opaque to the executor: it stores it and passes it
/// back, and never matches on it. Valid for the rest of the statement that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowLocator { Pk(KeyBytes), Tid(Tid) }

impl RowLocator {
    /// The lock-manager key for this row version (C19).
    pub fn lock_key(&self, rel: RelId) -> LockKey;
    /// The locator as an index entry stores it.
    pub fn to_index(&self) -> Locator;
}

/// The lock-manager key for a key in a unique index (the speculative-insertion wait, layer §6.9).
pub fn index_key_lock(index: IndexId, key: &KeyBytes) -> LockKey;
```

**Lock-key encoding (C19).** `LockKey { space, key }`: `space` is the `RelId` or `IndexId` number, and the first byte of `key` says what follows:

| Tag | Used for | Bytes after the tag |
| :--- | :--- | :--- |
| `0x00` | `RowLocator::Pk` | the primary key's `KeyBytes` |
| `0x01` | `RowLocator::Tid` | `segment` u32 BE, `page_no` u32 BE, `slot` u16 BE |
| `0x02` | `index_key_lock` | the unique key's `KeyBytes` |

The tag keeps a relation's row keys apart from an index's key locks even if the catalog's two id spaces overlap, so 05-access needs no promise about how the catalog numbers ids. The segment is in the Tid key because `truncate` gives a table a new segment while old locks may still be held.

### 5.4 `outcome`

```rust
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The write happened. The locator may differ from the input (the row moved).
    Done { locator: RowLocator },
    Conflict(Conflict),
    UniqueViolation { index: IndexId, key: KeyBytes },
    /// The row is gone for this snapshot, or SkipLocked skipped it.
    NotVisible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// Another transaction holds the row and has not ended. `holder` is its top-level id.
    InProgress { holder: TxnId },
    /// Another transaction committed a change after our snapshot. `latest` is for
    /// fetch_latest_committed, not for fetch.
    CommittedAfterSnapshot { commit_ts: Hlc, latest: Option<RowLocator> },
}

/// PostgreSQL's four row-lock modes (layer §6.10). Maps 1:1 to cascade-txn-api's LockMode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowLockMode { KeyShare, Share, NoKeyUpdate, Update }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPolicy { Wait, SkipLocked, DontWait }

/// lock_row's result. `row` is Some exactly when `outcome` is Done.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locked { pub outcome: WriteOutcome, pub row: Option<RowRef> }

#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum SpeculativeResult {
    /// Must be completed or aborted before the statement ends.
    Installed(SpecToken),
    KeyBusy(Conflict),
    UniqueViolation { index: IndexId, key: KeyBytes },
}

/// An installed speculative insertion (C4). Not Clone: it is consumed by complete or abort.
#[derive(Debug, PartialEq, Eq)]
pub struct SpecToken { rel: RelId, id: SpecId }

impl SpecToken {
    #[doc(hidden)] pub fn engine_new(rel: RelId, id: SpecId) -> Self;
    pub fn rel(&self) -> RelId;
    #[doc(hidden)] pub fn id(&self) -> SpecId;
}

impl RowLockMode {
    pub fn to_txn(self) -> LockMode;
    /// Layer §6.10's compatibility table. Symmetric.
    pub fn conflicts_with(self, held: RowLockMode) -> bool;
}
```

### 5.5 `scan`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRow { pub row: RowRef, pub loc: RowLocator }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction { Forward, Backward }

#[derive(Debug, Clone)]
pub struct SeqScanOpts {
    pub dir: Direction,
    pub resume: Option<ResumeHint>,
    pub ssi: Option<SsiReporter>,
    /// Pages one `next` may read before it yields to the reactor. 0 means the engine's default.
    pub page_budget: u32,
}

#[derive(Debug, Clone)]
pub struct IndexScanOpts {
    pub dir: Direction,
    pub index_only: bool,
    pub ssi: Option<SsiReporter>,
    pub page_budget: u32,
}

/// The rescan bindings: for an index scan, a new key range; None keeps the old one.
#[derive(Debug, Clone, Default)]
pub struct ScanParams { pub range: Option<KeyRange> }

/// An engine-private scan position (C4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeHint(Box<[u8]>);
impl ResumeHint {
    #[doc(hidden)] pub fn engine_new(bytes: Box<[u8]>) -> Self;
    #[doc(hidden)] pub fn bytes(&self) -> &[u8];
}

/// Reserved for the serializable-isolation design. Carries nothing in Step 1.
#[derive(Debug, Clone, Default)]
pub struct SsiReporter { _reserved: () }
```

### 5.6 `caps`

```rust
/// Capabilities for the planner. Correctness never depends on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AmCapabilities {
    pub ordered_scans: bool, pub mark_restore: bool, pub index_only: bool, pub count_star_fast: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TableStats { pub row_estimate: f64, pub dead_fraction: f32, pub free_pages: u32 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget { pub pages: u32 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress { pub more: bool }
```

### 5.7 `descriptor`

```rust
/// Everything an engine needs to know about a table, built by the scope host from the catalog
/// (§11 X7). Engines never read the catalog.
#[derive(Debug, Clone)]
pub struct TableDescriptor {
    pub rel: RelId,
    pub kind: StorageKind,
    pub scope: ScopeId,
    /// The catalog version that produced this descriptor. A write from a statement whose
    /// StmtCtx::schema_version is lower returns SchemaStale { min_leased: this }.
    pub schema_version: SchemaVersion,
    pub columns: Vec<ColumnDef>,
    pub key: KeyShape,
    pub indexes: Vec<IndexDef>,
    pub toast: ToastPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef { pub id: ColId, pub ty: ColumnType, pub nullable: bool }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyShape {
    /// No primary key. Only the heap engine accepts this.
    None,
    /// The primary-key columns, in key order. The IOT's organizing key; on a heap, covered by
    /// a unique index that also appears in `indexes`.
    Primary(Vec<ColId>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef { pub id: IndexId, pub key_columns: Vec<ColId>, pub unique: bool }

/// How oversized values are stored (heap §8). Step 1 has only the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastPolicy { Never }

impl TableDescriptor {
    /// Checks the descriptor's own consistency: column ids dense from 0, key columns exist and
    /// are not nullable, an IOT has a primary key, index ids unique.
    pub fn validate(&self) -> Result<(), TamError>;     // Unsupported("...") on a bad shape
}
```

`TableEnv` is not here (C9). Each engine's constructor takes its own environment.

### 5.8 `error`

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TamError {
    #[error("storage I/O failed: {0}")]                  Io(String),
    #[error("scope {scope:?} changed owner under this transaction")] Fenced { scope: ScopeId },
    #[error("schema changed; statement must restart at version {min_leased:?} or later")]
                                                         SchemaStale { min_leased: SchemaVersion },
    #[error("deadlock detected")]                        Deadlock,
    #[error("lock wait exceeded lock_timeout")]          LockTimeout,
    #[error("transaction is poisoned by an earlier failure")] Poisoned,
    #[error("statement failed after changing rows: {cause}")] StatementFailed { cause: FailCause },
    #[error("row of {len} bytes exceeds the maximum of {max}")] RowTooLarge { len: usize, max: usize },
    #[error("not supported: {0}")]                       Unsupported(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FailCause {
    #[error("duplicate key in index {index:?}")] UniqueViolation { index: IndexId, key: KeyBytes },
    #[error("wait for transaction {holder:?} expired")] WaitExpired { holder: TxnId },
    /// Every call after the first failure, until a savepoint rollback or abort (C16).
    #[error("current transaction is aborted")]   PriorFailure,
}

impl TamError {
    pub fn sqlstate(&self) -> &'static str;   // table below (C17)
    /// Io and Fenced: the transaction's outcome is unknown and the context is poisoned.
    pub fn poisons(&self) -> bool;
}
impl From<Liveness> for TamError { /* Fenced → Fenced, Poisoned → Poisoned,
                                      Failed → StatementFailed { cause: PriorFailure } */ }
impl From<Fenced> for TamError { /* → Fenced */ }
```

**SQLSTATE mapping (C17).** `TamError::sqlstate()` returns PostgreSQL's code, so the executor can raise the right error before `cascade-error` can carry codes (§11 X5):

| Error | SQLSTATE | PostgreSQL name |
| :--- | :--- | :--- |
| `Io` | 58030 | `io_error` |
| `Fenced` | 08007 | `transaction_resolution_unknown` |
| `SchemaStale` | 40001 | `serialization_failure` (the client retries) |
| `Deadlock` | 40P01 | `deadlock_detected` |
| `LockTimeout` | 55P03 | `lock_not_available` |
| `Poisoned` | 25P02 | `in_failed_sql_transaction` |
| `StatementFailed { UniqueViolation }` | 23505 | `unique_violation` |
| `StatementFailed { WaitExpired }` | 55P03 | `lock_not_available` |
| `StatementFailed { PriorFailure }` | 25P02 | `in_failed_sql_transaction` |
| `RowTooLarge` | 54000 | `program_limit_exceeded` |
| `Unsupported` | 0A000 | `feature_not_supported` |

**Which errors poison, and which fail.** Every engine entry point follows one order: `txn.check_live()?` first (layer §6.7). An `Io` or `Fenced` met later calls `txn.poison(..)` before returning. A `StatementFailed` calls `txn.mark_failed()` before returning. The fake does the same (api §8.4), and suite S10 checks it.

### 5.9 `keys`

```rust
/// The key of `cols` in `row`, with nulls encoded (null sorts last). For index entry keys and
/// primary keys. Err if a text value has an interior NUL.
pub fn encode_key(row: &Row, cols: &[ColId]) -> Result<KeyBytes, NulInText>;
/// The same, but None if any column is null: the unique check skips such keys, because
/// nulls are distinct (layer §11.4).
pub fn unique_probe_key(row: &Row, cols: &[ColId]) -> Result<Option<KeyBytes>, NulInText>;
/// A non-unique index's tree key: the column key followed by the locator (layer §11.4, btree B8).
pub fn nonunique_entry_key(key: &KeyBytes, loc: &RowLocator) -> KeyBytes;
```

Built on `encode_int8`, `encode_bool`, `encode_text`, `encode_null` and `concat` from `cascade-pgtypes` (A7, C13). Engines map `NulInText` to `TamError::Unsupported("NUL in a text key")`. PostgreSQL's text input already refuses NUL (22021), so this is a defensive check (§11 X4) (C23).

## 6. The traits (C3)

```rust
/// The plug the suite is written against (layer §6.1, A13). The executor never names it;
/// only cascade-access-dispatch and test code do.
#[allow(async_fn_in_trait)] // the futures must stay !Send (layer §6.1); see below
pub trait TableAccess {
    type Scan: RowScan;
    type Maint: MaintenanceStep;

    fn handle(&self) -> TableHandle;
    fn capabilities(&self) -> &AmCapabilities;
    fn stats(&self) -> TableStats;

    async fn insert(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, row: &Row) -> Result<WriteOutcome, TamError>;
    async fn update(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, loc: &RowLocator, new: &Row,
                    changed: &ColumnSet) -> Result<WriteOutcome, TamError>;
    async fn delete(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, loc: &RowLocator) -> Result<WriteOutcome, TamError>;
    async fn lock_row(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, loc: &RowLocator,
                      mode: RowLockMode, wait: WaitPolicy, proj: &Projection) -> Result<Locked, TamError>;

    async fn insert_speculative(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, row: &Row,
                                index: IndexId) -> Result<SpeculativeResult, TamError>;
    async fn complete_speculative(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, t: SpecToken) -> Result<RowLocator, TamError>;
    async fn abort_speculative(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, t: SpecToken) -> Result<(), TamError>;

    async fn fetch(&self, ctx: &OpCtx<'_>, txn: &TxnCtx, loc: &RowLocator,
                   proj: &Projection) -> Result<Option<RowRef>, TamError>;
    async fn fetch_latest_committed(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, loc: &RowLocator,
                                    proj: &Projection) -> Result<Option<(RowLocator, RowRef)>, TamError>;
    fn seq_scan(&self, txn: &TxnCtx, proj: &Projection, opts: SeqScanOpts) -> Self::Scan;
    fn index_scan(&self, txn: &TxnCtx, index: IndexId, range: KeyRange,
                  proj: &Projection, opts: IndexScanOpts) -> Self::Scan;

    async fn build_index(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, def: &IndexDef) -> Result<(), TamError>;
    async fn truncate(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx) -> Result<(), TamError>;
    fn maintenance(&self) -> Self::Maint;
}

#[allow(async_fn_in_trait)]
pub trait RowScan {
    async fn next(&mut self, ctx: &OpCtx<'_>) -> Option<Result<ScanRow, TamError>>;
    async fn rescan(&mut self, ctx: &OpCtx<'_>, params: &ScanParams) -> Result<(), TamError>;
    fn supports_mark_restore(&self) -> bool;
    fn mark(&mut self);
    async fn restore(&mut self, ctx: &OpCtx<'_>) -> Result<(), TamError>;
    async fn close(self, ctx: &OpCtx<'_>) where Self: Sized;
}

#[allow(async_fn_in_trait)]
pub trait MaintenanceStep {
    async fn step(&mut self, ctx: &OpCtx<'_>, budget: Budget) -> Progress;
}
```

Notes on the signatures:

- **`#[allow(async_fn_in_trait)]` (C21).** On a public trait rustc warns that callers cannot require `Send` futures, and the workspace gate allows no warnings. Here `!Send` futures are the intent (thread-per-core, layer §8), so the lint is allowed on these three traits only, with that reason written next to it.
- **No `Send + Sync` bound on `TableAccess`.** v0.1 had one. It forces nothing useful, since tables live on their scope's write cores (layer §8), and it would forbid an engine from holding `Rc` state. The fake is `Send + Sync` anyway.
- **Scans own their state (C15).** `Self::Scan` borrows neither the table nor the context. It holds a clone of the table's handle (engines keep their state behind an `Arc` or `Rc`) and a `ReadView`. So an executor can call `update(&mut txn, …)` on rows that a live scan yields.
- **`OpCtx` everywhere work can happen (Rule 8).** That covers every `async fn` above, including `RowScan::next`, `rescan`, `restore`, `close` and `MaintenanceStep::step`, which v0.1 left out. `seq_scan` and `index_scan` do no work (layer §6.8), so they take none.
- **`lock_row` and `fetch_latest_committed` take a `Projection`.** That way the row they return has the shape the executor asked for, as with `fetch`. Layer §6.3 left it implicit.
- **Only `lock_row` takes a `WaitPolicy`** (C3, layer §6.5). Every other write behaves as `Wait`.
- **The trait is used only through generics**, so it need not be object-safe.

**What every implementation must do on entry** (checked by the suite):

1. `txn.check_live()?`, before anything else.
2. For a write: if `txn.stmt().schema_version < descriptor.schema_version`, return `SchemaStale { min_leased: descriptor.schema_version }`.
3. For a write: take `txn.write_xid()` only once it is known the call will write. A refused or conflicting write allocates no subtransaction id.

## 7. `cascade-access-dispatch` (C2, C20)

```rust
pub enum Table {
    #[cfg(feature = "fake")] Fake(FakeTable),
    // Heap(HeapTable)  — Step 3
    // Iot(IotTable)    — Step 7
    // Temp(TempTable)  — Step 9
}
pub enum TableScan   { #[cfg(feature = "fake")] Fake(FakeScan),  /* engine arms as they land */ }
pub enum Maintenance { #[cfg(feature = "fake")] Fake(FakeMaint), /* engine arms as they land */ }

impl Table {
    // one inherent method per TableAccess method, same names, same arguments;
    // seq_scan/index_scan return TableScan, maintenance returns Maintenance.
}
impl TableScan   { /* one inherent method per RowScan method */ }
impl Maintenance { pub async fn step(&mut self, ctx: &OpCtx<'_>, budget: Budget) -> Progress; }

/// Aborts the speculative tokens a statement left unresolved (C6). The executor calls it with
/// the list end_statement returned and a lookup from RelId to the statement's open tables.
pub async fn abort_unresolved<'t>(
    ctx: &OpCtx<'_>, txn: &mut TxnCtx, unresolved: Vec<(u64, SpecId)>,
    lookup: impl Fn(RelId) -> Option<&'t Table>,
) -> Result<(), TamError>;
```

**Forwarding is generated.** One `macro_rules! forward_all` lists every trait method once and expands to the inherent methods, each a `match self { Table::Fake(t) => t.method(args).await, … }`. A new trait method that is missing from the macro fails to compile in `forward_all`'s own test, which calls every method through `Table` (api §10 commit 5). That is how the arms cannot drift from the trait.

**`abort_unresolved` is a free function (C20).** The unresolved tokens can belong to several tables, and `Table` cannot look up other tables. So the executor passes a lookup. A `RelId` with no table is a bug: the function `debug_assert!`s and goes on with the rest. It returns the first error after trying every token.

**The fake arm** is behind the `fake` feature, so the production build has exactly the three arms of A13 (C2).

## 8. The fake engine (C10)

### 8.1 Shape

```rust
#[derive(Clone)]
pub struct FakeTable(Arc<FakeInner>);
struct FakeInner { desc: TableDescriptor, flavour: LocatorFlavour, cfg: FakeConfig,
                   state: std::sync::Mutex<FakeState> }   // never held across an await

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocatorFlavour { Heap, Iot }

#[derive(Debug, Clone)]
pub struct FakeConfig {
    pub max_row_bytes: usize,          // RowTooLarge above it (default 8 000)
    pub caps: Option<AmCapabilities>,  // None: the flavour's defaults (below)
}

struct FakeState {
    versions: Vec<Version>,            // index = version id; never removed except by maintenance
    indexes: Vec<IndexDef>,            // desc.indexes plus built ones
    specs: HashMap<SpecId, VersionId>, // open speculative insertions
    watermark: Hlc,                    // set by the test, for maintenance
    inject: Injections,                // the next n calls fail with Io; see 8.5
    stats: TableStats,
}

struct Version {
    id: VersionId, creator: VersionWord, deleter: VersionWord,
    lock_only: bool, updates_key: bool,
    next: Option<VersionId>,           // the newer version of the same row (both flavours)
    pk: Option<KeyBytes>,              // encode_key of the primary key, when the table has one
    row: Row, dead: bool,              // dead: removed by maintenance
}

impl FakeTable {
    pub fn new(desc: TableDescriptor, flavour: LocatorFlavour, cfg: FakeConfig) -> Result<Self, TamError>;
    pub fn set_watermark(&self, ts: Hlc);         // maintenance horizon (test hook)
    pub fn inject_io(&self, after_calls: u32);    // the call after n more fails with Io
    pub fn with_descriptor(&self, desc: TableDescriptor) -> Self; // a schema change (test hook)
}
```

Index entries are not stored. They are derived from versions: version *v* has, in every index *i*, an entry with key `encode_key(v.row, i.key_columns)`, create word `v.creator`, and delete word `v.deleter` unless `v.lock_only`. This is exactly what the engines store, so the unique check (§8.3) reads the same truth.

### 8.2 Locators and resolution

| | `Heap` flavour | `Iot` flavour |
| :--- | :--- | :--- |
| Locator | `RowLocator::Tid` naming one version: `PageId { db: DatabaseId(0), segment: SegmentId(rel as u32), page_no: (id >> 16) as u32 }`, `slot = id & 0xFFFF` | `RowLocator::Pk(pk)` |
| An update | always returns a new locator | returns the same locator unless the primary key changed |
| `fetch(loc)` | Start at the named version and walk `next` as heap §5.2 does: `Live` → return it; `Deleted` → hop to `next` if any, else `None`; `NotBornYet` → `None` | Take the newest version with this `pk` (the chain end), then walk to older ones as kept §1 does: first `Live` → return it; `Deleted` met on the way, before any `Live` → `None` |
| Capabilities (default) | `ordered_scans: false`, `mark_restore: false`, `index_only: true`, `count_star_fast: false` | `ordered_scans: true`, `mark_restore: true`, `index_only: true`, `count_star_fast: false` |
| Sequential scan order | version id order (insertion order) | primary-key order |

A version is classified with `snapshot.row_visible(v.creator, deleter, status)`, where `deleter` is `None` when `v.deleter` is empty or `v.lock_only` is set. A zero creator panics in debug builds (layer §6.6, first row).

### 8.3 Write algorithms

All writes run the entry steps of api §6, then the steps below. The *target* is the version the locator resolves to. The *newest* is the end of the target's `next` chain. "Wait on *h*" means the wait loop of §8.4.

**The conflict check** (heap §4.4, for both flavours). It runs on the newest version, with `me = txn.xid()` and for a lock request `mode`, and gives the first matching row:

1. The newest version is dead, or deleted by my own transaction (`Deleted { own: true }` or own deleter below the horizon) → `NotVisible`.
2. Its creator or deleter (not lock-only) is another transaction still in progress → holder *h*.
3. Its deleter is a lock-only stamp by another transaction still in progress, and the stamped mode (`updates_key` ⇒ `Update`, else `NoKeyUpdate`) conflicts with the requested mode → holder *h*. Writes request `Update` if they delete or change a key column, else `NoKeyUpdate`.
4. `locks.conflicting(row.lock_key(rel), me, mode)` is non-empty → holder *h* = the first.
5. Its creator or a non-lock deleter committed after the snapshot → `Conflict::CommittedAfterSnapshot { commit_ts, latest: Some(newest's locator) }`. Under `SkipLocked` → `NotVisible`.
6. Otherwise, proceed.

With a holder *h*: under `Wait`, wait on *h* and start again from the top; under `DontWait`, `Conflict::InProgress { holder: top(h) }`; under `SkipLocked`, `NotVisible`.

**insert(row)**
1. If `row` encodes to more than `max_row_bytes` (sum of value sizes), return `RowTooLarge`.
2. IOT flavour: run the unique check on the primary key *before* writing. A conflict here is a value (`UniqueViolation`, or `Conflict::InProgress` after the wait bound).
3. `xid = txn.write_xid()`. Push a version with `creator = uncommitted(xid)` and call `txn.record_stamp` for it.
4. For each unique index, in `indexes` order (for the heap flavour, the primary-key index too): run the unique check. A conflict is now *after the row step*, so it calls `txn.mark_failed()` and returns `StatementFailed { cause }`.
5. Return `Done { locator }`.

**update(loc, new, changed)**
1. Resolve the target. If there is none, return `NotVisible`. Run the conflict check with `mode` from the key test, under `Wait`.
2. IOT flavour with a primary-key change: run the unique check on the new key before writing (a value, as in insert).
3. `xid = txn.write_xid()`. If the newest's deleter is a lock-only stamp by an earlier sub-id of my own transaction, first `locks.try_acquire(key, that sub-id, its mode)` (W3). Then stamp the newest's `deleter = uncommitted(xid)` with `lock_only = false`, and `updates_key` set if a key column is in `changed`. Push the new version, link `next`, `copy_locks(old key, new key)`, and record both stamps.
4. Unique checks for each unique index whose columns are in `changed`, as insert step 4.
5. Return `Done { locator }`: the new Tid for heap, the new or old pk for IOT.

**delete(loc)**: as update steps 1 and 3, but with `updates_key = true` and no new version.

**lock_row(loc, mode, wait, proj)**
1. Resolve the target and run the conflict check under `wait`. Anything but proceed is returned as `Locked { outcome, row: None }`.
2. `Update`/`NoKeyUpdate`: stamp the newest's deleter with `uncommitted(write_xid)` and `lock_only = true`, with `updates_key` set for `Update` (W3 as update step 3). `Share`/`KeyShare`: `locks.try_acquire(key, xid, mode)`. A refusal means a holder appeared, so go back to step 1.
3. Return `Locked { outcome: Done { locator }, row: Some(projected) }`.

A lock taken by an already-ended transaction is no lock: every check resolves the holder through `status` first.

**The unique check** (layer §11.4), for index *i* and key *k* (`unique_probe_key`; `None` → no check), over every non-dead version whose entry key is *k*, against the latest state:

| Entry (create *c*, delete *d*) | Result |
| :--- | :--- |
| *c* and *d* name the same in-progress transaction (an aborted speculative insertion) | ignore |
| *c* aborted, or *d* committed (any timestamp) | ignore: the key is free |
| *c* or *d* another transaction's, in progress | holder *h*: wait (insert, update, speculative under `Wait`) and redo the check |
| *c* committed or my own, *d* empty or aborted or another's in progress | **taken**: `UniqueViolation` for a plain write. For `insert_speculative`, `UniqueViolation` if the snapshot sees the version and otherwise `KeyBusy(CommittedAfterSnapshot { commit_ts, latest })` |

Rows 3 and 4 overlap when *c* committed and *d* is another's in progress. Row 3 wins: the delete may still commit, so the writer waits.

**insert_speculative(row, index)**: as insert, but the unique check on `index` runs *before* the row step (a value, layer §6.5), and its result maps to `KeyBusy` or `UniqueViolation`. On success, `id = txn.register_spec(rel)`, remember `id → version`, and return `Installed(SpecToken::engine_new(rel, id))`.

**complete_speculative(t)**: `txn.resolve_spec(rel, id)`. Nothing else changes. Returns the version's locator.

**abort_speculative(t)**: stamp the version's `deleter = uncommitted(its creator's sub-id)` (the same id, so other transactions ignore the entry), record the stamp, call `waits.signal(&index_key_lock(index, key))`, then `txn.resolve_spec`.

**fetch_latest_committed(loc, proj)**: `snap = txn.fresh_snapshot().await?`. Walk from the target to the newest. If the newest's creator or deleter is another transaction in progress, wait on it under `Wait` and start again. Return the newest version that `snap` sees as `Live`, with its own locator, or `None`.

**build_index(def)**: add `def` to `indexes`, then run the unique check over all current versions if `def.unique`. A violation is `StatementFailed`, because the index already exists for this transaction. **truncate**: mark every version dead under a pending flag that commit makes permanent and abort clears. The fake takes commit and abort from `ScriptedWorld`'s end-of-transaction hook (§11 X1, R-T9).

**Rollback** needs no code: an aborted creator is invisible by status, and an aborted deleter reads as `Live`.

### 8.4 The wait loop

```text
loop {
    match check() { Proceed => break, Value(v) => return v, Holder(h) => {} }
    match policy { DontWait => return InProgress(top(h)), SkipLocked => return NotVisible, Wait => {} }
    let what = WaitFor::Txn(h)                          // or SharedLocks(key) for a manager holder,
                                                        // or Key(index_key_lock) for a speculative key
    match waits.wait(ctx, txn.xid(), what, txn.wait_deadline()).await {
        Resolved  => continue,                          // start over: the row may have moved
        TimedOut  => return InProgress(top(h))          // or StatementFailed{WaitExpired} after the row step
        Deadlock  => return Err(Deadlock),
        LockTimeout => return Err(LockTimeout),
    }
}
```

The fake never holds its mutex across the `await`. It reads, releases, waits, and reads again.

### 8.5 Errors and injection

- `inject_io(n)`: the (n+1)-th later call fails with `Io("injected")` after `txn.poison(Poison::Io)`. Suite S10 uses it.
- `Fenced` comes from `check_live` after the test bumps the world's epoch.
- The fake never panics on caller input. It does panic on internal invariant breaks, such as a zero creator or a locator from another table, which are bugs.

### 8.6 Maintenance

`FakeMaint::step(budget)` visits at most `budget.pages × 64` versions per call. It marks a version dead when no snapshot can see it: its deleter (not lock-only) committed at or below the watermark, or its creator aborted. It relinks `next` around dead versions. `Progress { more }` is true while unvisited versions remain. The watermark is a test input because the real one comes from `cascade-watermark` (D-12, §11 X9).

### 8.7 What the fake deliberately does not model

Pages, the log, latches, cost, HOT, toast, and capacity limits other than `max_row_bytes`. Its scans are snapshots of the version list taken at the first `next`, filtered row by row through visibility, so a scan never sees a version created after it started. That matches the engines, because such versions carry a later sub-id or a later commit timestamp.

## 9. The conformance suite (C11)

### 9.1 Harness

```rust
pub trait TableHarness {
    type Table: TableAccess;
    /// A fresh, empty table for the descriptor.
    fn create(&mut self, desc: &TableDescriptor) -> Self::Table;
    /// Begin, commit, abort, savepoints, clock, fencing, deadlock and timeout injection.
    fn world(&self) -> &ScriptedWorld;
    /// The locator movement to expect.
    fn flavour(&self) -> LocatorFlavour;
    /// Injects an Io failure on the table's next call. Default: the case is skipped.
    fn inject_io(&mut self, _t: &Self::Table) -> bool { false }
}

pub struct SuiteReport { pub passed: Vec<&'static str>, pub skipped: Vec<(&'static str, &'static str)> }

pub async fn run_all<H: TableHarness>(h: &mut H) -> SuiteReport;   // S2–S13
pub async fn s2_basic<H: TableHarness>(h: &mut H, r: &mut SuiteReport);
// … one pub async fn per group
```

Failures panic with the case id (`"S4.3: …"`), as `cascade-storage-api`'s suites do. A capability-gated case records itself in `skipped` with the reason, and each engine's test asserts the exact expected skip list, so an engine cannot pass by omission without it showing.

**Fixtures.** `fixtures::t_id_email(kind)` gives the overview's table: `t(id int8 PK, email text UNIQUE, name text)`. `fixtures::t_nopk()` gives a heap table with no key.

**Concurrency inside a test.** Two transactions are two futures on one thread. A waiter that should block is polled once with `futures_lite::future::poll_once` and must return `Pending`. The test then commits or aborts the holder through the world and polls again. That tests the wake protocol without threads.

### 9.2 Cases

Each case has an id. "T1" and "T2" are transactions in read committed unless a case says otherwise.

| Group | Cases | Layer § |
| :--- | :--- | :--- |
| **S1 visibility** | Every row of both truth tables, against scripted statuses, including sub-ids resolved through their top and a rolled-back sub-id. Lives in `cascade-txn-api`'s own tests (X1). The suite re-runs it so that an engine build proves it links the same rule | §6.6 |
| **S2 basic** | .1 insert then fetch in the same statement: not visible (statement horizon); next statement: visible. .2 T2 does not see T1's row until T1 commits; then sees it in a new statement. .3 aborted insert never visible. .4 seq scan returns exactly the visible rows, each with a locator that `fetch` resolves. .5 `RowTooLarge` above the limit, and the context is not poisoned | §6.3, §6.8 |
| **S3 update/delete** | .1 locator movement: heap always new, IOT same unless the pk changes. .2 old locator still fetches the old version for an old snapshot. .3 update of a row deleted by a committed T2 → `NotVisible`. .4 a scan over a table with N updated rows emits N rows, never versions. .5 delete then fetch in a later statement → `None` | §6.5 |
| **S4 conflicts** | .1–.6 each row of the conflict table under `Wait` (blocks, then proceeds or returns the value after the holder ends). .7 the same under `DontWait` → `InProgress { holder: top }`. .8 under `SkipLocked` → `NotVisible`. .9 the wait bound expires → `InProgress`. .10 `Waited::Deadlock` → `Err(Deadlock)`, `LockTimeout` → `Err(LockTimeout)`. .11 a refused write allocated no sub-id | §6.5 |
| **S5 re-check** | .1 `CommittedAfterSnapshot` then `fetch_latest_committed` returns the newest version and its locator. .2 it waits for an in-progress holder. .3 it returns `None` when the latest committed state is deleted | §6.8 |
| **S6 row locks** | .1 the 4×4 compatibility matrix, as blocking or not. .2 a plain read never waits on any lock. .3 a share lock survives a non-key update by another transaction, and blocks a later key update on the new version. .4 `SkipLocked`. .5 a lock taken before a savepoint survives rolling back an update made after it (W3). .6 locks end with the transaction | §6.10 |
| **S7 unique** | .1 duplicate committed key → `UniqueViolation` (IOT pk: value) or `StatementFailed` (index after the row step). .2 after `StatementFailed` every call returns `StatementFailed { PriorFailure }` and commit is refused, and after rollback to a savepoint no trace remains (W2). .3 key deleted by a T2 that committed after the snapshot is free. .4 key inserted by such a T2 is a violation. .5 nulls never conflict. .6 in-progress holder → wait | §11.4 |
| **S8 speculative** | .1 each of the four results. .2 abort makes the row vanish for the writer at once. .3 two transactions race on one key, and the loser is woken by the abort (`WaitFor::Key`), not by the commit. .4 unresolved tokens come back from `end_statement` with their relation, and `abort_unresolved` aborts them | §6.9 |
| **S9 scans** | .1 construction does no work (no wait, no error even when fenced). .2 `rescan` at the same snapshot. .3 `mark`/`restore` where advertised (skip otherwise). .4 drop without `close`. .5 index scan in key order, both directions. .6 index-only results equal fetched results. .7 a scan stays usable while the same transaction updates the rows it yields (C15) | §6.8 |
| **S10 errors** | .1 `Fenced` after an epoch bump, then `Poisoned` on every call. .2 injected `Io` poisons. .3 `SchemaStale` for a write under an older version, while reads still work. .4 `Deadlock`, `LockTimeout` from the wait seam. .5 `sqlstate()` of each variant matches §5.8 | §6.11 |
| **S11 DDL** | .1 `build_index` then index scans and unique checks use it. .2 a unique build over duplicates fails. .3 `truncate` rolled back leaves the rows; committed, removes them | §6.12 |
| **S12 maintenance** | .1 `step` never removes a version an open snapshot can see. .2 it removes versions below the watermark, and `more` becomes false | §11.7 |
| **S13 statements and savepoints** | .1 `UPDATE` over its own scan never meets its new versions. .2 `INSERT … SELECT` from the same table reads only earlier rows. .3 a nested statement sees the outer statement's earlier writes. .4 `ROLLBACK TO SAVEPOINT` hides exactly the writes since it, on both flavours | §6.6 |

### 9.3 Property tests (C12)

`proptest` as a dev-dependency of `cascade-access-api` only. A generator produces up to 40 steps over 3 transactions and 4 keys: begin, insert, update, delete, lock (each mode and policy), savepoint, rollback-to, commit, abort, new statement. The runner applies each step to the fake and to the engine under test through two `ScriptedWorld`s fed identical scripts. After every step, every live transaction's seq scan must return the same set of rows (by value), and every outcome must be equal after locators are mapped through a per-run table from engine locator to fake locator. A step that blocks in one must block in the other. Shrinking gives a minimal failing script.

## 10. Landing plan

Each commit keeps the workspace building and the gate green (`cargo clippy -D warnings`, tests).

0. **Precondition (external):** `cascade-txn-api` exists with api §4 and `ScriptedWorld` (§11 X1). Step 1 cannot start without it.
1. **Types:** api §5, compiling, with unit tests for `conflicts_with`, the lock-key encoding, `sqlstate()`, the key helpers and `TableDescriptor::validate`.
2. **Traits:** api §6.
3. **The fake:** api §8, with its own unit tests for resolution in both flavours.
4. **The suite:** api §9, S2–S13, run against both fake flavours in `tests/conformance.rs`, plus the property test fake-against-fake (heap flavour against IOT flavour, comparing rows by value), which checks the runner itself.
5. **`cascade-access-dispatch`:** the enums with the fake arm, `abort_unresolved`, and a test that runs the whole suite through `Table`.
6. **Doc sync:** layer §6.3 (signatures with `OpCtx` and `Projection`), §6.7 (`ReadView`), §6.10 (`RowLockTable`), §6.11 (`FailCause`, `RowTooLarge`, `Unsupported`), for whichever of C14–C23 are accepted.

## 11. External dependencies

What 05-access assumes and requires from other components. Each row has the step that first needs it and the state today. "Required" items are requests to that component's owner. None of them is designed here.

### X1. `cascade-txn-api` (L4, transaction layer) — needed by Step 1

**State today:** does not exist. D-3 decided it is created now with what the access contract consumes. Who builds it is D-2.

| # | Requirement | Why 05-access needs it |
| :--- | :--- | :--- |
| R-T1 | The types and traits of api §4, with those names and signatures | Step 1 compiles against them |
| R-T2 | `Snapshot::visible` and `row_visible` implement layer §6.6's truth tables exactly, and are pure and total | Every engine and index decides visibility through them, and nothing else interprets a version word |
| R-T3 | `VisibilitySource::status` never blocks and resolves a sub-id through its top (W6). A commit flips one entry | Engines call it under a page pin |
| R-T4 | **Publication before the closed timestamp** (W6): a commit is `Committed` in the status source before the closed timestamp passes its timestamp | A snapshot must classify each word the same way for its whole life |
| R-T5 | `WaitSource::wait` resolves `Txn(x)` when *x* ends or *x*'s sub-id is rolled back, `SharedLocks(k)` when no conflicting holder remains, and `Key(k)` on `signal(k)` or when the holder ends. It returns `Deadlock` or `LockTimeout` when the lock manager decides so. It wakes by waker and never spins | The wait loop (§8.4) |
| R-T6 | `RowLockTable` (C14): non-blocking acquire, conflicting holders, copy to a new key; locks released at transaction end and on rollback to a savepoint for the rolled-back sub-ids; holders that have ended are never reported | A11 shared locks and W3 |
| R-T7 | `TxnCtx` as api §4: `write_xid` allocates a sub-id on the statement's first write; `begin_statement` takes a new horizon each statement; `check_live` covers epoch, poison and failed; `mark_failed` is cleared only by rollback to an enclosing savepoint; `end_statement` returns unresolved speculative ids; `record_stamp` accepts engine positions (D-1) | Layer §6.5–§6.7 |
| R-T8 | `ReadView` (C15): an owned, `Clone` view sharing the context's poison and failed flags | Scans that outlive a borrow |
| R-T9 | Feature `fake`: `ScriptedWorld` implementing every service, with `begin(iso) -> TxnCtx`, `commit`, `abort`, `savepoint`, `rollback_to`, a manual closed timestamp, `bump_epoch`, `inject_deadlock(waiter)`, `inject_lock_timeout(waiter)`, a wait bound setting, and an end-of-transaction hook the fake can observe | The fake and the suite run against it |
| R-T10 | `Isolation { ReadCommitted, RepeatableRead, Serializable }` is defined there | `ScriptedWorld::begin` and statement snapshots depend on it |

### X2. The transaction layer's runtime pieces (D-2) — needed by Step 4

**State today:** the runtime's `txnmgr` is only an HLC clock. These have no implementation: the status table, the closed timestamp, the lock manager (with deadlock detection, sharded by key), the background stamper that consumes `record_stamp` (D-1), and savepoint bookkeeping. Steps 1–3 run on `ScriptedWorld`. Step 4 (the heap on the real transaction layer) is blocked until they exist and pass suite S1–S13 through the real services.

### X3. `cascade-types` (L0) — Step 1, exists

Assumes, as they are today: `VersionWord`'s encoding (empty / uncommitted xid / committed HLC, bit 63 as tag), `TxnId` below 2^63, `Hlc` ordering, `OpCtx` with a deadline, `LocalBoxFuture`. 05-access needs no change to it for Step 1.

### X4. `cascade-pgtypes` (07-sql, named exception A7/D-9d) — Step 1, exists

Assumes: `KeyBytes` order is the SQL order of the encoded values; `encode_int8`, `encode_bool`, `encode_text`, `encode_null` and `concat` are stable (they are an on-disk format from Step 3); `Locator` and `IndexEntry` as today. Requires: text input refuses NUL before a value reaches 05-access (PostgreSQL 22021); text keys are `C`-collation only until `cascade-datum` (D-9g). When the key contract moves to `cascade-datum`, 05-access follows it, and `Datum`, `Row`, `Projection` and `ColumnSet` move with it (layer §5.2).

### X5. `cascade-error` (L0) — needed when the executor surfaces access errors (Step 3)

**State today:** `Error` holds strings, and `sqlstate()` maps by variant only (for example every `Txn` error is 40000). Requires: a way to carry a specific SQLSTATE, such as a code field or a variant with one, so `TamError::sqlstate()` survives the conversion. Until then `From<TamError> for cascade_error::Error` maps to `Txn` or `Storage` with the message, and loses the code.

### X6. `cascade-telemetry` (L0) — Step 1, exists

Assumes `Span` and `MetricsRegistry` as today, which are no-ops in v1. 05-access records per-call counters (outcomes, waits, conflicts) through `MetricsRegistry::add`.

### X7. The catalog (`cascade-catalog`, L6) — needed by Step 3

| # | Requirement |
| :--- | :--- |
| R-C1 | Assigns `RelId` and `IndexId`, stable and never reused |
| R-C2 | Builds `TableDescriptor` (api §5.7) for every table, including built-in bootstrap descriptors for its own tables (architecture §4.6) |
| R-C3 | Issues `SchemaVersion`s that only increase, leases one to each statement (`StmtCtx::schema_version`), and gives engines a new descriptor at the next statement boundary after a change |
| R-C4 | Coordinates every DDL phase above the contract: only `build_index` and `truncate` reach the engine, and it holds the table lock that blocks writes during `build_index` (layer §6.12) |
| R-C5 | Owns dropping a table's or index's storage (D-15, Step 8) |

### X8. The executor (`cascade-sql-exec`, L7) — needed when the executor adopts the contract (Step 3)

The contract's caller obligations. The suite cannot check these, so they are listed for the executor's design:

| # | Obligation |
| :--- | :--- |
| R-E1 | Calls `begin_statement` before and `end_statement` after every statement, and passes the unresolved list to `abort_unresolved` |
| R-E2 | Never inspects a `RowLocator`, and never uses one after its statement |
| R-E3 | Maps `SELECT … FOR …` to `lock_row` with its `WaitPolicy`. Raises `lock_not_available` (55P03) on `InProgress` under `DontWait` |
| R-E4 | Under read committed, follows `CommittedAfterSnapshot` with `fetch_latest_committed` and re-evaluates the predicate. Under repeatable read, raises `serialization_failure` (40001) |
| R-E5 | On `StatementFailed`, rolls back to the statement's implicit savepoint or aborts. On `Io`, `Fenced`, `Deadlock` or `LockTimeout`, aborts the transaction |
| R-E6 | Drives scans to completion or drops them. It may write through the context while a scan is live (C15) |

### X9. Components needed only by later steps

| Component | What 05-access will require | Step | State |
| :--- | :--- | :--- | :--- |
| Page cache, mini-transaction seam (`cascade-bpm`) | One injected seam to change pages with their redo records, as one unit (layer §9.1); pins never held across an await | 2–3 | Raw write closures only; seam open (D-4) |
| `cascade-redo` and the page store | Apply engine records through one dispatch point (A2, A10) and replay them | 2–3 | Only heap insert type 5 |
| `cascade-page` formats | Heap flag bits and hints (D-9b), the free-space map (D-9a), B-tree kinds (B decisions) | 3–6 | Heap kind 1 only |
| Segment allocation (`ScopePages`) | Survives a restart (D-8) | 3 | Open |
| `cascade-watermark` | The cleanup watermark for maintenance (D-12) | 4–5 | Does not exist |
| `cascade-undo` | The IOT's undo records (D-7) | 7 | Pre-pivot stub |
| `cascade-tempstore` | Session-private storage for temporary tables | 9 | Does not exist |
| Runtime (`cascade-runtime`) | Thread-per-core: a task never migrates, a waker re-queues on the task's own core, `!Send` futures allowed (layer §6.5, §8) | 1 (assumed), 4 (used) | Exists |

## 12. Open questions

| Question | Needed before |
| :--- | :--- |
| Which `Datum` types the fake must support beyond `Int64`, `Bool`, `Text` and `Null` | When the executor first uses the fake on real queries |
| `RowRef`'s out-of-line value handle, and whether it should carry a column mask for wide tables | The heap's row codec (Step 3) |
| How a descriptor says "temporary": a persistence field, or a third `StorageKind` | Step 9 |
| How an engine receives a new descriptor at a statement boundary: rebuilt from it, or a `reconfigure` call | Step 3 (with the catalog, X7 R-C3) |
| The exact shape of `TableStats` for the planner | The planner's design (layer §13.3) |

## 13. Decisions

Statuses: **Accepted** (by whom, when) or **Proposed**.

| # | Decision | Status |
| :--- | :--- | :--- |
| C1 | Three crates: `cascade-txn-api` (L4 seams), `cascade-access-api` (contract, fake, suite), `cascade-access-dispatch` (dispatch enums above the engines). Fixes the dependency cycle of layer §6.1 and settles D-3 | Accepted (Kenneth, 2026-11-27) |
| C2 | `Table`, `TableScan` and `Maintenance` have a `Fake` arm behind the `fake` feature; production builds have the three arms of A13 | Accepted (Kenneth, 2026-11-27) |
| C3 | `TableAccess`: a public trait with `async fn`s and associated `Scan` and `Maint` types. Only `cascade-access-dispatch` and tests name it. `OpCtx` is an argument of every call; only `lock_row` takes a `WaitPolicy` | Accepted (Kenneth, 2026-11-27) |
| C4 | `SpecToken` and `ResumeHint` are concrete opaque types in the contract, created by engines through hidden constructors | Accepted (Kenneth, 2026-11-27) |
| C5 | `TxnCtx` is a concrete struct in `cascade-txn-api`, built from `TxnClock`, `VisibilitySource`, `WaitSource` and `EpochSource`. `begin_statement` and `fresh_snapshot` are `async`. `check_live` is the one epoch-and-poison check | Accepted (Kenneth, 2026-11-27) |
| C6 | `end_statement` returns unresolved speculative tokens and the executor aborts them through `Table::abort_unresolved`; replaces "the engine aborts them" in layer §6.9 | Accepted (Kenneth, 2026-11-27) |
| C7 | `TamError` gains `RowTooLarge { len }` (PostgreSQL 54000) and `Unsupported` (for signatures that exist before their step) | Accepted (Kenneth, 2026-11-27) |
| C8 | `WaitFor::SharedLocks` takes an opaque `LockKey`, and `Waited` carries `Deadlock` and `LockTimeout`, so L4 names no L5 type | Accepted (Kenneth, 2026-11-27): the opaque `LockKey` with C1, `Waited` carrying the aborts with D-10 |
| C9 | No `TableEnv` in the contract. Each engine's constructor takes its own environment | Accepted (Kenneth, 2026-11-27) |
| C10 | One fake engine with two locator flavours, `Heap` and `Iot`, built on the shared visibility rule | Accepted (Kenneth, 2026-11-27) |
| C11 | The suite: a `TableHarness`, groups S1–S12, single-threaded interleaving, capability-gated cases reported as skips | Accepted (Kenneth, 2026-11-27) |
| C12 | Engine-against-fake property tests, with `proptest` as a dev-dependency of `cascade-access-api` | Accepted (Kenneth, 2026-11-27) |
| C13 | Key helpers (`key_of`) live in the contract crate on top of the `cascade-pgtypes` encoders, under the A7 exception | Accepted (Kenneth, 2026-11-27) |
| C14 | Row locks in the lock manager get an interface: `RowLockTable` (try-acquire, conflicting holders, copy to a new key) and `LockMode` in `cascade-txn-api`, as a fifth service in `TxnServices`. Amends C5. A requirement on L4 (X1 R-T6) | Proposed (v0.2) |
| C15 | Scans and maintenance steps own their state: a scan holds a `ReadView` cloned from `TxnCtx` instead of borrowing it, so the executor can write while a scan is live. Scan and maintenance methods take `OpCtx`. Replaces round-3 V25's "a scan keeps a shared borrow". A requirement on L4 (R-T8) | Proposed (v0.2) |
| C16 | After `StatementFailed`, later calls return `StatementFailed { cause: PriorFailure }` (25P02), because the context, being L4, cannot keep the original L5 cause | Proposed (v0.2) |
| C17 | `TamError::sqlstate()` with the mapping of api §5.8 (`Fenced` → 08007, `SchemaStale` → 40001, `Poisoned` → 25P02, …) | Proposed (v0.2) |
| C18 | `#[non_exhaustive]` only on `TamError`, `FailCause`, `Datum` and `ColumnType`; structs that engines build have public fields. Corrects v0.1 §5 | Proposed (v0.2) |
| C19 | Lock-key encoding: a tag byte (`0x00` pk, `0x01` Tid with segment, `0x02` unique-index key), so no promise about catalog id spaces is needed | Proposed (v0.2) |
| C20 | `abort_unresolved` is a free function in `cascade-access-dispatch` taking a `RelId` lookup, since tokens can span tables | Proposed (v0.2) |
| C21 | `#[allow(async_fn_in_trait)]` on `TableAccess`, `RowScan` and `MaintenanceStep`, because their futures are meant to be `!Send`; and no `Send + Sync` bound on `TableAccess` | Proposed (v0.2) |
| C22 | `ColumnType` is a local enum mirroring `Datum`, so that the A7 exception stays limited to the key contract | Proposed (v0.2) |
| C23 | Key helpers are `encode_key` (nulls encoded), `unique_probe_key` (`None` on a null) and `nonunique_entry_key`. A NUL in a text key maps to `Unsupported`. Refines C13's `key_of` | Proposed (v0.2) |
