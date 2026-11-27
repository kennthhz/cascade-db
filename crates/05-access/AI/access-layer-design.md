# Cascade DB Access Layer: Contracts, Engines and Migration

**Status:** Draft v0.2 for review, 2026-11-26, amended 2026-11-27 for the round-4 review's findings W1–W9 ([`access-design-review-round4-2026-11-27.md`](access-design-review-round4-2026-11-27.md)) and for the round-3 review's High and Medium findings ([`access-design-review-2026-11-27.md`](access-design-review-2026-11-27.md); each is marked there with its fix), and for Kenneth's decisions: A1–A18 ratified (register D-6), the enum `Table` over a trait (D-5 item 1), and access-api C1–C13 (D-10) ( see the [access-api design](../cascade-access-api/AI/access-api-design.md) and the [decision register](decision-register.md)). v0.2 applies the review of v0.1 ([`access-design-review.md`](access-design-review.md), 30 findings): the contract types and the visibility rule are now stated in this document instead of being cited from retired ones, the migration steps are renamed and re-sequenced, and the design errors the review found are corrected (§16 lists them). This document is the single source of truth for the access layer (L5): the table access contract, the table engines, secondary indexes, sequences, temporary tables and maintenance. The detailed engine designs live beside their crates: the [btree design](../cascade-btree/AI/btree-design.md), the [heap design](../cascade-heap/AI/heap-design.md) and the [page design](../cascade-page/AI/storage-page-mutation-design.md). The engine semantics they refine are kept in [`engine-semantics.md`](engine-semantics.md).

**Upstream:** [`architecture-design.md`](../../../docs/architecture-design.md) v0.10.2 — §4.5 (transactions), §4.6 (access), §4.8 (core sets), §1 (design rules). Where the two disagree, the architecture wins unless layer §13.2 says this document proposes a change. **Storage foundations:** [`storage-layer-design.md`](../../02-storage/AI/storage-layer-design.md) v0.5 §12, whose rules bind every engine. **Formats:** in code, not in documents — `cascade-types` (version words), `cascade-page` (page layouts), `cascade-wal-format` (records), `cascade-pgtypes` (keys).

**Code baseline:** `main` at `54b30c4`. Storage migration steps M0–M6 are landed; M7 (`cascade-redo` and the page store) is written in the working tree and not yet committed; M8 (the page cache onto `PageSource`) follows it. Runtime tasks T6.0–T6.2 have landed (page format, cross-core buffer pool). The heap access method (plan task T6.4) has not, and this document is its contract.

---

## 1. Purpose and Scope

The access layer is the API between the executor and tables. It speaks tables and rows: scans, point fetches, insert, update, delete, upsert, row locking, index builds and maintenance. Everything above it decides *policy* — the isolation matrix, conflict arbitration, foreign-key semantics, batching — and everything below decides *physics*: pages, version chains, undo, the log, indexes.

It has two faces, as every surveyed system does:

- **The planner face** — capabilities and statistics only. The planner never calls data functions.
- **The executor face** — the row operations, each on a core of the scope's write core set (layer §8).

Three engines implement one contract under one visibility rule:

- **IOT** (index-organized) — the default. Rows are the leaves of a B-tree ordered by primary key; the newest version is in the leaf, older ones are undo deltas (kept §1). It is built from `cascade-iot` over `cascade-btree-core`.
- **Heap** — the compatibility engine. Every update appends a new row version; HOT chains, prune and vacuum work as in PostgreSQL (kept §2). It is `cascade-heap` over the heap page format.
- **Temporary tables** — session-private, never logged, built on the same contract (layer §11.6).

Engines differ only below the contract (storage §12.1, principle 5): they share the log, the page cache, the transaction manager, snapshots and the page store.

### 1.1 Goals

- **One contract, several engines, one visibility rule.** The executor gains an engine without code changes.
- **Every component testable in isolation.** The contract ships a conformance suite and an in-memory fake with no pages and no log (architecture Rule 6); every engine passes the same suite.
- **No page-level escape hatch.** Every write goes through a mini-transaction (layer §9.1) inside the engine. No engine API exposes page bytes to the layer above.
- **Identical pages everywhere.** Every page change is a redo record; the leader, a follower and the page store produce the same bytes (storage §12.4).

### 1.2 Non-goals for version 1

- Pluggable third-party engines. The contract's enum has three arms (layer §6.1), and a fourth reopens that decision.
- Hash, GIN, GiST, BRIN and vector index kinds. `cascade-index` owns the seam they will land behind (layer §11.4); version 1 ships B-tree secondary indexes only.
- Cross-scope transactions and sharded tables. Version 1 has one scope per database (architecture D10).
- Bulk load beyond the reserved record type (type 8 in `cascade-wal-format`).
- Columnar storage. `cascade-columnar` uses the same page size but is its own layer, reached through the log.
- Concurrent index builds. A build blocks writes on its table for its duration (layer §11.4).

---

## 2. How to Read This Document

### 2.1 Reading order

New to the layer: §3 (glossary), then §7 (one write, followed end to end), then §6 (the contract). Implementing an engine: §6, §9 and §10, then that engine's design. Checking a decision: §4 for the list, §15 for its status, §13 for what is still open.

### 2.2 Citation convention

A section number is always written with its document. The prefixes the layer, B-tree, heap and kept documents use are:

| Prefix | Document |
| :--- | :--- |
| layer § | this document |
| btree § | [`cascade-btree/AI/btree-design.md`](../cascade-btree/AI/btree-design.md) |
| heap § | [`cascade-heap/AI/heap-design.md`](../cascade-heap/AI/heap-design.md) |
| page § | [`cascade-page/AI/storage-page-mutation-design.md`](../cascade-page/AI/storage-page-mutation-design.md) |
| kept § | [`engine-semantics.md`](engine-semantics.md) |
| storage § | [`crates/02-storage/AI/storage-layer-design.md`](../../02-storage/AI/storage-layer-design.md) |
| architecture § | [`docs/architecture-design.md`](../../../docs/architecture-design.md) |
| plan FR-n, plan Tn.m | [`runtime-implementation-plan.md`](../../09-runtime/cascade-runtime/AI/runtime-implementation-plan.md) |

### 2.3 Where each cited source lives

| Source | What it is | Where |
| :--- | :--- | :--- |
| Architecture | The target architecture. Wins over this document. | `docs/architecture-design.md` |
| Storage design | Storage contracts and the storage foundations of §12, which bind every engine | `crates/02-storage/AI/storage-layer-design.md` |
| Page design | The page layer and the heap page format (its decisions D1–D6 and L1–L3) | `crates/05-access/cascade-page/AI/` |
| Kept document | The IOT, heap and index semantics of the retired storage HLD v2 | `crates/05-access/AI/engine-semantics.md` |
| Plan | The runtime's tasks (T0.x–T6.x) and requirements (FR-n) | `crates/09-runtime/cascade-runtime/AI/runtime-implementation-plan.md` |
| ADR-0003 | The cross-core buffer pool | `AI/decisions/` |
| Code | Version words (`cascade-types`), page layouts (`cascade-page`), records (`cascade-wal-format`), keys (`cascade-pgtypes`), clock and status table (`cascade-runtime`, module `txnmgr`) | under `crates/` |

The storage HLD v2 and its work packages WP1, WP2 and WP5 were retired from the tree on 2026-11-22 and exist only in git history (`git show 381ee5b^:crates/02-storage/cascade-storage/AI/<name>.md`). **This document no longer depends on them.** What it needs from WP5 is restated in layer §6, what it needs from the HLD is in the kept document, and what it needs from WP1 and WP2 is in the code.

### 2.4 Identifier families

Numbering restarts in every family, so an identifier is never written without its family.

| Family | What it numbers | Defined in |
| :--- | :--- | :--- |
| A1, A2, … | This document's decisions | layer §4, layer §15 |
| B1, B2, … | B-tree design decisions | btree §12 |
| H1, H2, … | Heap design decisions | heap §13 |
| page D1–D6, page L1–L3 | The page design's heap-format decisions and layer decisions | page design §3, §4 |
| FR-n, Tn.m | The plan's requirements and tasks | the plan |
| M0–M9 | The storage migration's steps | storage §9 |
| Step 1–9 | This layer's migration steps | layer §12 |
| S1–S21 | The storage design's numbered decisions | storage §2 |
| architecture D1–D11 | The architecture's open decisions | architecture §10 |
| Rule 1–11 | The architecture's design rules | architecture §1 |
| 5, 10–15, 20–29, 40 | WAL record type bytes | `cascade-wal-format`; heap §9, btree §8 |
| F1–F4 | The four format decisions of the retired WP1 design, all implemented: F1 the version word (`cascade-types`), F2 the heap row header (`cascade-page`), F3 the record contract (`cascade-wal-format`), F4 the key encoding (`cascade-pgtypes`). Written "format F2" | the code |

---

## 3. Glossary

Plain words for the terms the access documents use. The formal definition is where the third column says.

| Term | Meaning | Defined in |
| :--- | :--- | :--- |
| Engine | One implementation of the contract: IOT, heap or temporary table | layer §1 |
| Scope | The unit of single-writer ownership: one log, one transaction-id space, one clock. In version 1, one database | storage §12.2 |
| Core set, write core set | The CPU cores a database's (or a table's) work may run on; the write core set is the union of those that may write the scope | architecture §4.8 |
| Page | A data page: an 8 KiB block of a table or an index — a heap page, an IOT leaf, an index leaf, a B-tree branch, a free-space-map page. Everything stored is pages. The log is not pages; it is a stream of records | `cascade-page`; layer §9.1 |
| Segment | A numbered run of pages inside a scope: one table, one index, one free-space map | layer §10 |
| Version word | 64 bits in every row header and index entry. Zero is empty. The top bit set names an in-flight transaction. Otherwise the value is a commit timestamp | `cascade-types` |
| Commit timestamp, read timestamp | Times from the scope's hybrid logical clock. A commit is ordered by its commit timestamp; a snapshot reads at its read timestamp | storage §12.3 |
| Snapshot | A read timestamp plus the reader's own transaction | layer §6.6 |
| Closed timestamp | The scope's promise that no later commit will carry an earlier timestamp. A snapshot's read timestamp is chosen at or below it (layer §6.7). Not yet in code (register D-2) | layer §6.7 |
| Status table | Per scope: transaction id → in progress (with its top-level id), committed at a timestamp, or aborted. The only way to resolve a word that names an in-flight transaction. Not yet in code: the runtime's `txnmgr` is only a clock today (register D-2) | `cascade-txn-api` contract; layer §13.1 |
| Cleanup watermark | The oldest read timestamp any active reader may still use. Versions that died before it can be removed | layer §11.7 |
| Locator | The executor's handle on a row version: a primary key (IOT) or a Tid (heap). Opaque to the executor | layer §6.5 |
| Tid | A heap row address: page and slot | layer §6.2 |
| Chain | A heap row's versions, linked old → new. A HOT chain is one that never leaves its page | heap §3 |
| Prune, vacuum | Prune: quick cleanup of dead versions on one page, triggered by access. Vacuum: background reclamation of slots and index space | heap §7 |
| Redo record, replay | A redo record is a small piece of data that names one data page and describes one change to it. Applying it runs the redo function on that page's image. Replay is applying records to data pages in log order | storage §12.4; layer §9.1 |
| Mini-transaction | The smallest unit in which a page changes: one logical page-level change, such as "insert this row" or "split this leaf", together with the redo records that describe it. Logged and applied as a whole. Not a transaction: one commits nothing and makes nothing durable | layer §9.1 |
| Group | The redo records of one mini-transaction: one contiguous LSN range, ending on the group-end flag | layer §9.1 |
| Seal | To end a mini-transaction: mark its last record with the group-end flag and hand the group to the log writer | layer §9.1 |
| Group-end flag | The mark on the last record of a group. The log never becomes durable inside a group | storage §2 (S16) |
| Page LSN | The end of the last redo record applied to a page | layer §9.3 |
| Fill cursor | Where the sequential-fill rule places the next insert in a segment | heap §2 |
| Speculative insert | An insert whose uniqueness is still being decided | layer §6.9 |
| Deadlock victim | The transaction the lock manager aborts to break a wait cycle | layer §6.11 |
| Epoch | A scope's ownership number. It rises on every handoff, and a transaction still working under the old one is fenced | storage §12.2 |
| Seam | A narrow interface that an engine must go through to reach something it does not own, so that what sits behind it can change without the engine changing. Three are used: the mini-transaction seam (the page cache's mini-transaction module), the visibility seam (`VisibilitySource`) and the wait seam (`WaitSource`) | layer §9.1, §6.5, §6.6 |
| Pin | A hold on a page's frame in the buffer pool: shared, to read a copy; exclusive, to change it. Never held across an `await` | `crates/03-memory/cascade-bpm/AI/CONTEXT.md` |
| Poisoned transaction | One that has met a storage failure or a lost epoch. Every later call returns `Poisoned` | layer §6.11 |
| Undo | An IOT row's earlier versions, as deltas chained from the leaf row | kept §1 |
| Out-of-line storage | A value too large for a row, kept as chunks of a hidden relation | heap §8 |
| Tree key | What a B-tree orders its entries by: the key, or the key followed by the locator | btree §2.3 |
| Run | The entries of one tree that share a tree key | btree §2.3 |
| IOT | Index-organized table: the rows live in the leaves of a B-tree ordered by the primary key, so the primary key is not a separate index. The default engine | layer §11.3 |
| HOT | "Heap-only tuple", PostgreSQL's term. An update is HOT when it changes no indexed column and the new version fits on the same page; it then adds no index entries, and the new version is reachable only through the chain | heap §4.1 |
| LSN, durable LSN | Log sequence number: a position in the log. The durable LSN is the highest one known to be on stable storage | storage §2 |
| HLC | Hybrid logical clock: a timestamp that tracks wall-clock time but never goes backwards and orders causally related events | storage §12.3 |
| Subtransaction id, statement horizon | A transaction id that a statement or savepoint writes under, recorded in the status table with its top-level transaction. The horizon is the next id at a statement's start; own words at or above it are invisible to that statement | layer §6.6 |
| TOAST, out-of-line value | PostgreSQL's name ("the oversized-attribute storage technique") for storing a large value in chunks outside the row. Here "out-of-line storage" | heap §8 |
| `OpCtx` | The per-call context the runtime passes to every async storage call: the core, the cancellation token and the deadline | `cascade-runtime` |
| Verdict | A prune or vacuum decision ("these slots are dead"), logged so that every replica applies the same one rather than deciding again | heap §7 |
| Stub | A crate that exists in the workspace but whose code is a placeholder from a replaced design | layer §5.2 |
| Replay boundary | A log position at which a reader of the log may stop and still see whole mini-transactions: always the end of a group | storage §2 |
| Physiological record | A redo record that names one physical page and describes a logical change within it ("insert this row in slot 7"), rather than the bytes changed | storage §12.4 |
| SSI | Serializable snapshot isolation, PostgreSQL's serializable level: it tracks read-write dependencies and aborts a transaction that would make a cycle | layer §6.4 |
| EvalPlanQual | PostgreSQL's re-check of a row that was updated concurrently under read committed. The re-check of layer §6.8 and heap §5.2.1 is its counterpart | layer §6.8 |

## 4. Summary of Decisions

Each decision carries one status: **Proposed** (the author's proposal, awaiting the project owner), **Accepted** (by whom, when), **Superseded** (by what), or **Restates the architecture** (which section). Layer §15 has the full log.

| # | Decision | Section | Status |
| :--- | :--- | :--- | :--- |
| A1 | The mini-transaction — one logical page change together with its redo records, applied as a whole (§9.1) — is the only write primitive. An engine never changes a page any other way | §9.1 | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.4 |
| A2 | Redo has one dispatch point, `cascade-redo`. The byte transforms live at the format authorities, the payload encodings in `cascade-wal-format` | §9.2 | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A3 | B-tree page formats live in `cascade-page`; `cascade-btree-core` owns tree logic, not bytes | §9.2 | **Accepted** (Kenneth, 2026-11-27, register D-6). Follows architecture §3.2 |
| A4 | A scope is a shared-memory engine across its write core set. This supersedes the retired WP5 D8 (message-to-owner reads) | §8 | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.8 |
| A5 | Engines are built from a `TableDescriptor` and never read the catalog | §6.12 | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.6 |
| A6 | Segment and page allocation is a scope-level facility. Ids are never reused | §10 | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A7 | The key contract stays in `cascade-pgtypes` until `cascade-datum` exists. The dependency is a documented exception. Text indexes support the `C` collation only until then | §5.2 | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A8 | Visibility needs one injected contract, `VisibilitySource`, so engines do not depend on the runtime | §6.6 | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A9 | `cascade-btree-core` is the tree mechanism for the IOT and for secondary indexes, and knows nothing about transactions | §11.1 | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.6 |
| A10 | The engine builds a mini-transaction's records, and the seam applies each record to the data page it names — the heap, IOT or index page in the buffer pool — through `cascade-redo`. Leader and replay therefore run one code path. A page's LSN is the end of its last record | §9.1, §9.3 | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A11 | Row locks: on a heap, exclusive locks are recorded in the row header and shared locks in the lock manager; on an IOT, all in the lock manager | §6.10 | **Accepted** (Kenneth, 2026-11-27, register D-6). Changes architecture §4.5 (see §13.2) |
| A12 | An engine that must wait sleeps in an injected wait seam (`WaitSource`) until woken, instead of polling. The wait layer's two aborts — deadlock victim, `lock_timeout` — arrive as errors | §6.5, §6.11 | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A13 | Callers see an enum (`Table`, three arms: IOT, heap, temporary); the conformance suite is written once over a test-only trait that the engines and the fake implement, and `Table` delegates to it, so the suite covers the whole surface | §6.1, §6.13 | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A14 | Each writing statement and each savepoint gets a subtransaction id; a snapshot carries the statement horizon, so a statement never sees its own changes, and savepoint rollback marks sub-ids aborted | §6.6 | **Accepted** (Kenneth, 2026-11-27, register D-11); changes architecture §4.5 |
| A15 | The unique check reads the latest committed state, not the statement snapshot | §11.4 | **Accepted** (Kenneth, 2026-11-27, register D-6). Proposed in round 3 |
| A16 | Each index change is its own mini-transaction, after the row's, in the same statement and transaction | §11.4 | **Accepted** (Kenneth, 2026-11-27, register D-6). Proposed in round 3 |
| A17 | A failure after a write's row step is an error that blocks commit until a savepoint rollback or an abort | §6.5 | **Accepted** (Kenneth, 2026-11-27, register D-14) |
| A18 | Earlier own locks survive a savepoint rollback; lock hints follow a pin protocol | §6.10 | **Accepted** (Kenneth, 2026-11-27, register D-6). Proposed in round 4 |

The decisions of the retired WP5 design (its D1–D15) are part of the contract in layer §6 and are no longer cited by number. Layer §16 maps each one to where it now lives. One of them is **superseded**: WP5 D8, message-to-owner reads, by A4.

---

## 5. Crates and Dependencies

### 5.1 The target crate set

"The page cache contract" below means the crate that owns the frames and the mini-transaction: `cascade-bpm` today, `cascade-pagecache` in the architecture.

| Crate | Status | Contains | Depends on |
| :--- | :--- | :--- | :--- |
| `cascade-txn-api` | To create (L4) | The transaction seams engines consume: `Snapshot`, `TxnStatus`, `VisibilitySource`, the visibility rule (layer §6.6), `WaitSource` (§6.5), `TxnCtx` and `StmtCtx` (§6.7), and a scripted fake of all of them. Started early, with only what the access contract needs (access-api C1) | `cascade-types` |
| `cascade-access-api` | To create | The contract of layer §6: the types, the suite's trait, the in-memory fake and the conformance suite | `cascade-types`, `cascade-txn-api`, `cascade-pgtypes` (A7), `cascade-telemetry` |
| `cascade-access-dispatch` | To create | The dispatch enums `Table`, `TableScan` and `Maintenance` (layer §6.1). It sits above the engines, because the enums name their types | `cascade-access-api` and every engine crate |
| `cascade-page` | Landed | The page layer: prologue, kind registry and the heap format (kind 1); the B-tree and free-space-map formats as they land (A3) | `cascade-types` |
| `cascade-btree-core` | Stub | The B+tree mechanism: descent, latching, structure changes, operations on byte keys | `cascade-types`, `cascade-page`, the page cache contract |
| `cascade-heap` | Stub | The heap engine | `cascade-types`, `cascade-page`, the page cache contract, `cascade-access-api`, `cascade-index` once it exists |
| `cascade-iot` | To create | The IOT engine | `cascade-access-api`, `cascade-btree-core`, `cascade-undo`, `cascade-index`, the page cache contract |
| `cascade-index` | To create | Secondary indexes for both engines | `cascade-access-api`, `cascade-btree-core` |
| `cascade-sequence` | To create | Sequences, `serial` and identity columns | `cascade-access-api` |
| `cascade-maintenance` | To create | The scheduler for undo purge, heap prune and vacuum | `cascade-access-api`, `cascade-watermark` once it exists |
| `cascade-temptable` | To create | Session-private temporary tables on the same contract | `cascade-access-api`, `cascade-tempstore` |

Engines reach pages only through the page cache contract and never through `cascade-storage-api`. Layer §12 gives the order in which the crates are created.

### 5.2 Layering facts this layer must state

- **`cascade-page` is in this directory but is an L1 format authority.** The architecture's layer map puts every page layout there. The repository directory is historical: the page design §1 keeps the page format below both the engine and the storage-side consumers of its bytes.
- **The key contract edge (A7).** `KeyBytes`, `KeyRange` and `IndexEntry::Locator` live in `crates/07-sql/cascade-pgtypes`. Their long-term home is `cascade-datum` (L1), which does not exist yet. Until then `cascade-access-api` depends on `cascade-pgtypes`, an upward edge (05 → 07) that breaks the repository's layering convention. It is a named exception in the layering check and it ends when the key contract moves. No access-layer type may name any other `07-sql` item. `Datum`, `Row`, `Projection` and `ColumnSet` (layer §6.2) move with it.
- **Key order and collations (round-3 fix V24).** Today's text key encoding (`encode_text`) is byte-wise, which matches PostgreSQL only for the `C` collation. Version 1 indexes on text columns support the `C` collation only, and `CREATE INDEX` on a text column with any other collation is refused with `feature_not_supported` until `cascade-datum` provides collation-aware keys (architecture §4.2). This is a prerequisite of Step 8 for non-`C` collations, and is listed in the decision register.
- **The visibility seam (A8).** Engines name `Snapshot` and `VisibilitySource`, never the runtime. Both are declared in `cascade-txn-api` (L4), as architecture §4.5 places them (access-api C1, decided 2026-11-27). No implementation exists yet: the runtime's `txnmgr` is only a clock today (decision register D-2). `cascade-scope-txn` will implement them, with no engine change.
- **The mini-transaction seam.** Engines name one injected type (layer §9.1). Its owner is the page cache, which is the only crate allowed to change a page.
- **`cascade-mvcc` and `cascade-undo` are pre-pivot stubs.** Their context files describe a design that was replaced. `cascade-heap`'s dependency on them is removed at Step 5. `cascade-undo` returns as a dependency of `cascade-iot` only: the heap has no undo (page §2).

---

## 6. The Contract: `cascade-access-api`

This section is the whole contract. Every type the executor sees is defined here or in layer §6.2.

### 6.1 Dispatch — an enum, not `dyn` or generics

```rust
// In cascade-access-dispatch, above the engine crates (access-api C1).
pub enum Table { Iot(IotTable), Heap(HeapTable), Temp(TempTable) }
```

`Table` re-exposes the whole surface as inherent `async fn`s that `match self` and delegate. The futures are `!Send`, as the runtime requires. A trait object would need boxed futures and would erase that; generics do not fit because the executor holds mixed collections of tables. Temporary tables are a third engine on the same contract, so the enum has three arms (A13). The temporary arm arrives with `cascade-temptable` (Step 9); every `match` on the enum lives in this repository, so adding it is a compile-time change and not a contract break.

**Names (decided by Kenneth, 2026-11-27).** The generic tree crate is `cascade-btree-core` (formerly `cascade-btree`), so that its name says it is the shared mechanism under the IOT and every index, not an index itself. The dispatch crate is `cascade-access-dispatch`, because all it does is route each call to the engine the catalog's table type names. The enum is `Table`, formerly `AnyTable`: callers hold "a table", and the arm says which engine stores it. `TableHandle` was not used because it already names the `{ rel, scope }` pair that `handle()` returns.

**Where the enum lives (access-api C1).** Not in `cascade-access-api`: the enum names `IotTable`, `HeapTable` and `TempTable`, and every engine crate depends on the contract crate, so the enum there would be a dependency cycle. It lives in `cascade-access-dispatch`, a thin crate above all the engines.

**Why there is also a trait (A13).** The rule behind it is one sentence: **write the tests once, and make every engine pass the same tests.** If each engine had its own hand-written tests, bugs would hide in the gaps between them. Running one test suite against four different engines needs a common plug — in Rust, a trait. But the official API must not be a trait, for the reasons above. So there are two doors with different jobs: the **trait is the private plug the test suite uses**, and the **enum is the public front door, whose methods only forward** to the plug. Since the front door only forwards, whatever a user can ask for goes to the thing the tests tested — there is no side door for an untested path to hide in. The trait is never in a production signature, so the rejection of traits above is about the public API and does not change.

### 6.2 Substrate types

```rust
// Owned by crates below this one:
//   cascade-types:  DatabaseId, ScopeId, Epoch, TxnId, Hlc, VersionWord, Lsn,
//                    PageId, PageNo, SegmentId, OpCtx, AlignedBuf
//   cascade-pgtypes: KeyBytes, KeyRange, Locator (Pk | Heap)          — see A7
//   cascade-page:    NextWord and its flag constants

// Declared here:
pub struct SchemaVersion(pub u64);   // the schema version a statement leased from the catalog
pub struct RelId(pub u64);           // a relation's stable id, assigned by the catalog
pub struct IndexId(pub u64);         // an index's stable id, assigned by the catalog
pub struct ColId(pub u16);
pub enum StorageKind { Iot, Heap }
pub struct Tid { pub page: PageId, pub slot: u16 }   // a heap row address
pub enum Datum { Null, Int64(i64), Bool(bool), Text(String) }   // grows with the type layer
pub struct Row(pub Vec<Datum>);              // one value per column, in column order
pub struct Projection(pub Vec<ColId>);       // the columns a read returns
pub struct ColumnSet(pub Vec<ColId>);        // the columns an update changed
```

On disk and in index entries a Tid is packed relation-locally (`Locator::Heap { page_no, slot }`); in memory it carries the full `PageId`. Engines convert at the boundary; nothing else does.

### 6.3 The surface

```rust
impl Table {
    // planner face
    pub fn handle(&self) -> TableHandle;            // TableHandle { rel: RelId, scope: ScopeId }
    pub fn capabilities(&self) -> &AmCapabilities;

    // writes. The calling core is in the scope's write core set (§8).
    // The engine maintains secondary indexes inside every write.
    pub async fn insert(&self, txn: &mut TxnCtx, row: &Row) -> Result<WriteOutcome, TamError>;
    pub async fn update(&self, txn: &mut TxnCtx, loc: &RowLocator, new: &Row,
                        changed: &ColumnSet) -> Result<WriteOutcome, TamError>;
    pub async fn delete(&self, txn: &mut TxnCtx, loc: &RowLocator) -> Result<WriteOutcome, TamError>;
    pub async fn lock_row(&self, txn: &mut TxnCtx, loc: &RowLocator,
                          mode: RowLockMode, wait: WaitPolicy) -> Result<Locked, TamError>;

    // upsert by speculative insertion (§6.9)
    pub async fn insert_speculative(&self, txn: &mut TxnCtx, row: &Row, index: IndexId)
        -> Result<SpeculativeResult, TamError>;
    pub async fn complete_speculative(&self, txn: &mut TxnCtx, token: SpecToken)
        -> Result<RowLocator, TamError>;
    pub async fn abort_speculative(&self, txn: &mut TxnCtx, token: SpecToken) -> Result<(), TamError>;

    // reads (§6.8)
    pub async fn fetch(&self, txn: &TxnCtx, loc: &RowLocator,
                       proj: &Projection) -> Result<Option<RowRef>, TamError>;
    pub async fn fetch_latest_committed(&self, txn: &mut TxnCtx, loc: &RowLocator)
        -> Result<Option<(RowLocator, RowRef)>, TamError>;
    pub fn seq_scan(&self, txn: &TxnCtx, proj: &Projection, opts: SeqScanOpts) -> TableScan;
    pub fn index_scan(&self, txn: &TxnCtx, index: IndexId, range: KeyRange,
                      proj: &Projection, opts: IndexScanOpts) -> TableScan;

    // DDL: the physical phase only (§6.12). Statistics.
    pub async fn build_index(&self, txn: &mut TxnCtx, def: &IndexDef) -> Result<(), TamError>;
    pub async fn truncate(&self, txn: &mut TxnCtx) -> Result<(), TamError>;
    pub fn maintenance(&self) -> Maintenance;
    pub fn stats(&self) -> TableStats;
}

pub enum Maintenance { Iot(IotUndoPurge), Heap(HeapPruneVacuum) }
impl Maintenance {
    /// One bounded unit of background work on the calling core.
    pub async fn step(&mut self, budget: Budget) -> Progress;
}
```

Reads take the transaction context, not a bare snapshot: `fetch` uses the current statement's snapshot, and `fetch_latest_committed` takes a fresh one (heap §5.2). Every operation also takes the caller's `OpCtx` (tenant, span, deadline — architecture Rule 8); it is omitted above for readability only.

### 6.4 Contract types

```rust
/// The executor's handle on a row version. Opaque: store it, pass it back, never inspect it.
pub enum RowLocator { Pk(KeyBytes), Tid(Tid) }

pub enum WriteOutcome {
    Done { locator: RowLocator },                  // the locator may differ from the input
    Conflict(Conflict),
    UniqueViolation { index: IndexId, key: KeyBytes },
    NotVisible,                                    // the row is gone for this snapshot
}

pub enum Conflict {
    /// Another transaction has the row and has not ended.
    InProgress { holder: TxnId },
    /// Another transaction committed a change after our snapshot. `latest` is for
    /// fetch_latest_committed, not for fetch.
    CommittedAfterSnapshot { commit_ts: Hlc, latest: Option<RowLocator> },
}

/// PostgreSQL's four row-lock modes.
pub enum RowLockMode { KeyShare, Share, NoKeyUpdate, Update }
pub enum WaitPolicy { Wait, SkipLocked, DontWait }
pub struct Locked { pub outcome: WriteOutcome, pub row: Option<RowRef> }  // row present iff acquired

pub enum SpeculativeResult {
    Installed(SpecToken),                         // must be completed or aborted before the statement ends
    KeyBusy(Conflict),
    UniqueViolation { index: IndexId, key: KeyBytes },
}
pub struct SpecToken { /* engine-private */ }

pub struct ScanRow { pub row: RowRef, pub loc: RowLocator }
pub enum TableScan { Iot(IotScan), Heap(HeapScan), Temp(TempScan) }
impl TableScan {
    pub async fn next(&mut self) -> Option<Result<ScanRow, TamError>>;
    /// Restarts with new parameters at the same snapshot (nested-loop join inner).
    pub async fn rescan(&mut self, params: &ScanParams) -> Result<(), TamError>;
    pub fn supports_mark_restore(&self) -> bool;   // merge join; ordered scans only
    pub fn mark(&mut self);
    pub async fn restore(&mut self) -> Result<(), TamError>;
    pub async fn close(self);
}
pub enum Direction { Forward, Backward }
pub struct SeqScanOpts { pub dir: Direction, pub resume: Option<ResumeHint>,
                         pub ssi: Option<SsiReporter>, pub page_budget: u32 }
pub struct IndexScanOpts { pub dir: Direction, pub index_only: bool,
                           pub ssi: Option<SsiReporter>, pub page_budget: u32 }

/// Capabilities for the planner. Correctness never depends on them.
pub struct AmCapabilities { pub ordered_scans: bool, pub mark_restore: bool,
                            pub index_only: bool, pub count_star_fast: bool }
pub struct TableStats { pub row_estimate: f64, pub dead_fraction: f32, pub free_pages: u32 }
pub struct IndexDef { pub id: IndexId, pub key_columns: Vec<ColId>, pub unique: bool }
```

`ScanParams`, `ResumeHint`, `SsiReporter`, `Budget` and `Progress` are implementation-shaped and not listed: `ScanParams` carries the rescan bindings, `ResumeHint` an engine-private scan position, `SsiReporter` is reserved for the serializable-isolation design, and `Budget`/`Progress` bound one maintenance step. `RowRef` owns a decoded copy of the visible columns, never a reference into a page; a toasted column is fetched on demand (heap §8).

---

### 6.5 Locators, outcomes and waits

**Locators.** `RowLocator` is opaque to the executor. A locator is valid for the rest of the statement that produced it, in the same transaction. When a write moves the row — an IOT primary-key update, a heap update that is not HOT — the new locator is reported in `WriteOutcome::Done`. `StmtCtx.serial` lets engines stamp locators in debug builds as a staleness check.

**Outcomes are values, until the write has changed something.** Conflicts and unique violations come back in `WriteOutcome` when the engine meets them *before* it has changed any page for this call: the row's conflict check, a lock, the primary key of an IOT, or a speculative insert's arbitrating index. `Err` is reserved for layer §6.11.

**After the row step, a failure is an error (round-4 fix W2).** A heap write changes the row first and then each index in its own mini-transaction (layer §11.4), and an IOT write changes the leaf row first and then its secondary indexes. If an index step after that could return a value, the executor could skip the row and commit a half-done write: an old version stamped deleted whose index entries never got delete words (which breaks heap H6 and lets vacuum free a slot a live entry still names), or a new version missing from some indexes. So:

- Index steps after the row step ignore `WaitPolicy` and always wait, with the bound, as PostgreSQL waits on a unique conflict even under `NOWAIT` and `SKIP LOCKED`, which apply to row locks only.
- A unique violation or an expired wait in such a step is `TamError::StatementFailed { cause }`, not a value. The engine also marks the transaction context *failed*: until the executor rolls back to an enclosing savepoint (which aborts the statement's subtransaction id, and with it the partial write) or aborts the transaction, every call returns `StatementFailed`, and `COMMIT` is refused. This is PostgreSQL's behavior: an error inside a statement aborts the transaction, or the subtransaction a savepoint opened.
- The conformance suite (S7) checks that an update whose second index conflicts leaves no visible trace after the rollback, and that the transaction cannot commit before it. Reads report a row that is not visible as `None`; writes report it as `WriteOutcome::NotVisible`.

**Waiting.** When a write or a lock meets a row that another transaction holds — its write still in flight, or its lock conflicting — the engine must sometimes wait for that transaction to end. `WaitPolicy` says whether it may, and the executor chooses it per statement:

| `WaitPolicy` | Behavior |
| :--- | :--- |
| `Wait` | The engine waits through the wait seam, with the bound the transaction layer configures. Then it proceeds, or returns `CommittedAfterSnapshot { commit_ts, latest }`, or `NotVisible` if the row is gone. When the bound expires it returns `Conflict::InProgress`. |
| `DontWait` | It returns `Conflict::InProgress { holder }` at once. The executor raises PostgreSQL's `lock_not_available`, since it asked not to wait. |
| `SkipLocked` | It skips the row: `NotVisible` from a write, no row from a scan. |

**Why waiting has its own seam (A12).** Waiting takes two different abilities. *Asking for the current state* — has that transaction ended? — is a poll, and `VisibilitySource` (layer §6.6) answers it. *Sleeping until something changes* cannot be a poll: an engine that re-checks in a loop burns its core and slows every session sharing it. So waking up is a second injected contract, `WaitSource`, implemented by the lock manager — the one component that sees every waiter, and therefore the only one that can also see a deadlock cycle:

```rust
pub enum WaitFor {
    /// Resolves when the transaction commits or aborts.
    Txn(TxnId),
    /// Resolves when the last shared locker of the row releases it (layer §6.10).
    SharedLocks(LockKey),
    /// Resolves when the engine signals the key: a speculative insertion was aborted and
    /// its delete word stamped (layer §6.9, round-3 fix V13). Also ends when the holder ends.
    Key(LockKey),
}
/// What one wait ended with.
pub enum Waited { Resolved, TimedOut }

// Declared in cascade-txn-api (access-api C1). Because L4 cannot name an L5 type, a lock
// target is an opaque LockKey built from the relation and the locator's or key's bytes
// (access-api C8). WaitSource also has `signal(key)`, which wakes WaitFor::Key waiters.
pub trait WaitSource: Send + Sync {
    fn wait<'a>(&'a self, ctx: &'a OpCtx<'_>, waiter: TxnId, what: WaitFor,
                deadline: Option<Instant>) -> LocalBoxFuture<'a, Waited>;
}
```

**What sleeping means here.** Notification, not spinning. A waiter suspends and is woken when the thing it waits for happens — the transaction ends, or the row's last shared locker releases. On this runtime that is the executor's own suspension: the wait returns `Pending` with a waker registered, the reactor runs other sessions, and the commit or abort that ends the wait triggers the waker, which re-queues the waiter. A waiter is a task, not a thread, so thousands of them cost memory, not cores. Spinning would be worse than wasted CPU on a thread-per-core runtime: a waiter that spins occupies the very core that may have to run the holder's commit before anyone can be woken. **Spin for latches; sleep for transactions.**

**Where a woken task goes.** Back onto its own core's run queue. A task never migrates: it is spawned on one core, and every wake re-enqueues it there — which is what makes `!Send` session state legal. Waking runs nothing; the waker only enqueues, and the reactor polls the task again when it reaches the front of the queue. On a cross-core wake the notification travels by way of the target reactor first, which then does the same local enqueue. When the task is polled, its future resumes from its own state — a suspension saves no stack; the future's state machine is the saved progress — and re-checks the world, because the holder may have committed or a third transaction may have taken the row meanwhile. That is why the wait loop begins the operation again. The short end of waiting is different: a refused buffer-pool pin (`Contended`) is retried after a yield, because pins are held for microseconds and a cross-core wake-up costs more than the hold.

**The engine's wait loop.** Under `Wait`, an engine repeats three steps: resolve the holder through `VisibilitySource`; if it is still in the way, sleep on it through `WaitSource::wait`, with the bound the transaction layer configures; when woken — or when the wait returns `TimedOut` — begin the operation again, because the row or its holder may have changed while the engine slept. A `TimedOut` expiry is the value `Conflict::InProgress`: the executor still decides what happens to this row — re-check it under read committed, skip it, or surface the wait to the client.

**Which waits end in an error.** Two aborts are decided *for* the transaction rather than by it, and they come back as `TamError` (layer §6.11): the lock manager may pick this transaction as the deadlock victim, and `lock_timeout` — a user setting the statement layer enforces — may interrupt a wait that has outlived it. Both abort the transaction, as in PostgreSQL. The rule of thumb for the whole contract: **a value when the executor can still decide what happens to this row; an error when the transaction's fate has been decided for it.** `DontWait`'s refusal is a value because the executor asked for it; a deadlock is an error because the victim did not choose it.

`lock_row` is the operation that returns the row together with the outcome, so `SELECT … FOR UPDATE` costs one lookup (layer §6.10).

### 6.6 The visibility rule

One rule decides every row version and every index entry. Engines and index code call it; nothing else interprets a version word.

```rust
// Declared in cascade-txn-api (access-api C1).
pub struct Snapshot { pub read_ts: Hlc, pub own: Option<OwnView> }

/// The reader's own transaction, as one statement sees it (round-3 fixes V1, V2).
pub struct OwnView {
    /// The top-level transaction. Every word this transaction writes names one of its
    /// subtransaction ids, whose top is this id.
    pub top: TxnId,
    /// The statement horizon: the next transaction id at the moment the statement began.
    /// Own words with an id at or above it were written by this statement, or by a later
    /// one, and are not visible to it.
    pub horizon: TxnId,
}

pub enum TxnStatus {
    /// `top` is the top-level transaction the id belongs to (itself, for a top-level id).
    InProgress { top: TxnId },
    Committed { ts: Hlc },
    /// A rolled-back transaction, or a subtransaction rolled back to a savepoint.
    Aborted,
}

/// Resolution of a word that names an in-flight transaction. Injected (A8).
pub trait VisibilitySource {
    fn status(&self, xid: TxnId) -> TxnStatus;
}

/// Word-level result.
pub enum Visibility {
    Visible,
    CommittedAfterSnapshot { commit_ts: Hlc },
    InProgress { holder: TxnId },
    Aborted,
}

/// Row-level result. `own` is set when the reader's own transaction did the delete.
pub enum RowVisibility { Live, NotBornYet, Deleted { own: bool } }

impl Snapshot {
    pub fn visible(&self, word: VersionWord, src: &dyn VisibilitySource) -> Visibility;
    pub fn row_visible(&self, creator: VersionWord, deleter: Option<VersionWord>,
                       src: &dyn VisibilitySource) -> RowVisibility;
}
```

**Statements and savepoints (round-3 fixes V1 and V2).** A transaction does not stamp every word with one id. Each statement that writes gets a **subtransaction id** the first time it writes, from the scope's ordinary transaction-id counter, and stamps that id. The status table records each subtransaction's top-level transaction. This one mechanism gives two PostgreSQL behaviours:

- *A statement does not see its own changes.* `UPDATE t SET v = v + 1` must not meet, later in the same scan, the new versions it just wrote, or it would update them again (the "Halloween problem"). PostgreSQL uses a command id in each row for this. Here, ids grow monotonically, so a statement's own changes are exactly the own words whose id is at or above the statement's **horizon**, the next id at the moment the statement began. Changes made by earlier statements are below it and visible; a nested statement (a trigger or a function) takes a new horizon, so it sees the changes the outer statement made before it ran.
- *`ROLLBACK TO SAVEPOINT` works on every engine.* Rolling back to a savepoint marks every subtransaction id allocated since that savepoint `Aborted` in the status table, with a logged record. Their words then read exactly as an aborted transaction's do, on the heap with no undo at all. The IOT additionally restores its leaf rows from undo (kept §1). Locks taken under a rolled-back subtransaction disappear with it: a header lock word names an aborted id, and the lock manager drops entries it lists for that id.

The first record each subtransaction writes carries its top-level id, so followers and recovery rebuild the mapping from the log.

**How a subtransaction's status is resolved (round-4 fix W6).** A subtransaction's entry in the status table holds only its top-level id and a "rolled back" flag. `status(sub)` is `Aborted` if the flag is set, and otherwise whatever `status(top)` is. A commit therefore changes exactly one entry, the top's, and every subtransaction of it that was not rolled back becomes committed at the same instant, with the same timestamp. Flipping sub-id entries one by one would let a reader see part of a transaction.

**Publication comes before the closed timestamp (round-4 fix W6).** A snapshot must classify each word the same way for its whole life. So the status table marks a commit `Committed { ts }` before the closed timestamp may pass `ts`: the closed timestamp advances to `t` only once every commit at or below `t` is both durable and published. Without this order, a reader could see a word as in progress (not born) and later, within the same snapshot, as committed at a timestamp at or below its read timestamp. This is an invariant of `cascade-txn-api`, checked by the scripted world in suite groups S1 and S2.

**The rule, as a truth table.** A version is *born* for a snapshot if its creator is visible at `read_ts`; it is *deleted* if its deleter is. "Own" below means a word whose id belongs to the reader's top-level transaction (`status(x)` is `InProgress { top }` with `top == own.top`).

| Creator word | Status | Born? |
| :--- | :--- | :--- |
| zero | — | Never valid. The engine refuses the page. |
| uncommitted(x), own | aborted (rolled back to a savepoint) | no (`NotBornYet`) |
| uncommitted(x), own | x ≥ `own.horizon`: written by this statement or a later one | no (`NotBornYet`) |
| uncommitted(x), own | x < `own.horizon` | yes |
| uncommitted(x), another transaction | in progress or aborted | no (`NotBornYet`) |
| uncommitted(x), another transaction | committed at ts | ts ≤ `read_ts` |
| committed(ts) | — | ts ≤ `read_ts` |

| Deleter word (only once born) | Status | Result |
| :--- | :--- | :--- |
| zero | — | `Live` |
| uncommitted(x), own | aborted, or x ≥ `own.horizon` | `Live`: the delete was rolled back, or this statement made it and must not see it |
| uncommitted(x), own | x < `own.horizon` | `Deleted { own: true }` |
| uncommitted(x), another transaction | in progress or aborted | `Live` — the delete may still roll back, or did |
| uncommitted(x), another transaction | committed at ts | ts ≤ `read_ts` → `Deleted { own: false }`, else `Live` |
| committed(ts) | — | ts ≤ `read_ts` → `Deleted { own: false }`, else `Live` |

`visible(word)` is the same rule for a single word: the empty word gives `Aborted`; an own word gives `Visible` below the horizon and `Aborted` if rolled back; another transaction's gives `InProgress`, `Aborted`, `Visible` or `CommittedAfterSnapshot`. The conflict rules of layer §6.5 and §11.4 treat every own word as "not another transaction": a transaction never waits on itself.

An index entry uses the same composition with its own two words: `row_visible(create, Some(delete))`.

**What each engine does with the result.** The three outcomes do not mean the same move in every engine, because heap chains run old → new and IOT undo runs new → old:

| | `Live` | `Deleted` | `NotBornYet` |
| :--- | :--- | :--- | :--- |
| Heap, resolving a Tid (heap §5.2) | Return that version | Hop to the newer version if the chain continues; otherwise the row is gone | Gone: every later version is younger still, and older ones cannot be reached |
| IOT, resolving a key (kept §1) | Return the leaf row | Walk undo to older versions | Walk undo to older versions |
| Index entry | The entry is usable | Skip the entry | Skip the entry |

### 6.7 Statement boundary and transaction context

```rust
pub struct TxnCtx { /* scope and epoch, the writer's TxnId once assigned, the statement context */ }

impl TxnCtx {
    /// Read committed: a new snapshot. Repeatable read and serializable: the transaction's.
    /// Async: a new snapshot waits until its read timestamp is closed (access-api C5).
    pub async fn begin_statement(&mut self) -> Result<&mut StmtCtx, Fenced>;
    /// Releases statement-scoped state and aborts speculative tokens left unresolved.
    /// Returns the speculative tokens left unresolved; the executor aborts them (access-api C6).
    pub fn end_statement(&mut self) -> Vec<(RelId, SpecId)>;
    /// A new snapshot, waited for until its read timestamp is closed. For fetch_latest_committed.
    pub async fn fresh_snapshot(&mut self) -> Result<Snapshot, Fenced>;
}

pub struct StmtCtx {
    pub snapshot: Snapshot,
    pub serial: u64,                   // locator staleness check in debug builds
    pub schema_version: SchemaVersion, // checked at write time (layer §6.12)
}
```

`TxnCtx` is a concrete struct in `cascade-txn-api`, built by the transaction layer from four injected services: a clock with closed-timestamp waits, the `VisibilitySource`, the `WaitSource` and the scope's epoch (access-api C5). Engines consume it and never construct it. Its poison flag is interior (a `Cell`; the context is `!Send`), so a read that holds only `&TxnCtx`, or a scan that outlives the call that created it, can still poison the transaction (round-3 fix V25). `end_statement` returns each unresolved token as a pair of relation id and token id, so `cascade-access-dispatch` can route the abort to its table (V26).

**How a read timestamp is chosen (round-3 fix V20).** A snapshot is closed when no commit can still appear below its read timestamp. Taking `read_ts = now` and waiting for the closed timestamp to pass it would make every read-committed statement wait for every concurrent commit to become durable, about the commit latency. So a snapshot takes `read_ts = max(closed_ts, session_token)`, where the session token is the commit timestamp of the session's own last commit (architecture §4.5, read-your-writes). It waits only when the token is ahead of the closed timestamp, which happens just after the session's own commit. For this to keep read-committed semantics across sessions, a commit is acknowledged to its client only once the closed timestamp has reached its commit timestamp, so any commit a client has heard about is visible to every snapshot taken afterwards. `fresh_snapshot` follows the same rule. The context carries the scope's epoch, and every engine call checks it: a call for an epoch the scope no longer holds returns `TamError::Fenced` before touching a page. That is a runtime outcome, not an assertion, because an epoch changes under a running transaction by design — a failover, or a scope reopened after a log failure.

### 6.8 Reads and the scan protocol

- **`fetch(txn, loc, proj)`** returns the row as the current statement's snapshot sees it, resolved from `loc`, or `None` if that snapshot does not see a row there.
- **`fetch_latest_committed(txn, loc)`** is the re-check after a conflict. It takes a fresh snapshot, so its answer includes transactions that committed after the statement's snapshot. It returns the newest version that snapshot sees, with that version's own locator. Read committed uses it after `CommittedAfterSnapshot`; under repeatable read the executor raises a serialization failure instead. Heap §5.2.1 gives the procedure. Reads take the transaction context rather than a bare snapshot — a change from the retired WP5 sketch — because the statement's snapshot lives in the context and only the re-check needs another one.
- **Scans** are pull iterators. Items carry locators (`ScanRow`), so an `UPDATE` or `DELETE` over a scan needs no second lookup. Constructing a scan does no I/O; the first `next()` starts it. The engine filters by visibility, so the executor only ever receives visible rows. `rescan` restarts at the same snapshot. `mark`/`restore` exist for merge join, on ordered scans only. Dropping a scan without `close` releases what it holds. No page pin is ever held across an `await`.
- **Sequential and index scans are separate entry points.** An IOT's sequential scan walks primary-key order; a heap's walks pages in order. Index-only scans work on both engines because entries carry their own visibility, so there is no visibility map.

---

### 6.9 Upsert by speculative insertion

Composing an upsert above the contract (look up the key, then insert or update) is not atomic: two transactions that both find the key absent both insert. PostgreSQL resolves this with speculative insertion, and so does this contract, in three calls:

1. **`insert_speculative(txn, row, index)`** inserts the row and lets the unique index named by `index` arbitrate. On a heap table the row is inserted like any other and the index entry's create word is the writer's uncommitted word; on an IOT the leaf row is itself the entry. The result is:
   - `Installed(token)` — no conflicting entry. The row is now visible to the writer only.
   - `KeyBusy(Conflict::InProgress { holder })` — another transaction holds the key and has not ended. Under `Wait` the engine has already waited.
   - `KeyBusy(Conflict::CommittedAfterSnapshot { .. })` — a live committed entry that the statement's snapshot cannot see. The executor's update branch works from `fetch_latest_committed`; a `DO NOTHING` statement simply skips the row.
   - `UniqueViolation` — a live committed entry the snapshot does see. (For a plain insert, both of the last two cases are a unique violation: see the unique check in layer §11.4, which reads the latest committed state, not the snapshot.)
2. **`complete_speculative(txn, token)`** changes nothing in storage. The insert becomes an ordinary one.
3. **`abort_speculative(txn, token)`** **does change bytes.** The transaction goes on running (`ON CONFLICT DO UPDATE`, `DO NOTHING`), so the row must stop existing at once. On a heap, the engine stamps the row's deleter and the entry's delete word with the writer's uncommitted word. On an IOT, where the row is itself the primary-key entry and has no deleter word, it sets the leaf row's delete-mark and writes the undo record that carries the deleting word, in one group with undo first (round-3 fix V15). From then on the row is `Deleted { own: true }` for the writer, and for every other transaction it is invisible whatever the writer's outcome, because the same transaction created and deleted it.

Two consequences for the conflict protocol. An entry whose create and delete words name the same in-flight transaction is not a conflict. Waiters for a key are woken when its delete word lands, not only when a transaction ends: a waiter sleeps on `WaitFor::Key`, and the engine signals that key when it stamps the delete word (round-3 fix V13). A token that the statement leaves unresolved is returned by `end_statement`, and the executor aborts it through `Table::abort_unresolved` (access-api C6); `TxnCtx` cannot call an engine without depending on L5. A debug assertion catches a list that is dropped.

### 6.10 Row locks (A11)

PostgreSQL has four row-lock modes with this compatibility:

| Requested mode | Conflicts with a holder in |
| :--- | :--- |
| `KeyShare` | `Update` (and a delete, which is key-changing) |
| `Share` | `NoKeyUpdate`, `Update` |
| `NoKeyUpdate` | `Share`, `NoKeyUpdate`, `Update` |
| `Update` | all four |

Foreign-key checks hold `KeyShare` on the referenced row for the transaction's duration, so all four are needed. Plain reads never take or honour row locks.

**Exclusive locks live in the row header; shared locks live in the lock manager.**

- **`FOR UPDATE`, `FOR NO KEY UPDATE`, and every `UPDATE` and `DELETE`** stamp the row's deleter word with the writer's uncommitted word and set two flag bits. `XMAX_LOCK_ONLY` says the word names a locker, not a deleter, so readers ignore it. `XMAX_UPDATES_KEY` says the holder deletes the row, changes a key column, or holds `FOR UPDATE`, as opposed to `NO KEY UPDATE`. The heap takes the bits, and the `HAS_MANAGER_LOCKS` hint below, from the unassigned part of its flag word (format F2 assigns three). The page layer assigns them (register D-9b).
- **`FOR SHARE` and `FOR KEY SHARE`** take an entry in the lock manager, keyed by the relation and the locator of the locked version, listing holders and modes. The entry disappears when the transaction ends. The only page change is the `HAS_MANAGER_LOCKS` hint bit in the row's flag word, set by a logged one-bit record the first time the version gets a lock-manager lock, under the protocol below.
- **The IOT keeps every row lock in the lock manager**, keyed by the relation and the primary key. Its leaf row has no deleter word to stamp (layer §11.3), and an IOT write already conflicts through the creator word, which names the in-flight updater — the same implicit-lock rule as InnoDB's writer id. So an IOT locker takes its lock in the lock manager and also sets a `HAS_LOCKS` bit in the leaf row's flag word (a logged change, while kind 3 is still unfrozen). An IOT writer consults the lock manager only when that bit is set, so writes to rows that nobody has locked never touch the shared lock table; the bit is cleared lazily by a writer that finds no holder, under the pin protocol below (round-3 fix V16, round-4 fix W8). The lock manager's table is sharded by key hash, so writers on different cores rarely meet in it. It needs no key-change flag: a key update moves the row to a new key, so a surviving live row at the old key *is* the statement that the update changed no key.
- **Row locks are in-memory and transaction-scoped.** They are never logged and never recovered: the manager starts empty when a scope opens, which is sound because every holder either committed (locks released at commit) or is rolled back by recovery. The same is true of PostgreSQL's stamped lock-only words and of InnoDB's and SQL Server's lock systems. Deadlock detection runs here too, over the locks it holds.
- **Who consults what.** Readers consult nothing: with `XMAX_LOCK_ONLY` set, the engine passes no deleter to the visibility rule, so the row stays live whatever became of the locker. Writers and exclusive lockers consult the header word — an in-progress holder whose mode conflicts starts the conflict protocol (layer §6.5); a holder that has ended means the lock is gone — and they consult the lock manager for shared holders of the version they target, honouring the table above. To keep that lookup off the common path, a writer looks in the lock manager only when the row's `HAS_MANAGER_LOCKS` hint is set (protocol below). Shared lockers consult the header word and the lock manager.

**Why this split.** Several transactions may hold a shared lock on one row at once. The row header has one deleter word, which can name one transaction, so it cannot list them; PostgreSQL adds a separate multi-locker id space for that case. The lock manager already has that list. Putting *every* row lock there would instead make every row write probe the lock table, which the header word avoids for the modes that writes themselves need.

**A savepoint rollback must not release an earlier lock (round-4 fix W3).** Transaction A takes `FOR UPDATE` in one statement (the deleter word names sub-id a1, lock only), sets a savepoint, and updates the row in a later statement, which overwrites the word with a2. Rolling back to the savepoint aborts a2, and without more care the row would look unlocked although A still holds its earlier lock. PostgreSQL had this bug before 9.3. So: **when a write overwrites a lock word that names an earlier subtransaction of the same top-level transaction, it first records that lock in the lock manager under the earlier sub-id** and sets the row's `HAS_MANAGER_LOCKS` hint (below), in the same mini-transaction as the overwrite. After a rollback to the savepoint the header word names an aborted id, but writers find the hint, consult the lock manager, and meet A's lock. The lock manager entry ends with A, as every row lock does.

**The hint protocol (round-4 fix W8).** Writers skip the lock manager unless a row's hint bit says it may hold locks: `HAS_MANAGER_LOCKS` on a heap row (the round-3 `HAS_SHARED_LOCKS`, renamed because W3 also puts exclusive locks there) and `HAS_LOCKS` on an IOT leaf row. A hint is only safe if setting it, checking it and clearing it are ordered against each other, so all three happen **under the exclusive pin of the row's page**:

1. *A locker* pins the page exclusively, checks the header word for a conflicting writer, registers in the lock manager (a non-blocking attempt; if it must wait, it unpins, waits, and starts again), sets the hint if it is clear in the same mini-transaction, and only then unpins.
2. *A writer* pins the page exclusively, checks the header word, and, if the hint is set, consults the lock manager for conflicting holders, all before it stamps and while it holds the pin.
3. *Clearing* (IOT only; the heap hint is never cleared for a version) is done only by a writer that holds the pin and has just found no holder. Because a locker registers only while holding the same pin, no lock can be registered between that check and the clear.

Lock release at transaction end touches no page; a stale set hint costs one lock-manager lookup. The protocol is model-checked with `loom` (architecture Rule 11).

**Locks follow the row.** No update leaves a lock behind on the old version. After any update, by the lock owner or by another transaction, every lock the old version carried is recorded again on the new one (heap §4.4). A shared lock survives an update because a non-key update does not conflict with it — which is exactly why a later key-changing update on the new version must still find it.

**Prior art.** Each ingredient is in production use. The lock-only stamp is PostgreSQL's (`HEAP_XMAX_LOCK_ONLY` — this design borrows the name), and PostgreSQL likewise carries locks over to a row's new version when an update moves it. The two-places rule — a write's lock read from the row, a `FOR` lock looked up in a lock system — is InnoDB's, which calls the row's writer id an *implicit* lock and keeps every explicit `FOR` lock in its lock system; InnoDB's secondary-index records, which carry no writer id, get manager locks, as every IOT lock does here. SQL Server and DB2 keep all row locks in the lock manager, with nothing in the row.

Two choices are not the mainstream default. Shared locks go to the lock manager rather than PostgreSQL's MultiXact id space — deliberately, because MultiXact is a large subsystem with its own wraparound emergencies (its ids are 32-bit, like PostgreSQL's transaction ids, so a busy database must periodically rewrite old rows before the counter wraps, or stop accepting writes), and these shared locks are transaction-scoped like every other row lock, so the manager loses nothing that outlives a transaction. And one bit records whether the in-flight writer changes key columns; PostgreSQL does not persist that for a write in progress and cannot always tell a key-changing update from a non-key one until it has waited the writer out, so a key-share holder here can be answered from the page instead.

A11 is a proposal that changes the architecture's sentence "row locks themselves live in version words" (layer §13.2). It needs the two flag bits from the page layer and the shared-lock lists from `cascade-lock`.

### 6.11 Errors

```rust
#[non_exhaustive]
pub enum TamError {
    /// A failure below the seam, reported as text: engines do no I/O of their own.
    /// The transaction's outcome is unknown.
    Io(String),
    /// The scope's epoch changed under this transaction. The outcome is unknown.
    Fenced { scope: ScopeId },
    /// A write under a schema version the catalog has retired.
    SchemaStale { min_leased: SchemaVersion },
    /// The lock manager chose this transaction to break a wait cycle (PostgreSQL 40P01).
    Deadlock,
    /// A lock wait exceeded `lock_timeout` (PostgreSQL 55P03).
    LockTimeout,
    /// This transaction already failed with Io or Fenced.
    Poisoned,
    /// A unique violation or an expired wait after the row step of a write (layer §6.5,
    /// round-4 fix W2). The statement's partial write is undone only by rolling back to a
    /// savepoint or aborting; until then every call returns this error.
    StatementFailed { cause: FailCause },
}
```

An `Err` follows one rule (layer §6.5): **a value when the executor can still decide what happens to this row; an error when the transaction's fate has been decided for it.** A failure below the seam, a lost epoch, a retired schema, and the two wait aborts — chosen as the deadlock victim, or interrupted because `lock_timeout` has passed — all decide the transaction's fate, so they are errors. `Deadlock` and `LockTimeout` end the statement, and the executor aborts the transaction, as PostgreSQL does (40P01 and 55P03). A plain statement timeout never enters the engine, so it is not in this type. A `DontWait` refusal and the expiry of an engine's own wait bound are values (layer §6.5): the executor asked for the first, and can still handle the second.

**Poisoning.** `Io` and `Fenced` poison the transaction: every later operation returns `Poisoned` without touching a page. The executor aborts, and the durable log decides what became of the transaction — a transaction that had not reached `COMMIT` is aborted, and one that was waiting on `COMMIT` may or may not have committed. This is the access layer's part of the log-failure sequence in storage §8.1.

### 6.12 Table descriptors and construction

```rust
pub struct TableDescriptor {
    pub rel: RelId,
    pub kind: StorageKind,
    pub scope: ScopeId,
    pub columns: Vec<ColumnDef>,   // ColId, type, codec, nullability
    pub key: KeyShape,             // see below
    pub indexes: Vec<IndexDef>,
    pub toast: ToastPolicy,        // heap §8
}

/// Everything an engine is given besides its descriptor: the mini-transaction seam,
/// the page cache contract, VisibilitySource, WaitSource and the scope's ScopePages.
pub struct TableEnv { /* .. */ }
```

`key` names the primary-key columns when the table has them. For an IOT they are the organizing key. For a heap table they are covered by a unique secondary index. A table without a primary key has none, and uses the heap engine.

An engine is constructed from a descriptor and a `TableEnv`. Engines never read the catalog: the scope host builds descriptors from it, and the catalog's own tables use built-in bootstrap descriptors (architecture §4.6). A schema change arrives as a new descriptor at the next statement boundary; a write under a retired schema version returns `TamError::SchemaStale`. `build_index` and `truncate` execute only the physical phase of a DDL statement, and the catalog coordinates the rest above this contract. Two constraints on them:

- **A build blocks writes on its table for its duration** (a table lock held by the statement). Without it, an entry inserted for a row that dies before the build commits would never receive a delete word (heap §7.3).
- **`truncate`** allocates a new segment at once and logs the old segment's drop only at commit, as part of the commit (round-3 fix V9). The page store discards a segment's files when it applies the drop (layer §10), so dropping earlier would destroy data that a rollback still needs. The descriptor's new segment takes effect at commit. A rolled-back `truncate` logs only the new segment's allocation, which is harmless: the unused segment is dropped with the abort. `DROP TABLE` and `DROP INDEX` follow the same rule.

### 6.13 The in-memory fake and the conformance suite

This section is A13 applied. The rule: **write the tests once; every engine takes the same test.** The tests plug into a private trait (§6.1); four implementations plug in beside them — the fake always, the three engines as they land. What each of the four is:

- **A fake engine**: a multi-version table in memory with no pages and no log. Versions are `(creator, deleter, payload)` triples resolved through the same visibility rule, and conflicts come from a scripted `VisibilitySource`. It implements the trait like a real engine, in two locator flavours: `Heap`, where a locator names one version and every update moves it, and `Iot`, where the locator is the primary key and moves only when the key changes (access-api C10). The suite checks locator movement against both, and each engine is compared with its flavour. It is the executor's test double, and its machinery is the base of the temporary-table engine (§11.6) — which is why the temporary arm is cheap to add.
- **A conformance suite**: one test per promise of §6, written as generic functions over the test-only trait, and run against every implementation of it — the fake and, as they land, the three engines. Because `Table` delegates to the trait (§6.1), passing the suite is not a parallel obligation the engines can drift away from: it is the same code the executor reaches.

The suite covers: every outcome under each `WaitPolicy`; the read-committed re-check; locator movement; the scan protocol (rescan, mark/restore, drop); speculative arbitration, including two transactions racing on one key; poisoning and fencing; and the visibility truth table against a scripted status source.

---

## 7. A Worked Write

One row inserted into a heap table, followed from the executor to the page store. The other three documents refer back to this. Steps 3 to 5 are one mini-transaction (layer §9.1).

1. **The executor** calls `insert(txn, row)`.
2. **The engine gates the row.** A cell larger than the largest one a page holds — 8158 bytes, 8132 of payload — is not logged. It is a candidate for out-of-line storage (heap §8) and, until that exists, is refused (heap §9).
3. **The engine builds the cell** through the page format's own encoder, with the creator word `uncommitted(txn.xid)`.
4. **The engine chooses the page.** While the insert record names only its segment (type 5), it takes the relation's fill mutex, reserves the group's LSN through the seam, asks the fill cursor for the page, and releases the mutex. That order makes the leader's layout the same as replay's (heap §2). Once the record names its page and slot (type 10), the engine picks a page with free space instead.
5. **The engine pins the target frame exclusively**, yielding and retrying if another core holds it, and hands the record to the seam. The seam applies it to the pinned frame through `cascade-redo` and stamps the page's LSN (layer §9). The engine releases the pin. No pin is held across an `await`.
6. **Secondary indexes**, once they exist, receive their entries in the same statement. The primary-key check returns a conflict as a value if another transaction already holds the key.
7. **The engine returns `Done { locator }`**, the new row's Tid.
8. **Other sessions** resolve `uncommitted(txn.xid)` through the status table, find it in progress, and do not see the row. The writer's own statements do.
9. **`COMMIT`** is the transaction layer's: it appends the commit record (taking the scope's commit-section mutex, a proposed change to the architecture, layer §13.2), waits for durability, and marks the status table `Committed { ts }`. Every snapshot with `read_ts ≥ ts` now sees the row. Nothing on the page changed.
10. **The page store** reads the record from the log and applies the same redo function, so it writes the same bytes.
11. **`ROLLBACK`** leaves the page alone. The status table says `Aborted`, the row is dead, and prune reclaims it.

This is the order once visibility exists. In the first slice (Step 3) the runtime plan reserves and applies the record in the commit path instead, so that a rolled-back insert never reaches a page — nothing would hide it yet.

---

## 8. Execution Model (A4)

**A scope is a shared-memory engine across its write core set** (architecture §4.8). Any core of that set may issue any call, read or write. `cascade-dataplane` routes whole plan fragments to cores in the fragment's core set and proxies a read-write session whole to the leader node. Inside this layer there is no core ownership, no message-to-owner routing and no per-scan channel.

What follows from that:

1. **Shared structures.** Everything the write core set touches is `Send + Sync`: the log writer, the status table, the lock manager's queues, the page cache, and each engine's per-scope state (descriptors, free-space map, page hints, sequence caches). Those on the commit path are model-checked with `loom` (architecture Rule 11).
2. **Where work serializes.** The architecture's budget — a commit uses at most one contended atomic and takes no lock — describes the commit itself in the uncontended case. It is not a statement about page access. In order, the serialization points are:
   - An uncontended commit's one shared write is the log's LSN reservation, one atomic (storage §12.3).
   - Every page touch takes the frame's pin word and then its mutex. Two cores on the same page serialize. A hot leaf — the rightmost one under sequential inserts — serializes all of its writers.
   - The commit path takes the scope's commit-section mutex, for one timestamp and one record append (runtime module `txnmgr`). The architecture says a commit takes no lock, so this is listed in layer §13.2 as a proposed change (round-3 fix V19). The mutex is held for a clock read and an LSN reservation, never for I/O; replacing it with a single atomic that hands out timestamp and LSN together is the way back to the architecture's budget, if measurements require it.
   - While the insert record is type 5, each relation has a fill mutex on the insert path (heap §2). It is deleted when the record names its page.
3. **Write conflicts are found in the row; explicit locks are found in the lock manager.** An engine finds a write-write conflict by reading the row's version words and records its own write by stamping them under the exclusive pin. Explicit row locks (`SELECT … FOR …`) that have no word to live in are kept in the lock manager (layer §6.10). On the heap a writer reaches the lock manager only when the row's header says shared locks may exist; on the IOT a writer checks one flag bit on the leaf row first, so a row nobody has locked costs no lock-manager lookup (round-3 fix V16).
4. **Two kinds of misplaced call.** A call from a core outside the write core set is a programming error and a debug assertion; the data plane's promises (architecture §4.8) rule it out. A call under an epoch the scope no longer holds is a runtime outcome, `TamError::Fenced` (layer §6.7).
5. **Cross-scope reads** take one read timestamp from `cascade-txn-coord`. Each fragment runs on its own scope's core set. The rules are the coordinator's.

*Supersession.* The retired WP5 design (its D8) chose message-to-owner reads on 2026-11-14, when the page cache was still one per core. Architecture v0.10.2 (§1.1, §4.8, D1) and ADR-0003 replaced that with the shared per-NUMA pool and made a scope a shared-memory engine; the pool landed at runtime task T6.2. The premise of D8 is gone. With it go the HLD v2 §10 sentence "all calls run on the core that owns the scope" and the page design's "runs serialized on the owning core".

---

## 9. The Write Path

### 9.1 The mini-transaction is the only write primitive (A1, A10)

**What a mini-transaction is.** It is the smallest unit in which a page changes: one logical page-level change, logged and applied as a whole. "Insert this row" is one. "Split this leaf" is one. "Mark the old version dead, link the chain and insert the new version" is one, although it touches two pages. Formally it has two halves that always travel together:

- a **group** of redo records — one contiguous LSN range in the scope's log, ending on the group-end flag — and
- the page changes those records describe, applied to the cached pages.

The two halves are one thing, not two: the group's records are exactly what gets applied to the pages. That is A10, below. The term is ARIES's (Mohan and Levine, 1992), where a mini-transaction is likewise a short, latch-held unit for which the log record suffices.

**Which pages.** A record is *appended to the log* and *applied to a data page*. The two never mix. A data page is an 8 KiB page of a table or an index: a heap page, an IOT leaf, an index leaf, a B-tree branch, a free-space-map page. On the leader that page is an image in the buffer pool; at the page store it is a block of a file. The log holds records, not data pages, and nothing is ever applied to it.

To apply a record is to run the redo function on one page image with that record as its input, which changes the image in place. A replayer runs the same function later and gets the same bytes. For example, `HEAP_INSERT_V2` names a segment, a page and a slot and carries a cell's bytes; applying it writes the cell into that heap page's free space, sets the slot's entry, updates the page's counts and stamps the page's LSN (heap §9).

**What a mini-transaction is not.** It is not a transaction, and it has nothing to do with `COMMIT`. A user transaction is a long sequence of mini-transactions, and its commit record is one more of them. One mini-transaction commits nothing, aborts nothing, and makes nothing durable.

**What one guarantees.**

| Property | Meaning | Why it holds |
| :--- | :--- | :--- |
| Atomic for every reader of the log | The durable LSN never falls inside a group, so a follower, the page store and recovery never see half of one. A B-tree split is never half done | storage S16: every append ends on a group end |
| The same bytes everywhere | The leader, a follower and the page store apply the same records with the same redo function | A10; layer §1.1 |
| Short, pins only | It holds page pins and nothing else: no row or table locks, no waits, and no `await` between its first pin and its last unpin | btree §3.3; heap §4 |
| Bounded | At most 1 MiB of records | storage §5.4, item 6 |

Being short and pin-only is what lets a failure discard one. If the process stops inside a mini-transaction, its group was never sealed, so none of it is in the durable log; and no page carrying its changes was written to storage, because a page may be written only once its LSN is at or below the durable LSN (storage §12.5). After the restart the change is simply not there. The same holds after a log failure, when the scope is reopened (storage §8.1).

**What becomes of one afterwards.** Its page changes stay on the pages whether the transaction later commits or aborts. Visibility is not the pages' business: an aborted transaction's changes are hidden by the status table, and its rows are reclaimed by cleanup (layer §6.6, layer §11.7).

**Who may make one.** Only the page cache's mini-transaction module can change a page. An engine reaches it through **the seam**, and a page change that bypasses the seam does not compile (architecture §4.4).

**What the seam is, and what it is not.** The seam is that module seen from an engine: a small interface with `reserve`, `apply` and `seal`. It is the only way an engine can change a page, so that the module behind it can be replaced without the engine changing. It changes page images and it talks to the log writer, but it is neither of the two components an engine must not touch:

```text
engine ── records ──▶ seam ── apply, via cascade-redo ──▶ the data page's image in the buffer pool
                        │
                        └── the sealed group ──▶ the log writer ──▶ the log store
```

- **It is not the log writer.** The log writer (`WalWriter` today, `cascade-log` in the architecture) owns LSNs, group commit and durability. The seam reserves an LSN range from it and hands it the sealed group. The seam never writes log bytes itself.
- **It is not a page writer.** The seam changes a page's image in the buffer pool and stops there. Compute never writes pages to disk; the page store writes them, by applying the same records. (Storage M8, landed 2026-11-27, removed the buffer pool's write-back: pages are a read cache, never written anywhere. The write-back was the buffer pool's, never the seam's.)

**How one is made.** Every page change in every engine is one mini-transaction, in three steps:

1. **Reserve** a contiguous LSN range for the group.
2. **Apply** each record, through `cascade-redo`, to the data page it names — an image in a buffer-pool frame that the engine holds under an exclusive pin — and stamp the page.
3. **Seal** the group with the group-end flag and hand it to the log writer.

Records are applied to the cached page before the group is sealed and reaches the log writer. So the page changes first, in memory; the write-ahead rule holds because of the next one: no page whose LSN is above the durable LSN ever leaves the cache, by eviction or by any write (storage §12.5; round-3 fix V18). Only a commit waits for durability. Because every append ends on a group-end flag, the durable LSN and every replay boundary fall on a mini-transaction edge (storage §2, S16).

**The engine supplies records; the seam applies them.** An engine never writes a page's bytes. It reads a data page to decide what to record, builds the record, and the seam applies that same record to that page's image through the one redo function that replay uses. So the leader, a follower and the page store run the same code on the same bytes, and there is no second code path to keep in agreement. The page cache makes its mutation methods invisible to engines, so a page change that skips the seam does not compile (architecture §4.4).

```rust
pub trait MiniTxnScope: Send + Sync {
    fn begin(&self) -> MiniTxn<'_>;
}

pub struct MiniTxn<'a> { /* the group: its LSN range and its records */ }

impl MiniTxn<'_> {
    /// Reserves a contiguous LSN range of `bytes` for the group. Synchronous, so that a
    /// caller which must order a decision against the log can (heap §2 does).
    pub fn reserve(&mut self, bytes: usize) -> Lsn;
    /// Adds `rec` to the group and applies it, through `cascade-redo`, to `frame`, which the
    /// caller holds under an exclusive pin. Stamps the frame with the record's end LSN.
    /// Never awaits. A failed apply ends the mini-transaction with an error and leaves the
    /// frame untouched.
    pub fn apply(&mut self, frame: &mut PageGuardMut, rec: RedoRecord) -> Result<(), TxnError>;
    /// Ends the group — the group-end flag on its last record — and hands it to the log writer.
    pub fn seal(self) -> SealedGroup;
}

pub struct SealedGroup { /* .. */ }
impl SealedGroup {
    /// Resolves when the group's last byte is durable. The caller holds no pin.
    pub async fn durable(self) -> Result<Lsn, TxnError>;
}
```

`PageGuardMut` is the page cache's exclusive guard (its name in `cascade-bpm` today). A group that changes several pages — a heap update that is not HOT, or a B-tree split — pins all of them first, applies its records in order, then releases. Order inside a group matters where one record depends on another: the IOT's group is `[undo record][leaf record]`, so the undo is in the log no later than the leaf change that needs it (kept §1).

**A group is all or nothing on the leader too.** An engine checks each change against the pinned copy before it builds the record, with the same function the apply uses, so `apply` does not fail on a checked frame. If it fails all the same, the image is corrupt: the scope is handled as after a log failure (storage §8.1), and the durable log decides.

The page cache's own design finalizes the shape. Engines rely on four things: records are applied by the redo function, `reserve` is synchronous, nothing awaits while a pin is held, and a group is never half applied. The in-memory fake keeps a log in a `Vec` and applies records to in-memory pages through `cascade-redo`.

### 9.2 Redo ownership (A2) and page formats (A3)

A redo record must be applicable to its page alone, give the same bytes every time, carry a version, and be safe to apply twice. Three crates share that work:

- **`cascade-wal-format`** turns each record into bytes and back. Each engine's design says what its records carry; the bytes live in this crate and are pinned by golden tests.
- **`cascade-redo`** is the single function, `apply_record`, that applies a record to a page. It keeps accepting every record version it has ever accepted.
- **The page-format crate owns the byte changes themselves.** Heap records are applied by calling `cascade-page`'s page methods; B-tree records by `cascade-page`'s B-tree page views (A3). One piece is still duplicated: `cascade-redo` has its own copy of the free-space arithmetic, which should be shared.

*Sources:* storage §12.4 (the redo contract); architecture §3.2 and Rule 5 (the three-way split); heap §9 and btree §8 (the records); page D4 (the page methods); layer §13.3 (the shared free-space item). Round-3 fix V35 moved these citations out of the sentences.

**A3 — B-tree page formats live in `cascade-page`.** The architecture lists the index-organized leaf layouts under `cascade-page`, and `cascade-redo` (L1) cannot depend on `cascade-btree-core` (L5). So the B-tree *bytes* — branch nodes, IOT leaf rows, index entries, their headers and their parse — are `cascade-page` kinds, and `cascade-btree-core` owns the tree *logic* over them. This corrects the page design's §2, which assigned those formats to `cascade-btree-core`; the architecture wins (architecture §1.1). The page design's boundary "cells are never unified across kinds" stands: an IOT leaf row and an index entry are different cell shapes that happen to live in one crate.

**Page-kind registry.** Kinds are assigned by the authority that lands them and frozen with golden tests (page L1). Assigned: **1 = heap**. Proposed, frozen when each lands: **2 = B-tree branch, 3 = IOT leaf, 4 = index leaf** (btree §2), **5 = tree metapage** (btree §2), **6 = free-space map** (heap §7.2), **7 = undo page** (the IOT design). An unknown kind is `WrongKind`, which is not corruption.

### 9.3 Page LSN

A page's LSN is the end of the last redo record applied to it (storage S17). In a group that touches several pages, each page receives the end of the group's last record *for that page*, not the group's end. The seam stamps exactly that on the leader, and `cascade-redo` stamps it on replay, so the two agree byte for byte.

Every rule reads it the same way: a page may be evicted when its LSN is at or below the durable LSN; a record applies only if its start is at or above the page's LSN; a page served as of an LSN has a page LSN at or below it.

---

## 10. Segments and Page Allocation (A6)

A page is `PageKey { segment, page_no }` inside a scope and `PageId { db, segment, page_no }` in the page cache. Engines convert once, at the seam. Allocation is a scope-level facility, injected with `TableEnv`:

```rust
pub trait ScopePages: Send + Sync {
    /// A new segment for a relation: a table, an index, the free-space map.
    /// Ids come from a counter that survives restarts and are never reused (storage S18).
    fn new_segment(&self, ctx: &OpCtx<'_>) -> SegmentId;
    /// The next page number a segment's tail may use.
    fn page_hint(&self, segment: SegmentId) -> PageNo;
    /// Gives a segment `count` more pages past its hint. Returns the first new page number.
    fn extend(&self, ctx: &OpCtx<'_>, segment: SegmentId, count: u32) -> Result<PageNo, TxnError>;
    /// How many pages the segment has been given. A sequential scan stops here.
    fn extent(&self, segment: SegmentId) -> PageNo;
}
```

Free space *inside* a heap's pages is the heap's own free-space map (heap §7.2); `ScopePages` knows only segment extents. A dropped or truncated relation retires its segment ids for good (storage S18): the log carries a segment-drop record, the page store discards the segment's files, and no later relation receives a retired id.

---

## 11. The Engines

**The mental model in brief.** `cascade-btree-core` is a generic B+tree: a sorted map from key bytes to opaque value bytes, with no knowledge of rows or transactions. `cascade-iot` uses one btree-core tree *as* the table: key = primary key, value = the whole row (creator word, undo pointer, flags, columns). `cascade-heap` keeps rows in heap pages addressed by (page, slot), with no tree. `cascade-index` is every index whose entries point at a row, each its own btree-core tree: key = indexed columns (plus the locator when not unique), value = the locator and two visibility words. On a heap table that includes the primary key's index; on an IOT table the primary key is the table itself. The [design overview](access-design-overview.md) §3 walks through an example.

### 11.1 `cascade-btree-core` — the tree mechanism (A9)

A B+tree over the page cache with byte keys and byte values, shared by the IOT's primary tree and by every secondary index. It knows nothing about transactions or visibility: version words inside leaf payloads are opaque bytes to it (architecture §4.6). It owns descent and latching, structure changes, ordered iteration with mark/restore, and the byte-level operations its two users need — insert, lookup, update in place, delete-mark, stamp, compact. The btree design is its detailed design.

### 11.2 `cascade-heap` — the compatibility engine

Row versions are appended to heap pages; every update writes a new version; a HOT chain never crosses a page; prune is plan-carried and vacuum is incremental, one page at a time. The page design owns the format (its decisions D1–D6). The heap design owns the engine: DML, visibility walks, free space, TOAST, vacuum, and the landing sequence.

### 11.3 `cascade-iot` — the default engine

The IOT has no detailed design yet. Kept §1 is its semantics; this section fixes what the rest of the layer needs from it, so that design cannot drift.

- **Operations on the tree.** Insert: a leaf record and an insert-undo record, in one group with undo first. Update: an undo delta, then the leaf row overwritten in place when it fits (btree §5.3), and otherwise a delete-marked old row beside a new one at the same key. Delete: the delete-mark and an undo record. Primary-key update: a delete-mark at the old key and an insert at the new one, so the locator moves. Read: the leaf row, and if the snapshot does not see it, the undo chain towards older versions until one is visible or the chain ends. Rollback: the transaction's undo chain backwards, a compensation record per step, then the abort record. Purge: delete-marked rows and their undo, at segment granularity, once no snapshot can need them.
- **A delete-mark is not self-contained.** The leaf row's `DELETE_MARK` is a flag without a timestamp. Whether a snapshot sees the row as deleted depends on when the deleting transaction committed, and only the undo record at `roll_ptr` carries that. It follows that every delete writes that record, and that purge keeps it for as long as the mark exists. The btree design treats the flag as opaque (btree §2).
- **The rest of the contract.** Its locator is the primary key. Its secondary indexes go through `cascade-index`; an update that changes no indexed column writes no entry, because the key did not change. Its purge is driven through `Maintenance::Iot`. Its leaf row header is a version word, a `roll_ptr` and a flag word, as a `cascade-page` kind (A3). Its row locks live in the lock manager (layer §6.10).
- **Open, and needed before the first undo page format is frozen:** the `roll_ptr` encoding, the undo record format, the undo segment layout, how recovery finds an in-flight transaction's undo, and how undo pages are cleaned up.

### 11.4 `cascade-index` — secondary indexes

**Entries.** `[key][locator][create word][delete word]`. The locator is the primary key for an IOT table and a Tid for a heap table. Both words are version words. An entry is visible when `row_visible(create, Some(delete))` says `Live` (layer §6.6), which is why index-only scans need no visibility map on either engine.

**Tree keys.** A unique index orders by key. A non-unique index orders by key followed by locator, so that entries are unique in the tree and a range over the key's prefix finds all of the key's entries (btree B8).

**The engines maintain indexes inside their writes**, in the same statement and the same transaction as the row, but each index change is its own mini-transaction (round-3 fix V6). A mini-transaction may not await (layer §9.1), and descending a B-tree awaits whenever a page is not resident, so one group cannot hold the row's page while it descends several trees. Order: the row's mini-transaction first, then each index's entries. What hides a half-done write from others is visibility, not log atomicity: the row and its entries all carry the same transaction's uncommitted word until it commits. After a crash in the middle, recovery aborts the transaction, so the row and any entries it got are dead together.

| Write | Index work |
| :--- | :--- |
| Insert a row | One entry per index, create word = the writer's |
| Update that changes an indexed column | Delete-word the old entry; insert a new one |
| Update that changes no indexed column | IOT: none, the key did not change — unless the primary key changed (next row). Heap: none if the update is HOT; otherwise new entries for the new version and delete words on the old ones (heap §4.2) |
| Primary-key update (IOT) | Every entry is rewritten — delete word on the old, a new one inserted — because every entry's locator is the primary key |
| Delete a row | Delete-word every entry of the row |

An entry never becomes visible before the row it points to: both are the same transaction's uncommitted work until that transaction commits.

**Unique check.** Uniqueness is a property of the current state, not of a snapshot. So the check reads every entry at the key (btree B8) against the *latest* state, which is PostgreSQL's "dirty snapshot": every committed word counts whatever its timestamp, and the writer's own words count (round-3 fix V3).

| What is at the key, in the latest state | Result |
| :--- | :--- |
| An entry another transaction created, or deleted, and has not ended | `Conflict::InProgress { holder }`: it may still abort |
| An entry committed and not deleted by a committed transaction, or the writer's own live entry | `UniqueViolation` for a plain insert. For `insert_speculative`, `UniqueViolation` if the statement's snapshot sees the row, and otherwise `Conflict::CommittedAfterSnapshot { commit_ts, latest }`, so that `ON CONFLICT DO UPDATE` can work from the latest row |
| An entry whose delete committed, at any timestamp | No conflict: the key is free |
| An entry whose creator aborted, or whose create and delete words name the same transaction | No conflict (layer §6.9) |

Whether a `UniqueViolation` or `InProgress` reaches the executor as a value depends on when the check runs (layer §6.5): before the write has changed anything (an IOT's primary key, the arbitrating index of `insert_speculative`) it is a value; in an index step after the row step it is `TamError::StatementFailed` (round-4 fix W2). A unique index compares key bytes only, never the locator. As in PostgreSQL, nulls are distinct: a key with a null in any column never conflicts. Speculative insertion rides on this check (layer §6.9).

**Cleanup.** An entry whose delete word, or whose aborted create, is older than the cleanup watermark is invisible to every snapshot that can still run. Index vacuum removes such entries page by page, as a pass of its own. A heap slot does not wait for it (heap §7.3).

**Other index kinds.** An internal index-method contract inside `cascade-index` is the seam behind which hash, GIN, GiST, BRIN and the native `pgvector` and `pg_trgm` kinds land later. Each must carry its own visibility, as B-tree entries do.

### 11.5 Sequences

`cascade-sequence` hands out monotonically increasing values with cached ranges per scope. Allocating a range is one log record, and the allocation waits until that record is durable before it returns any value from the range (round-3 fix V23). After a crash or failover the new owner resumes after the last durable range, so no value is reused; values cached and not handed out are skipped, as in PostgreSQL. Ranges are sized (by default 32 values, growing with the rate of use) so that the durability wait is rare. The executor and `serial`/`identity` defaults use a narrow `Sequence` contract.

### 11.6 Temporary tables

`cascade-temptable` is the third engine (layer §6.1). Its rows are visible only to the owning session. It keeps no log and no version words, apart from what session-local savepoints need, and it belongs to no scope — so writing to a temporary table never counts against the single-scope write rule (architecture §4.6). Rows live in session memory and spill to `cascade-tempstore` past a budget. Nothing in a temporary table survives the session.

### 11.7 Maintenance

`cascade-maintenance` drives `Maintenance::step` across a scope's relations within a budget per tick, so cleanup never exceeds its share and never stops a table (storage §12.1, principle 7). It keeps an in-memory work list per scope.

The **cleanup watermark** it works from is the oldest read timestamp still in use anywhere that reads this scope's pages: the leader, its followers and its read replicas. Prune and vacuum verdicts are logged and replayed on followers (page D2), so a leader that pruned against its own snapshots alone would remove versions a follower's reader can still see (round-3 fix V8). In single-box mode the watermark is the oldest open snapshot in the process. In the managed service it comes from `cascade-watermark` (architecture §4.5), and it is a dependency of Step 4 there. It must cover snapshots a follower *has not taken yet* (round-4 fix W7): a follower takes new snapshots at its own closed timestamp, which lags the leader's, so a verdict computed from today's snapshots could be replayed later on a follower whose new snapshot is older. The watermark is therefore at most the minimum, over the leader, every follower and every replica, of `min(oldest open snapshot, the closed timestamp it has applied)`. A follower's applied closed timestamp is the floor of every snapshot it can still open. A replica whose reader falls behind the watermark the leader is allowed to use, set by a bounded lag, gets the "snapshot too old" error that product definition §4.1 contemplates instead of holding cleanup back without limit. The scheduler will also enforce the per-database limits on transaction age and undo size (product definition §4.1) once that component exists.

---

## 12. Migration from the Current Code

Each step keeps the workspace building and its tests passing.

| Step | What lands | Needs |
| :--- | :--- | :--- |
| 1 | `cascade-access-api`: the contract (layer §6), the fake and the conformance suite | nothing |
| 2 | The mini-transaction seam (layer §9.1), on the runtime's log writer | nothing |
| 3 | The heap's first slice (plan T6.4): insert and scan, no visibility yet, sequential fill in LSN order (heap §2). A row's record is reserved and applied in the commit path (layer §7), so a rolled-back insert never reaches a page | Steps 1–2; `cascade-redo` (storage M7) |
| 4 | The MVCC milestone: slot states (page D1), visibility, the DML paths with real outcomes, prune, and the switch to record type 10 | Step 3; layer §13.1 decided |
| 5 | Engine hygiene: the dead dependencies go, the context files are synced | — |
| 6 | `cascade-btree-core` | the page design's module split (page L3); Steps 1–2 |
| 7 | `cascade-iot` and undo | Step 6; the IOT design |
| 8 | `cascade-index` and out-of-line storage | Steps 4 and 6 |
| 9 | Sequence, maintenance, temporary tables | Step 1 |

**How this relates to the storage migration** (storage §9). Storage M7 (redo and the page store) and M8 (the page cache onto `PageSource`; compute stops writing pages) run on their own schedule. They do not wait for this ladder, and two points of contact matter:

- Step 3 needs `cascade-redo`'s fill rule, which M7 delivers. M7 is written.
- After M8, the leader fetches an evicted page back from the page store instead of writing pages. An engine built on the seam (Step 2) needs no change when that happens. An engine that called the old page-write path directly would. That is why Step 2 comes before Step 3.

**What is not available until a step lands.**

- Until Step 4, rows have no visibility: every session sees every row.
- Until Step 8, the heap engine has no secondary indexes. A heap table with a primary key or a unique constraint cannot be enforced, so creating one is refused (`feature_not_supported`) rather than left unchecked.
- Until Step 7, every table is a heap table. The IOT becomes the default engine at Step 7.

---

## 13. Dependencies and Open Questions

### 13.1 Version words become self-describing (decided: stamping after commit)

**The problem.** Engines write `uncommitted(xid)` and never a committed word (heap §6). Every read of such a word asks the status table. That works while the entry exists, and as specified the entry must exist for as long as any page word names it:

- A row inserted by X and never updated keeps `xmin = uncommitted(X)` for the table's life.
- A live row whose delete was rolled back keeps `xmax = uncommitted(X)` likewise.

So the status table grows with the number of transactions that have ever touched a still-live row, and every row read is a lookup. PostgreSQL avoids this by writing the commit into the tuple — at commit, or by the first reader as a hint bit — which keeps its commit log compact.

**Whose decision.** The transaction layer's. It blocks Step 4.

**Two constraints the access layer adds.**

- A rewrite after commit is a page change. A1 allows one only with a redo record. An unlogged rewrite makes the leader's page differ from the replayed one, which layer §1.1 and the differential tests forbid.
- It must cover aborted transactions too, or their words stay behind on live rows.

**Directions.** Decided on 2026-11-27 by Kenneth: the recommendation below, register D-1. The comparison is kept for the record.

- *(a) Stamp through the log.* Each transaction records the positions of the words it stamped. At commit, and at abort, it emits stamp records for them, which `cascade-redo` applies everywhere. It costs one small record per touched row, written once. It gives the heap a real rollback in place of "write the abort record only" (kept §2). Status entries can then be retired once the stamps are durable.
- *(b) Keep the words and bound the table in memory another way.* No sound way is known. Transaction ids are monotonic, but commit order is not, so no range of ids can be declared resolved.
- *(c) Transaction status pages (round-3 fix V12).* The architecture already lists "transaction status pages" among `cascade-page`'s formats (architecture §4.2): a persistent, logged array indexed by transaction id, like PostgreSQL's commit log, holding each id's state, its commit timestamp, and for a subtransaction its top-level id (layer §6.6). It is paged through the buffer pool like any other data, so it never has to fit in memory, and recovery and followers rebuild it by replaying its records.

| | (a) Stamp through the log | (c) Status pages |
| :--- | :--- | :--- |
| Space | Status entries retire once a transaction's stamps are durable, so the table holds only recent transactions | About 16 bytes per transaction id (and since round 3 every writing statement has one), on disk, growing forever unless old pages are truncated; truncation needs every heap *and index* page below the cut to be frozen, which takes a pass over every page (round-4 fix W5) |
| Read cost | None once stamped; a lookup before | A lookup per unstamped word; one page read when the page is not cached |
| Commit cost | Stamp records proportional to rows touched, at commit or abort | One status record per transaction |
| Recovery | Must re-emit stamps for transactions that ended without them | Replays status records |
| Savepoints | Needs stamp-on-rollback per subtransaction | A sub-id is one more entry |

**Why the round-3 recommendation does not hold (round-4 fix W5).** Round 3 recommended (c) with lazy freezing by prune and vacuum. But prune and vacuum touch only pages that hold dead versions. A cold page whose rows never change is never frozen, and nothing freezes index pages at all, so the oldest unfrozen id would never advance without a periodic pass over every heap and index page: PostgreSQL's anti-wraparound vacuum, which heap §7.3 and product definition §4.1 set out to avoid. Without that pass the status pages grow by about 16 bytes per writing statement, roughly 14 GB a day at 10,000 per second.

**Recommendation:** (a) stamping, done *after* the commit rather than on its path, with (c) status pages as the bounded backing store. A commit is still one durable record and is acknowledged as before. Afterwards a background stamper, owned by the transaction layer, writes stamp records that replace the transaction's in-flight words with its commit timestamp, in row pages and index entries alike, from the positions each engine reported to `TxnCtx`. An abort, or a rollback to a savepoint, stamps the rolled-back words to zero instead (an empty creator makes the version invisible; an empty deleter makes it live). A transaction's status entry, and those of its subtransactions, are retired once its stamps are durable, so the status pages hold only transactions whose stamps are still pending, and they never need a whole-table pass. Recovery re-derives pending stamps from the positions logged with each mini-transaction. The cost is one small record per touched row and index entry, written once, off the commit path. This reversed round 3's recommendation, and Kenneth accepted it on 2026-11-27 (register D-1). Precedent: CockroachDB resolves its write intents to committed versions asynchronously after commit in the same way, and Oracle's delayed block cleanout stamps commit times lazily; PostgreSQL's approach (a compact commit log plus a freeze scan) was rejected because it needs the whole-table pass.

### 13.2 Changes proposed to the architecture

| Architecture | What it says | What this document proposes | Why |
| :--- | :--- | :--- | :--- |
| §4.5, `cascade-lock` | "Row locks themselves live in version words" | On a heap, exclusive row locks live in version words and shared ones in the lock manager. On an IOT, every row lock lives in the lock manager (A11, layer §6.10) | Several transactions can hold a shared lock on one row, which one word cannot list; and the IOT row has no word for a locker |
| §4.6, `cascade-access-api` | A trait, `TableAccessMethod` | An enum with three arms (layer §6.1). **Accepted** by Kenneth on 2026-11-27 (register D-5 item 1); the architecture is updated | The futures must be `!Send`, and temporary tables are a third engine |
| §4.4, `cascade-pagecache` | Only the cache's mini-transaction can change a page | The same, and the engine supplies records which the seam applies through `cascade-redo` (A10) | Leader and replay then share one code path |
| §4.8, commit budget | "A commit … takes no lock" | The commit path takes a short scope-wide commit-section mutex for one timestamp and one LSN reservation (layer §8, round-3 fix V19) | Commit timestamps and LSNs must be issued in the same order; a lock-free replacement is possible but not designed |
| §4.5, `cascade-txn` | One transaction id per transaction | Subtransaction ids per writing statement and per savepoint, with their top-level id in the status table (layer §6.6, round-3 fixes V1, V2) | Statement visibility and savepoint rollback on an engine without undo |

Three further points are confirmations, not changes: A3 follows the architecture's placement of page layouts in `cascade-page` (§3.2) and corrects the page design; the page LSN of layer §9.3 restates storage S17; and heap H9 supersedes the runtime plan's FR-46 acceptance that live layout may differ from replay — decided on 2026-11-26, and the plan is updated.

### 13.3 Open questions

| Question | Owner, and needed before |
| :--- | :--- |
| The final shape of the mini-transaction seam | The page cache's design; before Step 2 is frozen |
| `roll_ptr` encoding, undo record format, undo segment layout, how recovery finds an in-flight transaction's undo | The IOT design; before the first undo page format is frozen |
| The move of the key and value types to `cascade-datum` | The type layer's split; it ends A7 |
| How `ScopePages`' counters and extents survive a restart once compute replays nothing | The scope host's design; before Step 3 runs against the M8 world |
| A free-space predicate shared by `cascade-page` and `cascade-redo` instead of a mirror | With the storage M7 work |
| Locator validity beyond one statement, for cursors | The executor's design |
| Predicate-read reporting for serializable isolation | That design; the hook is reserved (layer §6.4) |
| Chunk format and compression for out-of-line storage | Before Step 8 |
| The cleanup watermark across scopes and replicas | `cascade-watermark`'s design |
| The free-space map's page layout | Before Step 4 |
| A11's two dependencies on other layers: the heap's two row-lock flag bits (the page layer) and the lock manager's per-row lock lists (`cascade-lock`) | Before Step 4 |
| `TableStats` | The planner's design |
| Bulk load | When it is scheduled; the record type is reserved |

---

## 14. Testing

| Level | What it checks | How |
| :--- | :--- | :--- |
| Golden bytes | Every page kind's layout and every record payload | Byte-literal tests at the format authority (`cascade-page`, `cascade-wal-format`), one per kind and version |
| Contract conformance | Every promise of layer §6 | The one generic suite, against the fake and every engine (layer §6.13) |
| Visibility | The truth table of layer §6.6 and each engine's mapping, over random interleavings of begin, write, commit and abort | Property tests in the contract crate, shared by the engines |
| Replay equivalence | For random workloads, pages rebuilt from the recorded groups equal the pages the leader produced, byte for byte | A differential harness over the in-memory seam's log and `cascade-redo`; the storage design's own differential test (storage §10) reuses it end to end |
| Engine differential | The engines agree on rows, outcomes and visibility for the same operations | Generic workloads through `Table`, arm against arm |
| Concurrency | The shared per-scope structures survive interleaving | `loom` on page hints, the free-space map and sequence caches (architecture Rule 11); the page cache and log writer carry their own |
| Row-level races | Reader against writer, prune against snapshot, vacuum against index entries, the HOT rules, speculative arbitration, row locks | Seeded interleavings through the fake seam and a scripted status source |
| Crash and fence | An `Io` or `Fenced` at each step of a mini-transaction poisons cleanly; replay reaches the durable prefix | A fault-injected seam fake; the storage design's crash tests cover the bytes |
| System | Wire behavior is unchanged when an engine is swapped | The runtime's query, commit and transaction suites |

## 15. Decision Log

| # | Decision | Status |
| :--- | :--- | :--- |
| A1 | The mini-transaction is the only write primitive. Layer §9.1 defines it | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.4 |
| A2 | One redo dispatch point; byte transforms at the format authorities | **Accepted** (Kenneth, 2026-11-27, register D-6). It composes architecture §4.2 and page D4 |
| A3 | B-tree page formats live in `cascade-page` | **Accepted** (Kenneth, 2026-11-27, register D-6). Architecture §3.2 places them there; it corrects page §2 |
| A4 | A scope is a shared-memory engine across its write core set | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.8, ADR-0003. **Supersedes** the retired WP5 D8 |
| A5 | Engines are built from a descriptor and never read the catalog | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.6 |
| A6 | Scope-level segment and page allocation; ids never reused | **Accepted** (Kenneth, 2026-11-27, register D-6). Applies storage S18 to this layer |
| A7 | The key contract stays in `cascade-pgtypes` for now, as a named exception | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A8 | One injected `VisibilitySource` | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A9 | `cascade-btree-core` is a transaction-blind tree | **Accepted** (Kenneth, 2026-11-27, register D-6). Restates architecture §4.6 |
| A10 | The engine supplies records; the seam applies them through `cascade-redo`. A page's LSN is the end of its last record | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A11 | Heap: exclusive row locks in the row header, shared ones in the lock manager. IOT: all in the lock manager | **Accepted** (Kenneth, 2026-11-27, register D-6). Changes architecture §4.5 (layer §13.2) |
| A12 | An injected wait seam (`WaitSource`): engines sleep until woken; deadlock and `lock_timeout` arrive as errors | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A13 | An enum for callers; a test-only trait for the suite, with `Table` delegating to it | **Accepted** (Kenneth, 2026-11-27, register D-6) |
| A14 | Subtransaction ids per writing statement and savepoint; a snapshot carries the statement horizon; savepoint rollback aborts sub-ids (layer §6.6) | **Accepted** (Kenneth, 2026-11-27, register D-11). Changes architecture §4.5 (layer §13.2) |
| A15 | The unique check reads the latest committed state, not the statement snapshot (layer §11.4) | **Accepted** (Kenneth, 2026-11-27, register D-6). Proposed in round-3 V3 |
| A16 | Index maintenance is a separate mini-transaction per index, in the row's statement and transaction (layer §11.4) | **Accepted** (Kenneth, 2026-11-27, register D-6). Proposed in round-3 V6 |
| A17 | A failure after a write's row step is `StatementFailed`; the transaction cannot commit until a savepoint rollback or an abort (layer §6.5) | **Accepted** (Kenneth, 2026-11-27, register D-14) |
| A18 | Overwriting an earlier own subtransaction's lock word first records that lock in the lock manager; lock hints are set, checked and cleared under the page's exclusive pin (layer §6.10) | **Accepted** (Kenneth, 2026-11-27, register D-6). Proposed in round-4 W3, W8 |

## 16. Provenance and Changes

**Origins.** The architecture and the storage design are linked in layer §2.3. The retired WP5 design (its decisions D1–D15, accepted 2026-11-14) became part of the contract in layer §6:

| WP5 | Subject | Now in |
| :--- | :--- | :--- |
| D1, D7 | Dispatch by enum | layer §6.1 (A13) |
| D2 | Opaque, statement-scoped locators | layer §6.5 |
| D3 | Outcomes as values; bounded, engine-side waits | layer §6.5, §6.11 |
| D4 | One visibility rule | layer §6.6 (A8); the truth table is new |
| D5 | Engines maintain secondary indexes | layer §11.4 |
| D6 | Read surface; the re-check; `lock_row` returns the row | layer §6.8, §6.10 |
| D8 | Message-to-owner reads | **Superseded** by A4 |
| D9 | Errors; poisoned transactions | layer §6.11; lock outcomes are new |
| D10 | Statement boundary | layer §6.7 |
| D11 | Scan protocol | layer §6.8 |
| D12 | Upsert by speculative insertion | layer §6.9; an abort now changes bytes |
| D13 | The key contract | A7 |
| D14 | DDL is the physical phase only | layer §6.12 |
| D15 | `RowRef` owns its buffer; lazy TOAST | layer §6.4, heap §8 |

The retired storage HLD's engine sections (§11–§13) are kept in `engine-semantics.md`.

**What v0.2 changed, after the review of v0.1.**

| Finding | Change |
| :--- | :--- |
| R1, R2, R25, R26 | Contract types restated (layer §6.4); visibility as a truth table (§6.6); a citation convention, a source table and an identifier legend (§2); a glossary (§3); a worked write (§7); one status vocabulary (§4, §15); migration steps renamed (§12) |
| R3, R8 | The heap walk and the per-engine mapping of the visibility outcomes (layer §6.6, heap §5.2) |
| R4 | Row locks: a representation, and `lock_row` stamps the row (layer §6.10, heap §4.4) |
| R5 | `abort_speculative` changes bytes (layer §6.9, heap §4.5) |
| R6, R7 | B-tree writers pin first and validate after; the root pointer is the top-level version (btree §3, §4) |
| R9 | The unique check's two outcomes are the right way round (layer §11.4) |
| R10 | The status table's bound is a blocking dependency with its constraints and two directions (layer §13.1) |
| R11 | Vacuum frees slots on the cleanup watermark; index vacuum is its own pass; builds block writes (heap §7.3, layer §6.12) |
| R12, R13, R14 | Tree keys for non-unique indexes and duplicate keys; an entry length; `mod_count` defined (btree B8, §2) |
| R15 | A page's LSN is the end of its last record (layer §9.3) |
| R16 | One mini-transaction model (layer §9.1, A10) |
| R17 | The ladder is re-sequenced and states what each step withholds (layer §12) |
| R18 | The serialization points are listed as they are (layer §8) |
| R19 | Three engine arms; the suite runs over a test-only trait (layer §6.1, §6.13) |
| R20 | A wait seam; lock outcomes in the error type (layer §6.5, §6.11) |
| R21 | The re-check takes a fresh snapshot (layer §6.8, heap §5.2) |
| R22 | The IOT delete-mark's dependency on undo (layer §11.3, btree §2) |
| R23 | No merge and no root collapse in version 1, and what that costs (btree §5.5) |
| R24 | The free-space map is a tree (heap §7.2) |
| R27–R30 | The contradictions, the stale-epoch rule, the dangling references and the `Datum` claim are fixed in place |

**Corrections this work made or still owes in other files.**

| File | State |
| :--- | :--- |
| `cascade-heap/AI/CONTEXT.md`, `cascade-btree/AI/CONTEXT.md` | Rewritten for the current engine semantics (commit `1246443`), and again to match v0.2 |
| `cascade-page/src/lib.rs`, its `AI/CONTEXT.md` | The append fit rule counts the slot entry it writes (commit `54b30c4`) |
| `runtime-implementation-plan.md` | FR-46 and the T6.4 bullets follow heap H9 (commit `1246443`) |
| `cascade-redo/src/lib.rs` | The same fit-rule correction, uncommitted with the storage M7 work |
| Page design §2 and its concurrency paragraph; `cascade-page/AI/CONTEXT.md` | **Owed.** The B-tree formats' home (A3) and the shared-memory engine (A4) are corrected there when the page design's module split lands |
| `cascade-heap/Cargo.toml` | **Owed** at Step 5: the description and the two dead dependencies |
| `cascade-mvcc/AI/CONTEXT.md`, `cascade-undo/AI/CONTEXT.md` | **Owed** to the transaction layer's design pass |
