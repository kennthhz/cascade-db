# cascade-access-api design — the table access contract, its fake and its suite

**Status:** Draft v0.1 for review, 2026-11-27. This is the implementation design for layer Step 1 (layer §12). It turns the contract of layer §6 into crates, modules, types and tests. It assumes the recommendations of decision register D-3 and D-5(1) and proposes how to settle them. Decisions are numbered C1–C13 (§12). C1, C5, C6 and C10 were accepted by Kenneth on 2026-11-27; the rest are proposed.

**Upstream:** the [layer design](../../AI/access-layer-design.md) v0.2 — §6 (the contract), §6.13 (the fake and the suite), §8 (execution model), §12 (Step 1). [`docs/architecture-design.md`](../../../../docs/architecture-design.md) v0.10.2 — Rules 1, 2, 6, 9, 10; §4.5 (`cascade-txn-api`); §4.6. The [decision register](../../AI/decision-register.md). Citation prefixes follow layer §2.2; "api §" means this document.

**Code baseline:** `main` at `5eedd63`. Reused as they are: `cascade-types` (`TxnId`, `Hlc`, `VersionWord`, `Epoch`, `ScopeId`, `PageId`, `OpCtx`, `LocalBoxFuture`), `cascade-pgtypes` (`KeyBytes`, `KeyRange`, `Locator`, the key encoders). The conventions copied from `cascade-storage-api`: `fake` and `conformance` cargo features, on by default; suites as generic `async fn`s over a harness trait; `futures-lite::block_on` to drive them.

---

## 1. What Step 1 delivers

1. The contract of layer §6 as compiled Rust, with no engine behind it yet.
2. An in-memory fake engine that keeps every promise of the contract. It is the executor's test double and the reference model for the engines.
3. One conformance suite, run against the fake now and against each engine as it lands.
4. The dispatch types the executor holds (`AnyTable`, `TableScan`, `Maintenance`), with the fake as their only arm until the first engine lands.

Nothing in Step 1 touches a page, the log or the buffer pool. It depends on none of the open gaps D-1, D-2, D-4 or D-7.

## 2. Two problems in layer §6 that the crate layout must solve

**The enum cannot live in the contract crate (C1).** `AnyTable` names `IotTable`, `HeapTable` and `TempTable`, which live in the engine crates, and every engine crate depends on `cascade-access-api`. Putting the enum in the contract crate is a dependency cycle. The enum has to live in a crate above all engines. Layer §6.1 already says every `match` on it lives "in the executor, in this repository"; this design gives it a crate of its own so the executor, the catalog's bootstrap and the tests share one copy.

**L4 types are declared in L5 (C5, C8).** `Snapshot`, `TxnStatus`, `VisibilitySource`, the visibility rule, `WaitSource`, `TxnCtx` and `StmtCtx` are transaction-layer concepts (architecture §4.5) that layer §6.6–§6.7 declares in the access crate. Index kinds and the IOT's undo walk use the same rule, and the lock manager implements `WaitSource`. Separately, `WaitFor::SharedLocks` names `TableHandle` and `RowLocator`, which are L5 types, so the L4 lock manager could not implement it without depending upward.

## 3. Crates (C1)

| Crate | Layer | Contains | Depends on |
| :--- | :--- | :--- | :--- |
| `cascade-txn-api` (new) | L4 | `Snapshot`, `TxnStatus`, `VisibilitySource`, the visibility rule, `WaitSource`, `WaitFor`, `LockKey`, `TxnClock`, `EpochSource`, `TxnCtx`, `StmtCtx`, `SchemaVersion`. Feature `fake`: a scripted world implementing all four injected traits | `cascade-types` |
| `cascade-access-api` (new) | L5 | The contract types of layer §6.2–§6.5 and §6.11–§6.12, the `TableAccess` trait (api §6), key helpers. Features `fake` (the fake engine) and `conformance` (the suite) | `cascade-types`, `cascade-txn-api`, `cascade-pgtypes` (A7), `cascade-telemetry` |
| `cascade-access` (new) | L5, above the engines | `AnyTable`, `TableScan`, `Maintenance`: inherent `async fn`s that `match` and delegate to `TableAccess` | `cascade-access-api`, and each engine crate as it lands |

`cascade-txn-api` is the architecture's crate of that name, started early with only what the access contract consumes. It grows the `ScopeTxn` and coordinator contracts in the transaction layer's own design. Creating it now settles D-3 without a later move of public types.

## 4. `cascade-txn-api`: the seams the engines consume

### 4.1 Visibility (layer §6.6, unchanged)

```rust
pub struct Snapshot { pub read_ts: Hlc, pub own: Option<TxnId> }
pub enum TxnStatus { InProgress, Committed { ts: Hlc }, Aborted }
pub trait VisibilitySource: Send + Sync { fn status(&self, xid: TxnId) -> TxnStatus; }
pub enum Visibility { Visible, CommittedAfterSnapshot { commit_ts: Hlc }, InProgress { holder: TxnId }, Aborted }
pub enum RowVisibility { Live, NotBornYet, Deleted { own: bool } }

impl Snapshot {
    pub fn visible(&self, word: VersionWord, src: &dyn VisibilitySource) -> Visibility;
    pub fn row_visible(&self, creator: VersionWord, deleter: Option<VersionWord>,
                       src: &dyn VisibilitySource) -> RowVisibility;
}
```

The functions are pure and total. A zero creator word is a corrupt page, not a visibility answer: `row_visible` panics in debug builds and returns `NotBornYet` in release, and the engine refuses the page before calling it (layer §6.6, first row of the table).

### 4.2 Waiting (layer §6.5) and lock keys (C8)

```rust
/// What a lock is held on, in bytes the lock manager never interprets.
pub struct LockKey { pub rel: u64, pub key: Vec<u8> }

pub enum WaitFor { Txn(TxnId), SharedLocks(LockKey) }
pub enum Waited { Resolved, TimedOut, Deadlock, LockTimeout }

pub trait WaitSource: Send + Sync {
    fn wait<'a>(&'a self, ctx: &'a OpCtx<'_>, waiter: TxnId, what: WaitFor,
                deadline: Option<Instant>) -> LocalBoxFuture<'a, Waited>;
}
```

Two changes from layer §6.5. `SharedLocks` takes an opaque `LockKey`, which the access crate builds from a relation id and the locator's encoded bytes, so L4 names no L5 type. And `Waited` carries the two aborts the lock manager decides, deadlock victim and `lock_timeout`, because otherwise there is no channel through which the engine learns them; the engine turns them into `TamError::Deadlock` and `TamError::LockTimeout`.

### 4.3 The transaction context (C5)

Layer §6.7 leaves `TxnCtx` to the transaction layer and fixes only what engines consume. Engines and the fake need it concrete, so it is a struct built from four injected services:

```rust
pub trait TxnClock: Send + Sync {
    fn read_ts(&self) -> Hlc;                                     // a fresh read timestamp
    fn wait_closed<'a>(&'a self, ts: Hlc) -> LocalBoxFuture<'a, Result<(), Fenced>>;
}
pub trait EpochSource: Send + Sync { fn current(&self, scope: ScopeId) -> Epoch; }

pub struct TxnServices {
    pub clock: Arc<dyn TxnClock>, pub status: Arc<dyn VisibilitySource>,
    pub waits: Arc<dyn WaitSource>, pub epochs: Arc<dyn EpochSource>,
}

pub struct TxnCtx { /* scope, epoch, isolation, xid: Option<TxnId>, services,
                       stmt: Option<StmtCtx>, poisoned: Option<Poison>, pending_spec: Vec<SpecId> */ }

impl TxnCtx {
    pub fn new(scope: ScopeId, epoch: Epoch, iso: Isolation, xid: Option<TxnId>,
               services: TxnServices) -> Self;           // the transaction layer calls this
    pub async fn begin_statement(&mut self) -> Result<&mut StmtCtx, Fenced>;
    pub fn end_statement(&mut self) -> Vec<SpecId>;      // C6
    pub async fn fresh_snapshot(&mut self) -> Result<Snapshot, Fenced>;
    pub fn check_live(&self) -> Result<(), Liveness>;    // epoch and poison, before any page
    pub fn poison(&mut self, why: Poison);
    pub fn xid(&self) -> Option<TxnId>;
    pub fn services(&self) -> &TxnServices;
}

pub struct StmtCtx { pub snapshot: Snapshot, pub serial: u64, pub schema_version: SchemaVersion }
```

- **`begin_statement` and `fresh_snapshot` are `async`.** Layer §6.7 has them synchronous, but a fresh snapshot must wait until its read timestamp is closed (heap §5.2.1), which is an await. Under read committed every statement takes a new snapshot, so `begin_statement` awaits too; under repeatable read it returns at once.
- **`check_live` is the epoch check of layer §6.7 in one place.** It compares the context's epoch with `EpochSource::current` and checks the poison flag. Every engine entry point calls it first.
- **`SchemaVersion` moves here** from layer §6.2, because `StmtCtx` carries it.
- **Speculative tokens (C6).** The context records each token an engine installs (as a `SpecId`). `end_statement` returns the unresolved ones; `cascade-access` provides `AnyTable::abort_unresolved(txn, ids)`, which the executor calls. Layer §6.9 said "the engine aborts them at `end_statement`", but `TxnCtx` cannot call an engine without depending on L5. A debug assertion catches an executor that drops the list.
- **Reserved for D-1.** If stamping through the log is accepted, the context gains `record_stamp(StampPos)`. It is not added until D-1 is decided.

### 4.4 The scripted world (feature `fake`)

`ScriptedWorld` implements all four traits for tests: a status map with `begin() -> TxnId`, `commit(xid) -> Hlc` and `abort(xid)`; a manual clock whose closed timestamp the test advances; a lock-manager stand-in that wakes `Txn` waiters when a status changes and `SharedLocks` waiters when the last holder releases; deadlock and timeout injection (`inject_deadlock(waiter)`); and an epoch the test can bump to simulate fencing. Wake-ups use `std::task::Waker`s; nothing spins.

## 5. `cascade-access-api`: the contract types

Modules and their content. Every type is as layer §6.2–§6.5 and §6.11–§6.12 define it, except where a row says otherwise.

| Module | Types | Changes from layer §6 |
| :--- | :--- | :--- |
| `ids` | `RelId`, `IndexId`, `ColId`, `StorageKind`, `TableHandle`, `Tid` | — |
| `row` | `Datum`, `Row`, `Projection`, `ColumnSet`, `RowRef` | `RowRef` holds `Vec<Datum>` in projection order, plus a reserved slot for the out-of-line fetch handle (heap §8). `#[non_exhaustive]` |
| `locator` | `RowLocator`, conversion to and from `cascade-pgtypes::Locator`, `RowLocator::lock_key(rel)` | `lock_key` builds the opaque `LockKey` of api §4.2 |
| `outcome` | `WriteOutcome`, `Conflict`, `RowLockMode`, `WaitPolicy`, `Locked`, `SpeculativeResult`, `SpecToken` | `SpecToken` is concrete and opaque (C4): `{ rel: RelId, id: SpecId }`, constructible only through `SpecToken::engine_new`, which is `#[doc(hidden)]` |
| `scan` | `ScanRow`, `Direction`, `SeqScanOpts`, `IndexScanOpts`, `ScanParams`, `ResumeHint`, `SsiReporter` | `ScanParams { range: Option<KeyRange> }`. `ResumeHint` is opaque bytes (C4). `SsiReporter` is an empty reserved struct |
| `caps` | `AmCapabilities`, `TableStats`, `Budget`, `Progress` | `Budget { pages: u32 }`, `Progress { more: bool }` for now |
| `descriptor` | `TableDescriptor`, `ColumnDef`, `KeyShape`, `IndexDef`, `ToastPolicy` | `TableEnv` is **not** here (C9) |
| `error` | `TamError` | Adds `RowTooLarge { len }` and `Unsupported(&'static str)` (C7). SQLSTATE mapping through `cascade-error` |
| `keys` | `key_of(row, cols) -> Option<KeyBytes>` | `None` when a column is null, which is how the unique check skips nulls (layer §11.4). Built on the `cascade-pgtypes` encoders (A7, C13) |

**Exhaustiveness (Rule 9).** `WriteOutcome`, `Conflict`, `SpeculativeResult`, `RowLockMode` and `WaitPolicy` are matched exhaustively by the executor on purpose: a new outcome must be a compile error at every match. Everything else is `#[non_exhaustive]`.

**`TableEnv` stays with the engines (C9).** Layer §6.12 sketches one `TableEnv` holding the mini-transaction seam, the page cache, `ScopePages` and the two transaction seams. A temporary table needs none of the page-side parts, and naming them here would make the contract crate depend on the page cache, whose seam is still open (D-4). So each engine's constructor takes its own environment, and the contract fixes only `TableDescriptor`. The transaction seams reach engines through `TxnCtx`, not through the environment.

## 6. The `TableAccess` trait (C3)

The plug the suite is written against (layer §6.1, A13). The executor never names it; only `cascade-access` and test code do.

```rust
pub trait TableAccess: Send + Sync {
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
                      mode: RowLockMode, wait: WaitPolicy) -> Result<Locked, TamError>;

    async fn insert_speculative(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, row: &Row,
                                index: IndexId) -> Result<SpeculativeResult, TamError>;
    async fn complete_speculative(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, t: SpecToken) -> Result<RowLocator, TamError>;
    async fn abort_speculative(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, t: SpecToken) -> Result<(), TamError>;

    async fn fetch(&self, ctx: &OpCtx<'_>, txn: &TxnCtx, loc: &RowLocator,
                   proj: &Projection) -> Result<Option<RowRef>, TamError>;
    async fn fetch_latest_committed(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx,
                                    loc: &RowLocator) -> Result<Option<(RowLocator, RowRef)>, TamError>;
    fn seq_scan(&self, txn: &TxnCtx, proj: &Projection, opts: SeqScanOpts) -> Self::Scan;
    fn index_scan(&self, txn: &TxnCtx, index: IndexId, range: KeyRange,
                  proj: &Projection, opts: IndexScanOpts) -> Self::Scan;

    async fn build_index(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx, def: &IndexDef) -> Result<(), TamError>;
    async fn truncate(&self, ctx: &OpCtx<'_>, txn: &mut TxnCtx) -> Result<(), TamError>;
    fn maintenance(&self) -> Self::Maint;
}
```

- **`async fn` in a trait** is stable at the workspace's `rust-version` 1.75. The futures carry no `Send` bound, as the runtime requires (layer §6.1). The trait is used only through generics, so it does not need to be object-safe.
- **Only `lock_row` takes a `WaitPolicy`**, as in layer §6.3. In PostgreSQL, `NOWAIT` and `SKIP LOCKED` exist only on `SELECT … FOR …`; `UPDATE`, `DELETE` and `INSERT … ON CONFLICT` always wait, bounded by the transaction layer and by `lock_timeout`. So every write and `insert_speculative` behave as `Wait`. Layer §6.5's phrase "the executor chooses it per statement" is read as "per locking statement".
- **`OpCtx` is explicit** as the first argument (Rule 8), where layer §6.3 omitted it for readability.
- **`RowScan`** is the scan protocol of layer §6.4 (`next`, `rescan`, `supports_mark_restore`, `mark`, `restore`, `close`) as a trait with `async fn`s. **`MaintenanceStep`** is `async fn step(&mut self, budget: Budget) -> Progress`.

## 7. `cascade-access`: the dispatch types (C2)

```rust
pub enum AnyTable {
    #[cfg(feature = "fake")] Fake(FakeTable),
    // Heap(HeapTable)  — Step 3
    // Iot(IotTable)    — Step 7
    // Temp(TempTable)  — Step 9
}
pub enum TableScan { #[cfg(feature = "fake")] Fake(FakeScan), /* engine arms as they land */ }
pub enum Maintenance { #[cfg(feature = "fake")] Fake(FakeMaint), /* engine arms as they land */ }
```

Each method is an inherent `async fn` that `match`es and forwards, written by a small `macro_rules!` so that the arms cannot drift from the trait. **The fake is a fourth arm behind the `fake` feature.** Without it the executor's tests could not hold a fake through `AnyTable`, and the fake would stop being the executor's test double. The production build turns the feature off, so the shipped enum has exactly the three arms of A13. Until the heap lands in Step 3, `AnyTable` has only the fake arm and the crate is used only by tests.

## 8. The fake engine (C10)

**Shape.** `FakeTable` is an `Arc` around one `std::sync::Mutex<FakeState>`, which is never held across an `await`. The state is a list of versions:

```rust
struct Version { id: u64, creator: VersionWord, deleter: VersionWord, lock_only: bool,
                 updates_key: bool, next: Option<u64>, row: Row }
```

plus the shared-lock table, the open speculative tokens, the table's indexes (as `IndexDef`s; entries are derived from versions, not stored) and a statistics counter.

**Two locator flavours.** Created with `FakeFlavor::Heap` or `FakeFlavor::Iot`:

- `Heap`: a locator is `RowLocator::Tid` naming one version (a synthetic `PageId` from the version id). Every update moves the locator, and fetches walk forward along `next` exactly as heap §5.2 does, including `Deleted` hops and `NotBornYet` as "no row".
- `Iot`: a locator is `RowLocator::Pk`. Updates keep the locator unless the primary key changes; fetches resolve the key to the newest version and walk back to the one the snapshot sees, as kept §1 does.

So the suite can check locator movement against both semantics, and each engine is compared with the flavour that matches it.

**Semantics it implements, each by the rule the real engines use:**

| Promise | How the fake keeps it |
| :--- | :--- |
| Visibility | Every read calls `Snapshot::row_visible` from `cascade-txn-api`, with no deleter when `lock_only` is set |
| Conflicts and waits | The conflict table of heap §4.4, driven through `TxnCtx`'s `WaitSource`; under `Wait` it loops resolve, sleep, start over (layer §6.5) |
| Row locks | Exclusive modes stamp `deleter` with `lock_only`; shared modes go in the fake's lock table under `RowLocator::lock_key`. Locks move to the new version on update (layer §6.10) |
| Unique check | The table of layer §11.4 over every version with the same `key_of`, ignoring entries whose create and delete name the same in-progress transaction |
| Speculative insertion | Layer §6.9: install, complete (no change), abort (stamp the deleter and wake key waiters) |
| Rollback | Nothing to do, as on the heap: an aborted creator is invisible by status |
| Errors | `check_live` first on every call; `Fenced` and injected `Io` poison the context |
| Maintenance | `step` removes versions whose creator and deleter are resolved and older than a watermark the test sets |

**What the fake deliberately does not model:** pages, the log, latches, capacity limits other than a configurable row-size limit (for `RowTooLarge`), and any cost. `count_star_fast`, `index_only` and ordered sequential scans are capabilities it advertises per flavour: ordered for `Iot`, unordered for `Heap`.

The fake is also the base of the temporary-table engine (layer §11.6). That reuse is left to Step 9; nothing here depends on it.

## 9. The conformance suite (C11)

**Harness.** As in `cascade-storage-api`:

```rust
pub trait TableHarness {
    type Table: TableAccess;
    fn create(&mut self, desc: &TableDescriptor) -> Self::Table;
    fn world(&self) -> &ScriptedWorld;          // begin, commit, abort, clock, fencing, injection
    fn flavour(&self) -> LocatorFlavour;        // Heap or Iot: what locator movement to expect
}
```

Every engine runs the suite against `ScriptedWorld`, not against the real transaction layer, which does not exist yet (D-2). When it does, the same suite runs once more against it.

**Concurrency inside a test.** Two transactions run as two futures on one thread, interleaved with `futures-lite`'s `zip` and `poll_once`. A waiter that should block is polled once and must return `Pending`; the test then commits or aborts the holder through the world and polls again. This tests the wake protocol without threads; `loom` coverage belongs to the shared structures of each engine, not to the contract.

**The groups**, one `pub async fn` each, every one a list of numbered cases tied to a section of layer §6:

| Group | What it checks | Layer § |
| :--- | :--- | :--- |
| S1 visibility | The two truth tables, every row, against scripted statuses. Pure, no table | §6.6 |
| S2 basic writes and reads | Insert, fetch, seq scan; own writes visible; others' not until commit; aborted rows never | §6.3, §6.8 |
| S3 update and delete | Locator movement per flavour; `NotVisible` on a row gone for the snapshot; scans emit rows, never versions | §6.5 |
| S4 conflicts and waits | Every row of the heap §4.4 table: writes under `Wait`, and `lock_row` under all three policies, including the wait bound expiring | §6.5 |
| S5 the re-check | `CommittedAfterSnapshot` then `fetch_latest_committed` returns the newest version with its own locator; it waits for in-progress holders | §6.8 |
| S6 row locks | The compatibility matrix of the four modes; a plain read never waits; locks survive a moving update; `SkipLocked` | §6.10 |
| S7 unique and nulls | The unique-check table; nulls never conflict | §11.4 |
| S8 speculative insertion | All four results; abort makes the row vanish for the writer; two transactions racing on one key, the loser woken by the abort, not the commit; unresolved tokens returned by `end_statement` | §6.9 |
| S9 scans | Construction does no work; `rescan` at the same snapshot; `mark`/`restore` where advertised; drop without `close`; index scans in key order; index-only results | §6.8 |
| S10 errors | `Fenced` after an epoch bump, then `Poisoned`; injected `Io` poisons; `SchemaStale`; `Deadlock` and `LockTimeout` from the wait seam; `RowTooLarge` | §6.11 |
| S11 DDL phase | `build_index` then index scans and unique checks use it; `truncate` rolled back leaves the rows | §6.12 |
| S12 maintenance | `step` never removes a version some open snapshot can see; it does remove versions below the watermark | §11.7 |

A case that depends on a capability (mark/restore, index-only, ordered sequential scan) is skipped when `capabilities()` does not advertise it, and the skip is reported, so an engine cannot pass by omission without it showing.

**Engine differential and property tests (C12).** Beyond the fixed cases, a property test generates random sequences of begin, insert, update, delete, lock, commit and abort over a few keys, runs them through the fake and through the engine under test, and checks that every snapshot's scan and every outcome agree. It needs `proptest`, which is not yet a workspace dependency; it would be added as a dev-dependency of `cascade-access-api` only.

## 10. Landing plan

Each commit keeps the workspace building and the gate green.

1. **`cascade-txn-api`**: the types of api §4, the visibility rule with its truth-table tests (S1 lives here as plain unit tests and is re-exported to the suite), and `ScriptedWorld`.
2. **`cascade-access-api` types and trait**: api §5 and §6, compiling, with doc tests for the key helpers.
3. **The fake**: api §8, with its own unit tests for the chain walk in both flavours.
4. **The suite**: api §9, S2–S12, run against both fake flavours in `tests/conformance.rs`.
5. **`cascade-access`**: the dispatch enums with the fake arm, and a test that runs the suite through `AnyTable` to prove the forwarding is complete.
6. **Doc sync**: layer §5.1, §6.1, §6.5, §6.7, §6.9, §6.12 and the architecture's §4.5–§4.6 updated for whichever of C1–C13 are accepted.

## 11. Open questions

| Question | Needed before |
| :--- | :--- |
| `Datum` grows with the type layer; which types the fake must support beyond `Int64`, `Bool`, `Text` and `Null` | When the executor first uses the fake on real queries |
| Whether `RowRef` should carry a column-mask instead of a full `Vec<Datum>` for wide tables | Before the heap's row codec is written (Step 3) |
| Whether `Isolation` belongs to `cascade-txn-api` now or waits for the transaction layer's design | Commit 1 |
| The exact shape of `TableStats` for the planner | The planner's design (layer §13.3) |

## 12. Decisions

Statuses: **Accepted** (by whom, when) or **Proposed**.

| # | Decision | Status |
| :--- | :--- | :--- |
| C1 | Three crates: `cascade-txn-api` (L4 seams), `cascade-access-api` (contract, fake, suite), `cascade-access` (dispatch enums above the engines). Fixes the dependency cycle of layer §6.1 and settles D-3 | Accepted (Kenneth, 2026-11-27) |
| C2 | `AnyTable`, `TableScan` and `Maintenance` have a `Fake` arm behind the `fake` feature; production builds have the three arms of A13 | Proposed |
| C3 | `TableAccess`: a public trait with `async fn`s and associated `Scan` and `Maint` types. Only `cascade-access` and tests name it. `OpCtx` is an argument of every call; only `lock_row` takes a `WaitPolicy` | Proposed |
| C4 | `SpecToken` and `ResumeHint` are concrete opaque types in the contract, created by engines through hidden constructors | Proposed |
| C5 | `TxnCtx` is a concrete struct in `cascade-txn-api`, built from `TxnClock`, `VisibilitySource`, `WaitSource` and `EpochSource`. `begin_statement` and `fresh_snapshot` are `async`. `check_live` is the one epoch-and-poison check | Accepted (Kenneth, 2026-11-27) |
| C6 | `end_statement` returns unresolved speculative tokens and the executor aborts them through `AnyTable::abort_unresolved`; replaces "the engine aborts them" in layer §6.9 | Accepted (Kenneth, 2026-11-27) |
| C7 | `TamError` gains `RowTooLarge { len }` (PostgreSQL 54000) and `Unsupported` (for signatures that exist before their step) | Proposed |
| C8 | `WaitFor::SharedLocks` takes an opaque `LockKey`, and `Waited` carries `Deadlock` and `LockTimeout`, so L4 names no L5 type | Accepted in part with C1: the opaque `LockKey`. `Waited` carrying the aborts is proposed |
| C9 | No `TableEnv` in the contract. Each engine's constructor takes its own environment | Proposed |
| C10 | One fake engine with two locator flavours, `Heap` and `Iot`, built on the shared visibility rule | Accepted (Kenneth, 2026-11-27) |
| C11 | The suite: a `TableHarness`, groups S1–S12, single-threaded interleaving, capability-gated cases reported as skips | Proposed |
| C12 | Engine-against-fake property tests, with `proptest` as a dev-dependency of `cascade-access-api` | Proposed |
| C13 | Key helpers (`key_of`) live in the contract crate on top of the `cascade-pgtypes` encoders, under the A7 exception | Proposed |
