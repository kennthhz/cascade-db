# Access layer: decisions needed from the project owner

**Status:** Living register, started 2026-11-27. It lists every decision that must be made before the access layer (L5) can be implemented, who makes it, what it blocks, and the recommendation. When a decision is made, its row moves to **Decided** with the date and who decided, and the design document it changes is updated in the same commit.

**Sources:** the [layer design](access-layer-design.md) v0.2 (§4, §13), the [heap design](../cascade-heap/AI/heap-design.md) (§12, §13), the [btree design](../cascade-btree/AI/btree-design.md) (§10, §12), the [page design](../cascade-page/AI/storage-page-mutation-design.md), [`docs/architecture-design.md`](../../../docs/architecture-design.md) v0.10.2, and the code at `main` `5da1c2b`. Citation prefixes follow layer §2.2.

**Status values:** **Open** (needs a decision), **Decided** (by whom, when), **Deferred** (with the trigger that reopens it).

---

## 1. Open decisions

Ordered by what they block. "Owner" is who decides; Kenneth is the project owner and ratifies every row. D-1 and D-11 to D-13 were added or changed by the fixes for the round-3 review ([`access-design-review-2026-11-27.md`](access-design-review-2026-11-27.md)).

### D-1. How version words become self-describing

| | |
| :--- | :--- |
| **Question** | Engines only ever write `uncommitted(xid)`, so the status table must keep an entry for every transaction any live row still names, forever. How is that bounded? |
| **Options** | (a) Stamp through the log: each transaction records the positions it stamped; at commit and abort it emits stamp records that `cascade-redo` applies everywhere, after which its status entry can be retired. (b) Keep the words and bound the table in memory some other way: no sound method is known, because commit order is not txn-id order. (c) Transaction status pages, which the architecture already lists (§4.2): a logged, paged array indexed by transaction id, holding state, commit timestamp and top-level id, cached like any other page (added by round-3 review V12). |
| **Recommendation** | Changed 2026-11-27: (c) for version 1, with (a)'s logged stamping used lazily by prune and vacuum to freeze old words so that old status pages can be truncated (layer §13.1 compares the three). Commit stays one record, and subtransaction ids for savepoints (D-11) are just more entries. |
| **Owner** | Transaction layer, ratified by Kenneth |
| **Blocks** | Step 4 (the MVCC milestone) for every engine |
| **Status** | Open |

### D-2. Who builds the transaction-side seams, and when

| | |
| :--- | :--- |
| **Question** | The contract injects `VisibilitySource` (status table), `WaitSource` (lock manager wake-ups), `TxnCtx::fresh_snapshot` (closed timestamp) and the shared-lock lists of A11. None of them exists in code: the runtime's `txnmgr` is only an HLC clock, and there is no `cascade-lock` or `cascade-scope-txn`. The layer documents describe them as if `txnmgr` already had them. |
| **Options** | (a) Build the L4 pieces (`cascade-txn-api`, status table, closed timestamp, `cascade-lock`) as their own work stream before Step 4, with access Steps 1–3 proceeding against fakes. (b) Grow them inside the runtime's `txnmgr` first and split them out later. |
| **Recommendation** | (a). The access contract only needs the trait shapes, and the fakes in `cascade-access-api` cover Steps 1–3. Step 4 cannot start until real implementations exist. |
| **Owner** | Kenneth |
| **Blocks** | Step 4; the real conformance runs of every engine |
| **Status** | Open |

### D-4. The mini-transaction seam and the page cache

| | |
| :--- | :--- |
| **Question** | A1 and A10 require that only the seam changes a page. Today `cascade-bpm` exposes `PageGuardMut::write` and `write_logged`, which hand callers the raw page bytes. Where does `MiniTxn` live, and when do the raw writers disappear? |
| **Options** | (a) A mini-transaction module inside `cascade-bpm` that owns the only mutable access, with `write`/`write_logged` made crate-private once the runtime's heap path moves onto it. (b) A new crate between the engines and `cascade-bpm`, leaving the raw writers public. |
| **Recommendation** | (a). It is the only option that makes a seam bypass fail to compile, which is the point of A1. |
| **Owner** | Page cache design, ratified by Kenneth |
| **Blocks** | Step 2, and so Step 3 |
| **Status** | Open |

### D-5. The architecture changes the layer design proposes

| | |
| :--- | :--- |
| **Question** | Accept the three changes of layer §13.2 and update `docs/architecture-design.md`? |
| **Items** | (1) A13: callers see an enum `AnyTable` with three arms, plus a test-only trait for the conformance suite, instead of the architecture's `TableAccessMethod` trait. (2) A11: heap exclusive row locks in the row header, shared ones in the lock manager; IOT row locks all in the lock manager. This replaces "row locks themselves live in version words". (3) A10: engines supply records and the seam applies them through `cascade-redo`. (4) The commit path's short commit-section mutex, against "a commit takes no lock" (round-3 V19). (5) A14: subtransaction ids (D-11). The architecture now carries these as marked "pending D-5" notes (round-3 V32), to be made plain text when decided. |
| **Recommendation** | Accept all five. |
| **Owner** | Kenneth |
| **Blocks** | (1) Step 1. (2) Step 4. (3) Step 2. (4) Step 4. (5) Step 1. |
| **Status** | Open |

### D-6. Ratify the proposed layer, B-tree and heap decisions

| | |
| :--- | :--- |
| **Question** | Every decision in the three designs is still Proposed except H9. Which are accepted as written? |
| **Items** | Layer A1–A16 (layer §15). B-tree B1–B13 (btree §12). Heap H1–H8, H2a, H10–H13 (heap §13). Page L1–L3 and D1–D6 (page design §9). |
| **Recommendation** | Ratify in the order they are needed: A-decisions before Step 1; A10 and the page L3 split before Step 2; H1–H13 and page D1–D6 before Step 4; B1–B11 before Step 6. The ones that only restate the architecture (A1, A4, A5, A9) can be accepted now. |
| **Owner** | Kenneth |
| **Blocks** | Each step, as listed |
| **Status** | Open |

### D-7. The IOT engine's design

| | |
| :--- | :--- |
| **Question** | The default engine has no detailed design. Open: the `roll_ptr` encoding, the undo record format, the undo segment layout (per transaction, per core, shared), how recovery finds an in-flight transaction's undo without a checkpoint, and how undo pages are purged. `cascade-undo` is a stub from a replaced design. |
| **Recommendation** | Write the IOT and undo design as its own document before Step 6 starts, so the B-tree's leaf format and the undo page kind are frozen together. |
| **Owner** | Kenneth (scheduling); the IOT design (content) |
| **Blocks** | Step 7, and the first undo page format |
| **Status** | Open |

### D-8. How `ScopePages` survives a restart

| | |
| :--- | :--- |
| **Question** | Segment ids must never be reused, and a sequential scan stops at a segment's extent. After storage M8, compute replays nothing, so where do the segment counter and the extents come from on open? |
| **Options** | (a) Log a record for each `new_segment` and `extend`, and rebuild the state from the log tail plus a small persisted page. (b) Keep them in the catalog. (c) Ask the page store for each segment's extent on open. |
| **Recommendation** | (a), so it follows the same rule as every other page change. To be confirmed in the scope host's design. |
| **Owner** | Scope host design, ratified by Kenneth |
| **Blocks** | Step 3 running against the M8 world |
| **Status** | Open |

### D-9. Smaller format and layering decisions

| # | Question | Recommendation | Blocks |
| :--- | :--- | :--- | :--- |
| D-9a | The free-space map's page layout (kind 6) | Freeze it with H5/H12 before Step 4 | Step 4 |
| D-9b | The heap row-lock flag bits `XMAX_LOCK_ONLY`, `XMAX_UPDATES_KEY` and the `HAS_SHARED_LOCKS` hint, in the `next` word's flag field; and the IOT leaf row's `HAS_LOCKS` bit (round-3 V16) | Assign them in `cascade-page` while the formats are still major 0 | Step 4 (heap), Step 7 (IOT) |
| D-9c | The `cascade-page` frame/heap module split (page L3) | Land it early; it is code organization only | Step 6 |
| D-9d | The upward dependency from `cascade-access-api` to `cascade-pgtypes` until `cascade-datum` exists (A7) | Accept as a named exception in the layering check | Step 1 |
| D-9e | In the Step 3 slice a row is reserved and applied in the commit path, so a transaction cannot read its own uncommitted inserts | Accept for Step 3 and say so in the step's notes, or apply at insert and filter by an in-memory "own rows" set | Step 3 |
| D-9f | A shared free-space predicate for `cascade-page` and `cascade-redo` instead of a mirror | Do it with the next change to either | — |
| D-9g | Text indexes support only the `C` collation until `cascade-datum` provides collation-aware keys; other collations are refused at `CREATE INDEX` (layer A7, round-3 V24) | Accept, and list it in product definition §5.5 | Step 8 |
| D-9h | Sequence range size (default 32, growing with use); allocation waits for durability (layer §11.5, round-3 V23) | Accept | Step 8 |

### D-10. The access-api implementation design (Step 1)

| | |
| :--- | :--- |
| **Question** | Accept decisions C1–C13 of the [access-api design](../cascade-access-api/AI/access-api-design.md)? |
| **Highlights** | C1: three crates, because `AnyTable` cannot live in the contract crate without a dependency cycle. C2: a `Fake` arm on `AnyTable` behind a feature. C5: `TxnCtx` concrete in `cascade-txn-api`, with `begin_statement` and `fresh_snapshot` async. C6: unresolved speculative tokens returned by `end_statement`. C7: `RowTooLarge` and `Unsupported` errors. C8: no L5 types in `WaitFor`. C9: no `TableEnv` in the contract. |
| **Recommendation** | Accept as written, or comment on the PR |
| **Owner** | Kenneth |
| **Blocks** | Step 1 |
| **Status** | Partly decided. C1, C5, C6 and C10 accepted by Kenneth on 2026-11-27, and C8's opaque lock key with them, since C1 forbids L4 naming an L5 type. C2, C3, C4, C7, the rest of C8, and C9, C11, C12, C13 are still open |

### D-11. Statement visibility and savepoints (round-3 review V1, V2)

| | |
| :--- | :--- |
| **Question** | A statement must not see its own changes (the Halloween problem), and `ROLLBACK TO SAVEPOINT` must work on the heap, which has no undo. How? |
| **Options** | (a) Subtransaction ids: each writing statement and each savepoint stamps a fresh id recorded with its top-level transaction; a snapshot carries the statement's horizon; savepoint rollback marks sub-ids aborted. (b) A command id in each row header, as PostgreSQL does, plus a separate savepoint mechanism. (c) A per-transaction in-memory map from written versions to statements. |
| **Recommendation** | (a), now written into layer §6.6 as A14. It needs no row-format change and solves both problems with one mechanism. Cost: one status entry per writing statement, and the status table must map sub-ids to their top. |
| **Owner** | Transaction layer, ratified by Kenneth |
| **Blocks** | Step 1 (it changes `Snapshot` and `TxnStatus` in `cascade-txn-api`) |
| **Status** | Open |

### D-12. The cleanup watermark in the managed service (round-3 review V8)

| | |
| :--- | :--- |
| **Question** | Prune and vacuum verdicts replay on followers and read replicas. Their snapshots must hold cleanup back, or be cut off. |
| **Recommendation** | In the managed service the watermark comes from `cascade-watermark`, covering followers and replicas, with a bounded lag after which a replica's reader gets "snapshot too old". Single-box mode keeps the process-local watermark (layer §11.7). |
| **Owner** | `cascade-watermark` design, ratified by Kenneth |
| **Blocks** | Step 4 in the managed service |
| **Status** | Open |

### D-13. The IOT tree reclaims empty leaves (round-3 review V11)

| | |
| :--- | :--- |
| **Question** | The no-merge policy (B6) leaves empty leaves behind, which defeats the job-queue showcase. |
| **Options** | (a) Unlink empty leaves, leaving a tombstone whose page is reused after the cleanup watermark (btree B12). (b) Drop the queue showcase from version 1. (c) Full merge. |
| **Recommendation** | (a), now in btree §5.5. |
| **Owner** | Kenneth |
| **Blocks** | Step 6 |
| **Status** | Open |

## 2. Decided

| # | Decision | By, when |
| :--- | :--- | :--- |
| D-3 | `Snapshot`, the visibility rule, `WaitSource` and `TxnCtx` live in a new, small `cascade-txn-api` (L4), created now with only what the access contract consumes (access-api C1) | Kenneth, 2026-11-27 |
| C1 | Three crates: `cascade-txn-api`, `cascade-access-api`, and `cascade-access` for the dispatch enums above the engines | Kenneth, 2026-11-27 |
| C5 | `TxnCtx` is concrete in `cascade-txn-api`; `begin_statement` and `fresh_snapshot` are async | Kenneth, 2026-11-27 |
| C6 | `end_statement` returns unresolved speculative tokens; the executor aborts them | Kenneth, 2026-11-27 |
| C10 | One fake engine with two locator flavours, `Heap` and `Iot` | Kenneth, 2026-11-27 |
| H9 | While the heap insert record is type 5, placement follows `cascade-redo`'s sequential-fill rule and the fill cursor advances in LSN order | Kenneth, 2026-11-26 |

## 3. Deferred

None yet.
