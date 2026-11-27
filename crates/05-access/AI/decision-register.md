# Access layer: decisions needed from the project owner

**Status:** Living register, started 2026-11-27. It lists every decision that must be made before the access layer (L5) can be implemented, who makes it, what it blocks, and the recommendation. When a decision is made, its row moves to **Decided** with the date and who decided, and the design document it changes is updated in the same commit.

**Sources:** the [layer design](access-layer-design.md) v0.2 (§4, §13), the [heap design](../cascade-heap/AI/heap-design.md) (§12, §13), the [btree design](../cascade-btree/AI/btree-design.md) (§10, §12), the [page design](../cascade-page/AI/storage-page-mutation-design.md), [`docs/architecture-design.md`](../../../docs/architecture-design.md) v0.10.2, and the code at `main` `5da1c2b`. Citation prefixes follow layer §2.2.

**Status values:** **Open** (needs a decision), **Decided** (by whom, when), **Deferred** (with the trigger that reopens it).

---

## 1. Open decisions

Ordered by what they block. "Owner" is who decides; Kenneth is the project owner and ratifies every row.

### D-1. How version words become self-describing

| | |
| :--- | :--- |
| **Question** | Engines only ever write `uncommitted(xid)`, so the status table must keep an entry for every transaction any live row still names, forever. How is that bounded? |
| **Options** | (a) Stamp through the log: each transaction records the positions it stamped; at commit and abort it emits stamp records that `cascade-redo` applies everywhere, after which its status entry can be retired. (b) Keep the words and bound the table some other way: no sound method is known, because commit order is not txn-id order. |
| **Recommendation** | (a), as layer §13.1 recommends. It also gives the heap a real rollback. Cost: one small record per touched row, written once. |
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

### D-3. Where `Snapshot` and the visibility rule live

| | |
| :--- | :--- |
| **Question** | Architecture §4.5 puts `Snapshot` and `Snapshot::visible` in `cascade-txn-api` (L4). The layer design §6.6 (A8) defines them in `cascade-access-api` (L5). `TxnCtx` is likewise "constructed by the transaction layer" but defined in L5. |
| **Options** | (a) Move `Snapshot`, `TxnStatus`, `VisibilitySource`, the visibility rule and `TxnCtx` to `cascade-txn-api`; `cascade-access-api` depends on it. (b) Keep them in `cascade-access-api` and change the architecture. |
| **Recommendation** | (a), by creating a small `cascade-txn-api` now with only what the access contract consumes, as [access-api design](../cascade-access-api/AI/access-api-design.md) C1 and C5 propose. Index kinds and the undo walk use the same rule, and L4 owning it matches the architecture. |
| **Owner** | Kenneth |
| **Blocks** | Step 1 (the contract's crate layout) |
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
| **Items** | (1) A13: callers see an enum `AnyTable` with three arms, plus a test-only trait for the conformance suite, instead of the architecture's `TableAccessMethod` trait. (2) A11: heap exclusive row locks in the row header, shared ones in the lock manager; IOT row locks all in the lock manager. This replaces "row locks themselves live in version words". (3) A10: engines supply records and the seam applies them through `cascade-redo`. |
| **Recommendation** | Accept all three. |
| **Owner** | Kenneth |
| **Blocks** | (1) Step 1. (2) Step 4. (3) Step 2. |
| **Status** | Open |

### D-6. Ratify the proposed layer, B-tree and heap decisions

| | |
| :--- | :--- |
| **Question** | Every decision in the three designs is still Proposed except H9. Which are accepted as written? |
| **Items** | Layer A1–A13 (layer §15). B-tree B1–B11 (btree §12). Heap H1–H8, H10–H13 (heap §13). Page L1–L3 and D1–D6 (page design §9). |
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
| D-9b | The two heap row-lock flag bits, `XMAX_LOCK_ONLY` and `XMAX_UPDATES_KEY`, in the `next` word's flag field | Assign them in `cascade-page` while the heap format is still major 0 | Step 4 |
| D-9c | The `cascade-page` frame/heap module split (page L3) | Land it early; it is code organization only | Step 6 |
| D-9d | The upward dependency from `cascade-access-api` to `cascade-pgtypes` until `cascade-datum` exists (A7) | Accept as a named exception in the layering check | Step 1 |
| D-9e | In the Step 3 slice a row is reserved and applied in the commit path, so a transaction cannot read its own uncommitted inserts | Accept for Step 3 and say so in the step's notes, or apply at insert and filter by an in-memory "own rows" set | Step 3 |
| D-9f | A shared free-space predicate for `cascade-page` and `cascade-redo` instead of a mirror | Do it with the next change to either | — |

### D-10. The access-api implementation design (Step 1)

| | |
| :--- | :--- |
| **Question** | Accept decisions C1–C13 of the [access-api design](../cascade-access-api/AI/access-api-design.md)? |
| **Highlights** | C1: three crates, because `AnyTable` cannot live in the contract crate without a dependency cycle. C2: a `Fake` arm on `AnyTable` behind a feature. C5: `TxnCtx` concrete in `cascade-txn-api`, with `begin_statement` and `fresh_snapshot` async. C6: unresolved speculative tokens returned by `end_statement`. C7: `RowTooLarge` and `Unsupported` errors. C8: no L5 types in `WaitFor`. C9: no `TableEnv` in the contract. |
| **Recommendation** | Accept as written, or comment on the PR |
| **Owner** | Kenneth |
| **Blocks** | Step 1 |
| **Status** | Open |

## 2. Decided

| # | Decision | By, when |
| :--- | :--- | :--- |
| H9 | While the heap insert record is type 5, placement follows `cascade-redo`'s sequential-fill rule and the fill cursor advances in LSN order | Kenneth, 2026-11-26 |

## 3. Deferred

None yet.
