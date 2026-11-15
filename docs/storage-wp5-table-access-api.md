# WP5 — Table access API: detailed design

Status: DRAFT for review · 2026-11-14 · supersedes the earlier WP5 draft (folds in the 2026-11-14 review of HLD v2 §10 read as the API's detailed design — provenance and finding→decision mapping at the end)
Upstream: docs/storage-subsystem-hld-v2.md §10 (R1; enabled by R5 "engines differ only below the access API"). Per the upstream doc's instruction this doc keeps the Responsibility / Key decisions / Invariants / Interfaces / Open questions headings and closes or explicitly carries the §17 questions it inherits. Where a decision cannot be made inside WP5, it is listed under "HLD amendments required" — the upstream rule is that detailed designs must not break an invariant without changing that doc first.
Depends on: WP2 (WAL writer + txn manager) owns the internals of `TxnCtx`, `Snapshot`, status table, and waiting; WP1 owns byte formats this API names (`KeyBytes`, version words, index entries). This doc **pins those interfaces**.

Current repo state: the surface does not exist yet — `crates/05-access` holds stubs (`cascade-heap` defines `Tid` only; `cascade-btree` is 7 lines). The debug stand-in engine remains the pgwire backend until T6.5. This doc is the contract the heap engine (T6.4+) and the future IOT engine implement against.

---

## 1. Scope and industry precedent

### 1.1 What this layer is

The API between the core planner/execution engine and tables. It speaks tables and rows: scans, fetches, insert / update / delete / upsert, row locking, index builds, maintenance. Everything above it decides *policy* (isolation matrix, conflict arbitration, FK semantics, batching); everything below decides *physics* (pages, version chains, undo, WAL, indexes).

It has two faces, and every surveyed system separates them the same way:

- **Planner face** — capabilities and statistics only. The optimizer never calls data functions (PG's `amcanorder`-style flags, SQL Server's metadata costing, Oracle's access-path selection).
- **Executor face** — the row operations. Executed exclusively on the core that owns the data (subject to D8's routing model).

### 1.2 Prior art

| System | Public pluggable table API? | Boundary |
| :---- | :---- | :---- |
| PostgreSQL | **Yes** — Table AM (PG 12, 2019) | `TableAmRoutine` function pointers: `tuple_insert/delete/update`, `scan_begin/getnext/rescan/end`, `tuple_satisfies_snapshot`; parallel `IndexAmRoutine` for indexes. Executor-side DML nodes (`ModifyTable`) wrap AM calls; ON CONFLICT uses the speculative-insert protocol |
| MySQL / MariaDB | **Yes** — `handler` (`sql/handler.h`), oldest such API | `ha_rnd_*` (table scan), `ha_index_*` (index scan), `ha_write_row/update_row/delete_row`, `ha_position`; per-table `ENGINE=`; MVCC entirely inside the engine. Reports outcomes as `HA_ERR_*` codes — our value-outcome design (D3) inverts this correctly |
| SQL Server | Architecturally yes, not pluggable | QP ↔ storage-engine "access methods" boundary. **Best precedent for Cascade's shape**: every table is heap- or clustered-index-organized (IOT-like), and every operator works over both through the same interfaces |
| Oracle | No public API, internally layered | KCB/KCR/KTR/KDS layers; heap/IOT/clustered coexist behind the same internal data layer. Notable: IOT secondary indexes carry a **physical guess rowid** with PK-descent fallback — an engine-internal locator trick that validates D2's opacity |
| SQLite | **Yes** — virtual tables (`xFilter/xNext/xColumn/xRowid/xUpdate`) | Simplest existence proof of a row-level plugin contract |

Lessons adopted: **PG** — full scan protocol with rescan and mark/restore (D11); speculative insertion as the upsert primitive (D12); per-table AM choice. **MySQL** — engine-owned concurrency reporting. **SQL Server** — one API honestly covering heap + clustered organizations. **Oracle** — locator opacity pays for engine-internal fallback strategies.

## 2. Responsibility

Give the executor exactly one interface for row operations, whatever the storage organization behind it. The API hides: row location and its movement, version chains (undo deltas or append-only tuples), secondary index maintenance, cleanup (purge/prune/vacuum), and page mechanics. It exposes: engine-neutral reads at a snapshot, writes that report conflicts and violations as values, and engine-agnostic scan entry points. It offers no page-level escape hatch — every write routes through the scope's txn manager and WAL writer inside the engine.

Both organizations are first-class: **IOT** (PK-clustered B-tree, row in leaf, newest version in place, old versions as undo deltas — upstream §11) and **heap** (append-only versions, HOT chains, prune + vacuum — upstream §12). One API, two implementations, one visibility rule over both.

## 3. Key decisions

### D1 — Enum dispatch, not `dyn` or generics (absorbs review M5)

Upstream §10 sketches `trait TableAccessMethod` with `async fn` and `impl RowStream` returns. As drawn, that is AFIT — **not dyn-compatible** — and the sketch is not valid Rust. The same constraint was met on the storage seam (`PageStore`, F4) with per-call generics; here generics are wrong because the executor holds *mixed* collections of tables. Resolution:

```rust
pub enum AnyTable { Iot(crate::iot::IotTable), Heap(crate::heap::HeapTable) }
```

`AnyTable` re-exposes the whole surface as inherent `async fn`s that `match self` and delegate (two arms, negligible cost, `!Send`-native; matches the runtime's existing dispatch idiom). Rejected: `async-trait` crate (per-call allocation, erases the !Send story), manual boxed-future vtables (unsafe-adjacent boilerplate for two variants). **Requires an upstream sketch amendment** (§10, §16 — see "HLD amendments required").

### D2 — Locators are opaque, statement-scoped, move only through outcomes

`RowLocator` (`Pk(KeyBytes)` / `Tid(Tid)`) is opaque to the executor: stored, passed back, never inspected. Payloads are nonetheless persisted formats owned below the API — `Tid { page, slot }` exists in `cascade-heap` and is what index entries carry (§13); `KeyBytes` is the WP1-normalized key encoding. Oracle's guess-rowid stays an engine-internal optimization forever: that is the payoff of opacity.

Validity is **same-statement, same-txn** (upstream §10 invariant, kept verbatim). Movement is reported only via `WriteOutcome::Done { locator }`; engines never mutate a caller-supplied locator in place. `StmtCtx.serial` (D10) exists so engines can stamp locators in debug builds as a cheap staleness tripwire — not part of the signature contract.

### D3 — Concurrency outcomes are values; waiting is engine-side and bounded (absorbs M1, L1)

`WriteOutcome` carries conflicts and violations as values; only infrastructure failures are `Err` (D9). The isolation-policy matrix (§7) lives in the executor: RC re-checks on `CommittedAfterSnapshot`; RR/SSI fail; `InProgress` means the caller chooses its policy *per call site* (wait / no-wait / skip-locked).

The waiting itself is **engine-side** (PG `XactLockTableWait`, InnoDB lock waits — never a caller-side poll loop):

| `WaitPolicy` | Behavior |
| :---- | :---- |
| `Wait` | Engine blocks (bounded, WP2-configured) on the holder resolving via the status table; then acquires, or returns `CommittedAfterSnapshot { commit_ts, latest }`, or `NotVisible` if the row is gone |
| `DontWait` | Returns `Conflict::InProgress { holder }` immediately |
| `SkipLocked` | Skips the row: returns `NotVisible` for it (the executor moves on) |

Bound expiry under `Wait` returns `InProgress` — bounded waits prevent unbounded stalls; exact bounds are WP2 configuration.

Read/write asymmetry in not-visible signaling is deliberate and pinned: reads use `Option::None`; writes use `WriteOutcome::NotVisible` (the executor maps `None` → skip row, `NotVisible` → RC re-check via `fetch_latest_committed`).

### D4 — One visibility substrate; engines compose, never reinvent (absorbs M6)

The single §7 rule, shared by rows, undo chains, and index entries, with composition pinned so engines cannot diverge:

```rust
impl Snapshot {
    /// §7 word-level rule: committed commit_ts ≤ read_ts, or own uncommitted write.
    pub fn visible(&self, word: VersionWord) -> Visibility;
    /// The ONLY row/entry composition. Engines and index code call this, nothing else.
    pub fn row_visible(&self, creator: VersionWord, deleter: Option<VersionWord>) -> RowVisibility;
}

pub enum RowVisibility {
    Live,                                       // materialize the version
    NotBornYet,                                 // creator invisible at read_ts → walk to older / skip entry
    Deleted { in_progress: Option<TxnId> },     // deleter resolved or in-flight → walk to older / skip entry
}
```

`row_visible(creator, delete_word)` with `delete_word = None`-equivalent is exactly the index-entry rule of §13. The executor never sees version words, undo pointers, chains, or page bytes — `RowRef` presents decoded, already-visible data (invariant 4).

### D5 — Engines maintain secondary indexes inside writes

Confirmed from upstream "Proposed" (§16): insert/update/delete perform all index maintenance internally; heap skips index writes on HOT updates, IOT skips when no indexed column changed; the executor has no index-write entry point. Unique checking is engine-internal and surfaces as `UniqueViolation` or the speculative protocol (D12) — never as a probe API for the executor.

### D6 — Read surface: point fetches, pinned RC re-check, scans that carry locators (absorbs M2, L2)

- `fetch(snap, loc, proj) -> Option<RowRef>` — resolve at a snapshot; `None` if not visible.
- `fetch_latest_committed(txn, loc)` — **the RC re-check, pinned**: performs the `Wait` behavior of D3 internally, then returns the newest version visible to {own uncommitted writes ∪ committed ≤ now} plus its **current** locator; `None` if the row was deleted. This is exactly the EPQ-style re-check recipe: the executor re-evaluates its predicate on the returned row and decides (keep / retry / abandon).
- `lock_row` returns the row with the lock — SELECT FOR UPDATE costs one descent, not two: `Result<Locked, TamError>` where `Locked { outcome: WriteOutcome, row: Option<RowRef> }` (row present iff acquired).
- `seq_scan` — IOT: PK-ordered leaf walk; heap: page-order walk.
- `index_scan` — with `index_only` when covered columns suffice; possible on both engines because entries carry their own visibility (§13). No visibility map, ever.
- Locator-resolution differences stay below the API: IOT primary-index scans return rows straight from leaves; heap indexes (including PK) return `Tid`s fetched from the heap.

`RowRef` **owns a decoded copy** of the visible columns — engines never hand out references into latched pages (F3 copy-out discipline, landed with the BPM guards). See D15 for TOAST.

### D7 — `Maintenance` is an enum with an async `step`, not `&dyn` (absorbs M5, part 2)

Upstream sketches `fn maintenance(&self) -> &dyn Maintenance`; with async work inside, `dyn` is impossible (D1's rationale). Substitute the same enum idiom:

```rust
pub enum Maintenance { Iot(IotUndoPurge), Heap(HeapPruneVacuum) }
impl Maintenance {
    /// One bounded unit of background work, on the owning core's reactor.
    pub async fn step(&mut self, budget: Budget) -> Progress;
}
```

The housekeeper drives it per relation with budgets — no table-wide stops (upstream principle 7). FSM access stays engine-internal. **Requires an upstream sketch amendment.**

### D8 — Scope ownership and the cross-scope read model (**OPEN — H2; the one architecture decision this doc cannot make alone**)

Writes: every write call runs on the core owning the relation's scope; the engine rejects a write targeting a second scope (upstream §4). Misrouting is a programming error — debug-assert, not a runtime path.

Reads across scopes are the open question, because upstream §10 says "all calls run on the core that owns the scope" while §8 says cross-core page access is "by message… (detailed design to confirm for cross-core readers)". Both resolutions satisfy §7's read rule; they must be pinned because they decide `TableScan`'s shape and the executor's structure:

- **Option A — message-to-owner reads (recommended).** A cross-scope SELECT's coordinator ships sub-scans to owner cores; scan state lives on the owner; rows stream back over channels; the caller holds a channel-pair handle (still a `!Send` data plane). Consequences: caches stay per-core and owned (§8's default); the landed T6.2 cross-core BPM demotes to a single-scope-write concern; closed-ts waits happen owner-side; cost is per-scan channel setup and the executor becomes a remote-row consumer. Glommio discipline applies (endpoints connect on different executors; same-executor sequential connect deadlocks spinning).
- **Option B — shared-page cross-core reads.** Weaken §10 to "writes run on the owner core; reads may run wherever pages are reachable" — viable precisely because T6.2 exists (shared pool, per-frame mutex, ticket+generation re-verify). Consequences: no channel plumbing; scan cursors live on the calling core; but §8's locality and no-shared-lock arguments are forfeited, and T6.2's gate becomes load-bearing for reads.

Either way this requires an **HLD amendment** (§8 and §10 sentences, §16 row). Status: **Decided 2026-11-14 — Option A (message-to-owner reads)**.

### D9 — Errors are infrastructure-only; txns poison on failure (absorbs M4)

```rust
pub enum TamError {
    Io(std::io::Error),        // WAL/storage failure — outcome unknown (§6)
    Fenced { scope: ScopeId }, // epoch lost (§4) — outcome unknown
    SchemaStale { min_leased: SchemaVersion }, // §14 lease rule: write under a retired schema
    Poisoned,                  // this txn already failed once; see below
}
```

Rule: any `Io` or `Fenced` **poisons** `TxnCtx` — every later operation on that txn returns `Poisoned` without touching storage. The executor aborts; the outcome of the failed txn is decided by recovery, exactly as §6 prescribes ("WAL failure = outcome unknown = fence; recovery decides from the durable log"). Everything the txn can decide about is a value in `WriteOutcome` / `UpsertArbitration` (D12), never an error.

### D10 — Statement is a first-class boundary (absorbs M3)

Upstream's central invariant references "the same statement" yet defines no statement object, and RC takes a fresh snapshot per statement (§7). Pin it:

```rust
impl TxnCtx {
    pub fn begin_statement(&mut self) -> &mut StmtCtx;  // RC: fresh Snapshot; RR/SSI: txn's snapshot reused
    pub fn end_statement(&mut self);                    // releases statement-scoped state
}
pub struct StmtCtx {
    pub snapshot: Snapshot,              // the read_ts + own txn_id (§7)
    pub serial: u64,                     // locator tripwire source (D2)
    pub schema_version: SchemaVersion,   // leased version validated at write time (D14/L5)
}
```

Speculative tokens must resolve before `end_statement` (D12); engines may abort any left open as statement teardown.

### D11 — The scan protocol (absorbs H1, L3, L4)

A scan is a pull iterator with join support — the PG lesson. Items carry locators so scanned UPDATE/DELETE targets need no second probe:

```rust
pub struct ScanRow { pub row: RowRef, pub loc: RowLocator }

pub enum TableScan { Iot(IotScan), Heap(HeapScan) }   // enum dispatch, D1
impl TableScan {
    pub async fn next(&mut self) -> Option<Result<ScanRow, TamError>>;
    /// Nested-loop inner: restart with new outer parameters, same snapshot.
    pub async fn rescan(&mut self, params: &ScanParams) -> Result<(), TamError>;
    /// Merge-join support — ordered index scans and IOT leaf walks only.
    pub fn supports_mark_restore(&self) -> bool;
    pub fn mark(&mut self);
    pub async fn restore(&mut self) -> Result<(), TamError>;
    pub async fn close(self);
}
```

Pinned semantics: construction is infallible and does no I/O (the first `next()` opens); visibility filtering is engine-internal via `Snapshot::row_visible` — the executor only ever receives visible rows; `rescan` re-reads at the same snapshot; dropping without `close` releases pins synchronously (and engines never hold page pins across an await — F3). Capabilities feed the planner face:

```rust
pub struct AmCapabilities {
    pub ordered_scans: bool,     // pathkey planning
    pub mark_restore: bool,      // merge-join eligibility (redundant with supports_mark_restore; planner reads statically)
    pub index_only: bool,        // covered scans
    pub count_star_fast: bool,   // hint only; correctness never depends on it
}
```

SSI predicate-read reporting is reserved now, defined by WP9: `SeqScanOpts`/`IndexScanOpts` carry `ssi: Option<SsiReporter>` (per-core, internally synchronized); scans report at page/range granularity pending WP9's sampling rules — the hard case §17 names.

### D12 — Upsert via speculative insertion, not executor composition (absorbs H3)

Composing upsert above the API (probe, then insert-or-update) is atomicity-unsafe: two txns upserting a fresh key both miss the probe, both insert, and the loser dies on a violation the composition cannot arbitrate. PG solved this with speculative insertion; Cascade takes the same shape with the engine keeping the arbitration:

```rust
pub struct SpecToken { /* engine-private */ }

pub enum SpeculativeResult {
    Installed(SpecToken),            // no conflict — MUST be resolved before statement end (D10)
    KeyBusy(Conflict),               // InProgress{holder} → wait + retry; CommittedAfterSnapshot{latest}
                                     //   → switch to the update branch via fetch/fetch_latest_committed
    UniqueViolation { index: IndexId, key: KeyBytes }, // committed-live conflict, per §13's rule
}

impl AnyTable {
    pub async fn insert_speculative(&self, txn: &mut TxnCtx, row: &Row, index: IndexId)
        -> Result<SpeculativeResult, TamError>;
    pub async fn complete_speculative(&self, txn: &mut TxnCtx, token: SpecToken)
        -> Result<RowLocator, TamError>;            // row becomes a normal uncommitted insert
    pub async fn abort_speculative(&self, txn: &mut TxnCtx, token: SpecToken) -> Result<(), TamError>;
}
```

The speculative row is visible only to its own txn; a conflicting inserter sees `KeyBusy(InProgress)` and waits on us via the status table (D3 machinery). The executor composes ON CONFLICT DO NOTHING / DO UPDATE from this: `Installed` → branch decision → `complete` or `abort`; `KeyBusy(CommittedAfterSnapshot)` → update branch against `latest`; `UniqueViolation` → per constraint semantics. One engine operation, one arbitration point; the two-branch WAL work (insert records vs update records) happens below the API. (Alternative considered and rejected: a composite `upsert(row, index, OnConflict)` — more executor-friendly, less orthogonal, and it hides the wait/retry loop that RC semantics need to see.)

### D13 — The key contract is co-owned with WP1 (absorbs M7)

`KeyRange`/`KeyBytes` is the API's most-used read parameter and WP1's most-constrained encoding. Pinned shape; WP1 owns the bytes:

```rust
pub struct KeyRange { pub lower: Bound<KeyBytes>, pub upper: Bound<KeyBytes> } // Unbounded | Included | Excluded
```

Rules: multi-column keys are order-preserving concatenated encodings — collation and numeric normalization live in WP1's normalization step, and `KeyRange` equality/ordering is defined *on normalized bytes*; prefix scans are ranges over the concatenation; null ordering (sort-first vs sort-last) is pinned in WP1, not here. Joint deliverable: WP5 accepts the shape above, WP1 freezes the encoding — neither ships without the other.

### D14 — DDL methods are physical phases only; schema versions come from the statement (absorbs M8, L5)

`build_index` / `truncate` are ordinary scope-local operations in this trait, but upstream §4 makes DDL the one cross-scope transaction (2PC, catalog scope coordinating). Division of labor: the TAM method executes the **physical phase** only; catalog coordination, 2PC, and leases are WP10's, invoked above this API. Schema-version validation is enforced at write time against `StmtCtx.schema_version` (D10); a write under a retired version returns `TamError::SchemaStale` (statement-fatal).

### D15 — `RowRef` owns its buffer and, for TOAST, an engine handle (absorbs L6)

Lazy TOAST (upstream open question, direction confirmed) + F3 copy-out means `RowRef` cannot be a borrowed view. It owns its decoded projection buffer plus an engine handle and locator so toasted columns can materialize on demand:

```rust
impl RowRef {
    pub fn value(&self, col: ColId) -> Result<Option<&Datum>, ToastNeeded>;
    pub async fn toast_value(&mut self, col: ColId) -> Result<Option<Datum>, TamError>; // materializes into the owned buffer
}
```

Final TOAST chunk format/compression is WP7's; this decision only pins that laziness lives behind an owning `RowRef`, never behind a borrow.

## 4. Invariants

1. A locator returned by a write or scan is valid for that txn's later calls **in the same statement**; beyond that, movement is observable only via `fetch_latest_committed` or a fresh `WriteOutcome::Done` (D2).
2. Every write goes through the scope's txn manager and WAL writer; the API offers no page-level escape hatch. *(upstream §10, verbatim)*
3. Visibility is decided only by `Snapshot::visible` / `Snapshot::row_visible` (§7), never inside the executor, never by an engine-private shortcut (D4).
4. The executor never observes version words, undo pointers, HOT chains, or page bytes; `RowRef` presents decoded, already-visible data (D6).
5. All futures are `!Send`; writes run on the core owning the relation's scope; the read-routing model is D8 (**open** — both options preserve this invariant's write half).
6. An index entry is never visible before the row version it points to (§13, surfaced as: index→row joins happen inside the engine under the entry's own visibility).
7. A slot/leaf is never reused while a locator can resolve through an index the engine maintains (heap: §12's slot rule; IOT: locator *is* the PK).
8. A poisoned txn never touches storage again; its outcome is recovery's to decide (D9).
9. A speculative insert is resolved (`complete`/`abort`) before statement end; engines abort stragglers at teardown (D10, D12).
10. No page pins are held across an await; dropping a scan or `RowRef` releases pins synchronously (F3 discipline; D11, D15).

## 5. Interfaces

Substrate types pinned here, owned elsewhere:

```rust
pub struct Hlc(pub u64);            // 48-bit ms | 16-bit logical (WP2)
pub struct TxnId(pub u64);          // 63-bit per-scope counter, top bit reserved (WP2)
pub struct VersionWord(pub u64);    // top bit 1 = uncommitted txn_id, 0 = commit_ts (WP1/WP2)
pub struct ScopeId(/* … */);        // log domain identity (WP2)
pub struct SchemaVersion(pub u64);  // leased schema version (WP10)
pub struct KeyBytes(pub Vec<u8>);   // normalized key encoding — see D13 (WP1 joint)
pub struct IndexId(/* … */);        // stable UUID (§14)
pub struct RelId(/* … */);
pub struct ColId(pub u16);
pub enum StorageKind { Iot, Heap }

pub struct Snapshot { pub read_ts: Hlc, pub own: Option<TxnId> }   // WP2 constructs
pub enum Visibility {                                              // word-level, §7
    Visible,
    CommittedAfterSnapshot { commit_ts: Hlc },
    InProgress { holder: TxnId },
    Aborted,
}
```

The API surface (`AnyTable` delegates both arms):

```rust
pub struct Tid { pub page: PageId, pub slot: u16 }   // exists in cascade-heap; packs to u64
pub enum RowLocator { Pk(KeyBytes), Tid(Tid) }       // opaque (D2)

pub enum WriteOutcome {
    Done { locator: RowLocator },
    Conflict(Conflict),
    UniqueViolation { index: IndexId, key: KeyBytes },
    NotVisible,
}
pub enum Conflict {
    InProgress { holder: TxnId },
    CommittedAfterSnapshot { commit_ts: Hlc, latest: Option<RowLocator> },
}

pub enum RowLockMode { KeyShare, Share, NoKeyUpdate, Update }
pub enum WaitPolicy { Wait, SkipLocked, DontWait }          // semantics pinned in D3
pub struct Locked { pub outcome: WriteOutcome, pub row: Option<RowRef> }  // D6

pub enum Direction { Forward, Backward }
pub struct Projection(/* ordered ColId list */);
pub struct ColumnSet(/* changed-column bitmap */);
pub struct Row(/* values; Datum owned by cascade-types */);
pub struct IndexDef { /* name, key cols, unique — physical detail is WP8's */ }
pub struct TableStats { pub row_estimate: f64, pub dead_fraction: f32, pub free_pages: u32 } // planner face; shape grows with the planner
```

Operations:

```rust
impl AnyTable {
    // planner face
    pub fn handle(&self) -> TableHandle;            // TableHandle { rel, kind }
    pub fn capabilities(&self) -> &AmCapabilities;  // D11

    // writes — txn must own the scope (D8); engine maintains indexes (D5)
    pub async fn insert(&self, txn: &mut TxnCtx, row: &Row) -> Result<WriteOutcome, TamError>;
    pub async fn update(&self, txn: &mut TxnCtx, loc: &RowLocator, new: &Row,
                        changed: &ColumnSet) -> Result<WriteOutcome, TamError>;
    pub async fn delete(&self, txn: &mut TxnCtx, loc: &RowLocator) -> Result<WriteOutcome, TamError>;
    pub async fn lock_row(&self, txn: &mut TxnCtx, loc: &RowLocator,
                          mode: RowLockMode, wait: WaitPolicy) -> Result<Locked, TamError>;

    // upsert — speculative protocol (D12)
    pub async fn insert_speculative(&self, txn: &mut TxnCtx, row: &Row, index: IndexId)
        -> Result<SpeculativeResult, TamError>;
    pub async fn complete_speculative(&self, txn: &mut TxnCtx, token: SpecToken)
        -> Result<RowLocator, TamError>;
    pub async fn abort_speculative(&self, txn: &mut TxnCtx, token: SpecToken) -> Result<(), TamError>;

    // reads (D6, D11)
    pub async fn fetch(&self, snap: &Snapshot, loc: &RowLocator,
                       proj: &Projection) -> Result<Option<RowRef>, TamError>;
    pub async fn fetch_latest_committed(&self, txn: &TxnCtx, loc: &RowLocator)
        -> Result<Option<(RowLocator, RowRef)>, TamError>;
    pub fn seq_scan(&self, snap: &Snapshot, proj: &Projection, opts: SeqScanOpts) -> TableScan;
    pub fn index_scan(&self, snap: &Snapshot, index: IndexId, range: KeyRange, dir: Direction,
                      proj: &Projection, index_only: bool) -> TableScan;

    // DDL and maintenance (D7, D14)
    pub async fn build_index(&self, txn: &mut TxnCtx, def: &IndexDef) -> Result<(), TamError>;
    pub async fn truncate(&self, txn: &mut TxnCtx) -> Result<(), TamError>;
    pub fn maintenance(&self) -> Maintenance;
    pub fn stats(&self) -> TableStats;
}
```

Supporting shapes: `SeqScanOpts { dir, resume: Option<ResumeHint>, ssi: Option<SsiReporter>, page_budget: u32 }`; `ScanParams` (rescan bindings); `ResumeHint` (page/key position, engine-private payload); `Budget`/`Progress` for `Maintenance::step`.

## 6. HLD amendments required

Per the upstream doc's own rule, accepting this detailed design requires amending HLD v2:

| § | Change |
| :---- | :---- |
| §8 | Resolve "cross-core readers: detailed design to confirm" to **message-to-owner reads** (D8, decided 2026-11-14) |
| §10 sketch | Replace the AFIT trait + `&dyn Maintenance` with the enum-dispatch shape (D1, D7); add `insert_speculative`/`complete_speculative`/`abort_speculative` (D12); add `StmtCtx`/statement boundary (D10); define `TamError` + poisoning (D9); add `row_visible` composition (D4); scan items carry locators + rescan/mark-restore (D11) |
| §16 | "Access API" row Proposed → Decided (D3/D5/D6 confirm the four Proposed items); new rows: speculative-insert upsert (Decided); engine-side bounded waits (Decided); scan protocol with rescan/mark-restore (Decided); cross-scope read routing (Pending — D8) |
| §17 | Remove "batch APIs / SSI reporting / TOAST" rows' WP5 ownership where superseded; cross-scope routing question moves to the decision log as Pending |

## 7. Open questions

Closed here: dispatch mechanism (D1/D7); error channel + poisoning (D9); upsert primitive (D12); scan protocol (D11); wait semantics (D3); RC re-check recipe (D6); statement boundary (D10); key-range shape (D13); TOAST laziness behind an owning `RowRef` (D15).

Carried forward:

| Question | From | Owner / needed before |
| :---- | :---- | :---- |
| ~~Cross-scope read routing~~ | §8/§10 | **Closed 2026-11-14 — message-to-owner reads (D8, Option A)** |
| Batch APIs (multi-row insert, bulk load) + WAL record shape | §10, §17 | WP1 reserves the bulk-load type now; API extension at WP6/WP7 |
| SSI predicate-read reporting semantics (sampling granularity) | §10, §17 | WP9 (hook reserved in D11) |
| TOAST chunk format, compression, versioning of toasted values | §15 | WP7 (laziness pinned in D15) |
| Locator validity beyond one statement (cursor pinning) | sharpened §10 invariant | Executor/cursor design; until then same-statement only (D2) |
| Wait-queue structure and fairness | §7 | WP2 (owns status-table waiting; D3 pins only the observable contract) |
| `TableStats` shape the planner needs | §10 | Planner design (explicitly out of scope) |
| ~~Exact `KeyRange` bound semantics for nullable/multi-column keys~~ | D13 | Closed at WP1 T1.4: NULLS LAST is bytewise (bare `0xFF` marker, above every non-null tag); multi-column keys are per-column encodings concatenated, prefix scans are ranges over the concatenation with uppers constructed so NULL continuations stay in range. Per-type encodings for numeric/temporal/etc. remain gated on invariant 7 (order-preserving normalization) before those types join an index |
| Sequences/identity on sharded tables | §14, §17 | Sharding design |
| Index MVCC costs (delete-word churn, deferred index writes) | §13 | WP8 (API unaffected; `UniqueViolation`/D12 already value-shaped) |

## 8. Migration path in this repo

1. **T6.4 (heap AM)** lands the first honest subset: `AnyTable::Heap`, `TableHandle`, `capabilities`, `insert` / `fetch` / `seq_scan` (`next` only; `rescan`/`mark`/`restore` as reserved stubs), `WriteOutcome::Done`/`NotVisible`, `TamError` (poisoning wired as stub), `RowRef` without TOAST, visibility pinned to `RowVisibility::Live`-only (M6 scope guard: no MVCC yet). The speculative API is *reserved* (compile-checked signatures, `UniqueViolation`-only behavior) so MVCC work later extends without breaking the executor surface.
2. **WP2 landing** swaps the stub for the real status-table-backed `row_visible`, engine-side waits, poisoning, and real speculative arbitration — values the debug-era wire suites never produced, so existing tests pass unchanged until MVCC behavior is deliberately pinned.
3. **WP6** adds `AnyTable::Iot` implementing the same surface with `RowLocator::Pk`; the executor gains the IOT arm without code changes (D1's payoff). Merge-join `mark`/`restore` arrives with IOT ordered leaf walks.
4. **WP7/WP8** add TOAST behind `RowRef` and index scans; **WP9** fills the SSI hook; **WP10** takes over DDL coordination.

## 9. Decision log

| Decision | Status |
| :---- | :---- |
| D1/D7 enum dispatch, no `dyn`/generics/`async-trait` | Decided (upstream sketch amendment required) |
| D2 opaque statement-scoped locators; moves only via outcomes | Decided |
| D3 outcomes as values; engine-side bounded waits; `WaitPolicy` pinned | Decided (confirms upstream Proposed) |
| D4 single visibility rule; `row_visible` composition pinned | Decided |
| D5 engine-maintained secondary indexes | Decided (confirms upstream Proposed) |
| D6 read surface; RC re-check recipe; `lock_row` returns the row | Decided |
| D8 scope ownership + cross-scope read routing | Decided 2026-11-14 — **message-to-owner (Option A)**; HLD §8/§10 amendment required |
| D9 errors = infrastructure only; poisoned txns | Decided |
| D10 statement boundary (`StmtCtx`, serial, schema version) | Decided |
| D11 scan protocol: locators in items, rescan, mark/restore, drop semantics, capabilities, SSI hook reserved | Decided |
| D12 upsert via speculative-insert primitives | Decided |
| D13 key contract co-owned with WP1 | Decided; encoding implemented at WP1 T1.4 (`KeyBytes` tag scheme in cascade-pgtypes) |
| D14 DDL = physical phase; schema versions from `StmtCtx` | Decided |
| D15 `RowRef` owns buffer + engine handle for lazy TOAST | Decided |

## 10. Review provenance

Folded from the 2026-11-14 adversarial review of HLD v2 §10 read as this API's detailed design (3 high / 8 medium / 6 low) plus the prior WP5 draft. Finding → decision mapping: H1→D11 · H2→D8 · H3→D12 · M1→D3 · M2→D6 · M3→D10 · M4→D9 · M5→D1+D7 · M6→D4 · M7→D13 · M8→D14 · L1→D3 · L2→D6 · L3→D11 · L4→D11 · L5→D14 · L6→D15.
