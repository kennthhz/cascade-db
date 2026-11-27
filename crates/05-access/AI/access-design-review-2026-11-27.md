# Access layer design review, round 3 — 2026-11-27

**What this is.** An adversarial review of every design document under `crates/05-access`, written from the stance of an outside senior principal engineer who has not worked on Cascade DB. It follows the two earlier rounds in [`access-design-review.md`](access-design-review.md) (findings R1–R30 and T1–T12, all applied), so it looks for what those rounds missed rather than re-checking their fixes.

**Documents reviewed** (on `main` at `5eedd63`, plus PR #3):

| Short name | Document |
| :--- | :--- |
| layer | [`AI/access-layer-design.md`](access-layer-design.md) v0.2 |
| kept | [`AI/engine-semantics.md`](engine-semantics.md) |
| heap | [`cascade-heap/AI/heap-design.md`](../cascade-heap/AI/heap-design.md) v0.2 |
| btree | [`cascade-btree/AI/btree-design.md`](../cascade-btree/AI/btree-design.md) v0.2 |
| page | [`cascade-page/AI/storage-page-mutation-design.md`](../cascade-page/AI/storage-page-mutation-design.md) (2026-11-15) |
| api | `cascade-access-api/AI/access-api-design.md` v0.1 (in PR #3) |
| register | [`AI/decision-register.md`](decision-register.md) |
| contexts | the three `AI/CONTEXT.md` files |

They were read against [`docs/product-definition.md`](../../../docs/product-definition.md) and [`docs/architecture-design.md`](../../../docs/architecture-design.md) v0.10.2, and the claims about code were checked against the code.

**What the review looked for.** (1) Algorithms that would give wrong answers or lose a guarantee if built as written. (2) Contradictions with the product definition, the architecture, or between the access documents. (3) Whether a reader from outside the project can follow the documents: undefined terms, dense prose, and statements made without the background needed to judge them.

**Fix status.** On 2026-11-27 all 12 High findings and all 19 Medium findings were fixed in the documents, each marked below with **Fixed** and a one-line description. The Low findings (V27–V30, V33, V37–V40) are not yet addressed. Choices made while fixing that need Kenneth's ratification are in the [decision register](decision-register.md): D-1 (changed), D-5 (two more items), D-9b, D-9g, D-9h, D-11, D-12 and D-13.

**Severity.** **High**: wrong query results, a broken product promise, or an algorithm that cannot be built as specified. **Medium**: underspecified, a cost not acknowledged, or a contradiction that will mislead an implementer. **Low**: clarity or tidiness. Findings are numbered V1–V40.

---

## Verdict

The layering, the one-contract-many-engines shape, and the redo discipline are sound. The two earlier rounds fixed most local errors. But this round found **four gaps that would break PostgreSQL behavior for ordinary statements**, and none of them was on any open-question list:

1. **No statement-level visibility (V1).** A transaction sees all of its own writes, including the ones the current statement just made. An `UPDATE` that scans a heap table will find the new versions it wrote and update them again.
2. **No savepoints on the heap (V2).** Product definition §5.3 promises savepoints, but a heap stamps every word with the one transaction id and has no undo, so `ROLLBACK TO SAVEPOINT` cannot undo anything on a heap table.
3. **The unique check reads the wrong snapshot (V3).** It decides against the statement's snapshot. PostgreSQL decides against the latest committed state, so the design both reports false violations and lets through real duplicates.
4. **A heap engine cannot find the index entries it must stamp (V4).** Entries point at the root of a HOT chain, but the engine reaches rows through newer versions, and chains have no back pointer.

There are also eight more High findings (V5–V12), several of them in paths the previous rounds rewrote. Readability has improved a lot since round 1, but the documents still assume a reader who already knows PostgreSQL's internals, InnoDB, ARIES and this project's history. The page design was never brought up to the current conventions and contradicts three accepted decisions.

**Recommendation.** Do not start Step 3 or later code until V1–V4 have a decided design. Step 1 (the contract crate) can proceed, but it must first add a statement identifier to `Snapshot` (V1), or the contract will have to change later.

---

## Part 1. Correctness

### High

#### V1. A snapshot has no statement identifier, so statements see their own output

*Where:* layer §6.6 (`Snapshot { read_ts, own }` and the truth table), heap §5.1–§5.2, api §4.1.

*Problem:* The rule makes every version created by `own` visible to every read of that transaction. PostgreSQL also records which *command* in the transaction made a change (its `cmin`/`cmax` command ids), and a statement sees only changes made by earlier commands. Without that:

- `UPDATE t SET v = v + 1` on a heap table scans pages in order. Each update appends a new version, often later in the same page or on a later page. The scan reaches that version, sees it as `Live` (its creator is `own`), and updates it again. The statement does not terminate, or it updates rows several times. This is the "Halloween problem".
- `INSERT INTO t SELECT * FROM t` reads its own inserts.
- On an IOT, a primary-key update moves a row to a higher key, ahead of an ordered scan, which then meets it again.

The architecture already expects this: its `NestedCtx` carries "a command counter so that nested statements see earlier changes" (architecture §4.9). The access contract has nowhere to put it.

*Fix:* Add a statement (command) number to `Snapshot` and to every version a transaction writes. Options: keep a command id in the row header (PostgreSQL); or keep, per transaction, an in-memory map from each version written to the statement that wrote it, consulted only when the creator is `own`; or have each engine skip versions created by the current statement during scans. Decide before the contract crate freezes `Snapshot`, and add a suite group for it.

**Fixed (2026-11-27):** Each writing statement stamps its own subtransaction id, and `Snapshot.own` became `OwnView { top, horizon }`: own words at or above the statement's horizon are invisible to that statement, so it never sees its own output. Layer §6.6 and A14; api §4.1; new suite group S13; register D-11.

#### V2. Savepoints cannot work on the heap engine

*Where:* heap §1 ("It keeps no undo"), heap §6, layer §6.6, kept §2; product definition §5.3; architecture §4.5 and §4.12.

*Problem:* Product definition §5.3 promises savepoints, and every ORM uses them, often implicitly. The architecture assigns savepoint rollback to `cascade-undo`, which only the IOT uses. On a heap, rows are stamped `uncommitted(xid)` with the transaction's single id. After `ROLLBACK TO SAVEPOINT`, the rows written since the savepoint still carry that id and stay visible to the transaction and, on commit, to everyone. Nothing in any access document mentions savepoints, except one sentence about temporary tables (layer §11.6).

*Fix:* Choose a mechanism before Step 4. Options: subtransaction ids as PostgreSQL does, so that rolling back a savepoint marks a sub-id aborted in the status table, which also needs the status table to map sub-ids to their parent; or stamp-through-the-log rollback, which register D-1(a) would make possible. Add savepoints to the register.

**Fixed (2026-11-27):** The same subtransaction ids give savepoints: `ROLLBACK TO SAVEPOINT` marks every sub-id since the savepoint aborted in the status table, so the heap needs no undo; the IOT also restores from undo. Layer §6.6, heap §1, architecture §4.5 (pending note); register D-11.

#### V3. The unique check uses the statement snapshot instead of the latest committed state

*Where:* layer §11.4 (unique check table), btree §6, layer §6.9; compare kept §3 ("visible-live in the latest state").

*Problem:* The table says "committed and visible to the statement's snapshot → `UniqueViolation`" and "committed after the statement's snapshot → `CommittedAfterSnapshot`". Two wrong answers follow:

- A key whose row was *deleted* by a transaction that committed after the snapshot is still `Live` to the snapshot, so the insert reports `UniqueViolation`. The key is in fact free, and PostgreSQL accepts the insert.
- A plain `INSERT` that meets a key inserted by a transaction committed after its snapshot gets a `Conflict` value, not a violation. Nothing says what the executor does with it. If it retries or ignores it, a duplicate is created.

Uniqueness is a property of the current state, not of a snapshot. Kept §3 had it right ("visible-live in the latest state"); the layer table replaced it.

*Fix:* Evaluate the unique check against the latest committed state plus own changes (PostgreSQL's "dirty snapshot"): another transaction's in-progress entry gives `InProgress`; a committed, undeleted entry gives `UniqueViolation`; a committed and deleted entry gives no conflict. Keep `CommittedAfterSnapshot` only for the `ON CONFLICT DO UPDATE` path, where the executor needs the row.

**Fixed (2026-11-27):** The unique check now reads the latest committed state (a dirty snapshot): a key whose delete committed is free, a committed live key is a violation for a plain insert, and `CommittedAfterSnapshot` is kept only for `insert_speculative`. Layer §11.4 table and §6.9, btree §6, suite S7; decision A15.

#### V4. The heap engine cannot locate the index entries it must stamp

*Where:* heap §3 (what a locator names), heap §4.2 ("delete words on newest's entries"), heap §4.3, heap §7.3 step 1, layer §11.4.

*Problem:* After HOT updates, a row's index entries point at the chain's *root* (the oldest version, page D1). A non-HOT update or a delete must put delete words on those entries. To find an entry in a non-unique index, the tree key is key + locator (btree B8), so the engine needs the root's Tid. But:

- A sequential scan returns "this version's own Tid" (heap §5.1), which after a HOT update is not the root.
- Chains link old → new only, so the engine cannot walk back from the version it holds to the root.
- "Newest's entries", in heap §4.2, do not exist after a HOT update: the newest version has no entries of its own.

So deletes and non-HOT updates reached by a scan cannot stamp the right entries. Heap H6, the vacuum safety argument, rests on every entry being stamped, so the failure also lets vacuum free a slot while a live entry still points at it. A later reader then follows that entry to whatever row reuses the slot.

*Fix:* Pick one: store the root slot in each heap-only version (PostgreSQL derives it with a page scan); have scans return the root Tid as the locator; or stop stamping entries on update and give up H6's argument, as PostgreSQL does. Then restate H6.

**Fixed (2026-11-27):** Index entries always name the HOT chain's root, and the engine derives a version's root under the pin from the page's slot links, as PostgreSQL does. Deletes and non-HOT updates stamp the root's entries; H6 restated on that basis. Heap §3, §4.2, §4.3, §7.3; decision H2a.

#### V5. A rolled-back update leaves a dangling chain pointer that a later reader follows

*Where:* heap §4.2, heap §5.2 (chain walk), heap §7.1 (prune relinks only on the same page), page D2.

*Problem:* A non-HOT update by X stamps v1's deleter, links `v1.next` to v2 on another page, and inserts v2. Suppose X aborts. v1 stays `Live` (deleter aborted), but `v1.next` still points at v2. Prune of v2's page removes v2, and vacuum frees its slot. Prune of v1's page cannot relink v1, because the successor was on another page. Later, a new row R takes v2's old slot. Then Y deletes v1 and commits. A reader whose snapshot sees Y's delete finds v1 `Deleted`, follows `next`, and returns R as the row. That is a wrong result.

PostgreSQL guards against this exact case: when it follows `t_ctid`, it checks that the next tuple's creator equals the previous tuple's deleter.

*Fix:* In the chain walk, hop only if the successor's creator word names the same transaction as the predecessor's deleter word, and treat a mismatch as end of chain. Or have a new deleter clear `next`. Add the case to heap §5.2's table and to the property test.

**Fixed (2026-11-27):** The chain walk hops only if the successor's creator word equals the predecessor's deleter word, and otherwise treats the link as stale. The case is added to heap §5.2's table and the tests. Heap §5.2, H10.

#### V6. Index maintenance "in the same mini-transaction group" breaks the no-await rule

*Where:* layer §11.4 ("in the same mini-transaction group as the row"), layer §9.1 ("no `await` between its first pin and its last unpin"), heap §4.2, heap §7.3 step 1, btree §3.4.

*Problem:* An insert into a table with three indexes would have to hold the heap page's exclusive pin while it descends three B-trees. A descent awaits whenever a page is not resident (btree §3), and an index insert may split, which needs its own multi-pin protocol. So the requirement contradicts the rule that makes mini-transactions safe. It also creates pin-order deadlocks between heap pages and index pages that the design does not order: heap §4 says to pin two heap pages "in a fixed order, as btree §3.4 describes", but btree §3.4's order is parent → left → right, which has no meaning for two heap pages.

Heap H6's argument does not need the same group. It needs the delete word to be written by the same *transaction*, because visibility, not log atomicity, is what hides the entry.

*Fix:* Make index maintenance separate mini-transactions in the same statement, ordered so that the row is written before its entries. Restate H6 in terms of transactions. Define a pin order for heap pages, such as ascending `(segment, page_no)`.

**Fixed (2026-11-27):** Index maintenance is now a separate mini-transaction per index, after the row's, in the same statement and transaction; H6 is restated in terms of the transaction. Heap pages are pinned in ascending `(segment, page_no)` order. Layer §11.4 and A16, heap §4, §4.2, §7.3.

#### V7. The read-committed re-check takes its snapshot before it waits

*Where:* heap §5.2.1 (H13) steps 1–2; layer §6.8.

*Problem:* Step 1 takes a fresh snapshot. Step 2 waits for in-progress holders of the row. A holder that commits during the wait gets a commit timestamp above that snapshot's read timestamp, so step 3 cannot see the holder's version. It returns the version the conflict was already about, and the executor loops or acts on stale data.

*Fix:* Wait first, then take the fresh snapshot, then walk. If the walk meets a new in-progress holder, repeat.

**Fixed (2026-11-27):** The re-check now waits for the holders first, then takes the fresh snapshot, then walks, and repeats if it meets a new in-progress holder. Heap §5.2.1, H13.

#### V8. In version 1 the cleanup watermark ignores followers and read replicas

*Where:* layer §11.7 ("in version 1, the oldest open snapshot in the process"), heap §7.1, heap §7.3; architecture §2.3, §4.5 (`cascade-watermark`), D4.

*Problem:* The managed service has followers and read replicas from version 1, and they serve reads (architecture D4). Prune and vacuum verdicts are carried in the log and replayed on followers (page D2). If the leader prunes against its own process's oldest snapshot, a follower's older snapshot loses versions it can still see. That gives wrong results on replicas. The architecture's `cascade-watermark` includes followers and replicas; the layer document's version-1 shortcut does not.

*Fix:* State that version 1 uses the cross-node watermark in the managed service and the process-local one only in single-box mode. Alternatively, give followers the "snapshot too old" error the product definition already contemplates (§4.1). Make this a dependency of Step 4.

**Fixed (2026-11-27):** The watermark covers the leader, followers and replicas: process-local only in single-box mode, `cascade-watermark` in the managed service with a bounded lag and "snapshot too old" beyond it. Layer §11.7, heap §7.1; register D-12.

#### V9. A rolled-back `TRUNCATE` has already dropped the segment

*Where:* layer §6.12 ("logs the old segment's drop and allocates a new one … a rolled-back `truncate` leaves the table as it was"), layer §10.

*Problem:* The segment-drop record makes the page store discard the segment's files (layer §10). If the drop is logged when `truncate` runs and the transaction then aborts, the old data is gone at the page store although the descriptor still names the old segment.

*Fix:* Log the drop at commit, or as part of the commit record. An aborted truncate then logs only the new segment's allocation, which is harmless. Do the same for `DROP TABLE`.

**Fixed (2026-11-27):** `truncate` now logs the old segment's drop only at commit; an abort logs only the new segment's allocation. The same rule is stated for `DROP TABLE` and `DROP INDEX`. Layer §6.12.

#### V10. Equal keys split across leaves are unreachable under the right-child routing rule

*Where:* btree §2.2 ("child that owns keys in `[k_i, k_{i+1})`"), btree §2.3 (a run may be cut "if one run alone fills a page", and a lookup "follows `next_leaf`"), btree §5.1.

*Problem:* If a run of equal tree keys `k` is cut between leaves L and L′, the parent's separator for L′ is `k`. Under the right-child rule, a lookup for `k` routes to L′ and never visits L. Following `next_leaf` only moves right, so the older part of the run, in L, is never seen. A unique check would miss an in-progress entry, and a scan would miss rows.

*Fix:* Route equal keys to the leftmost child that can hold them, so that descent goes left when the key equals the separator and the scan then moves right. Or forbid cutting a run, and allow overflow pages for a run larger than a page.

**Fixed (2026-11-27):** Descent has two modes: leftmost on equal separators for lookups, scans and existing entries, and rightmost only to insert at the end of a run. A lookup therefore starts at the first leaf of a cut run and moves right. Records name the entry within the run on their page. Btree §2.2, §2.3, §3.2, §8; decision B13.

#### V11. The IOT tree never shrinks, which contradicts the product's queue showcase

*Where:* btree §5.5 (B6, "no merge and no root collapse"); product definition §4.1 ("tables do not bloat", "Showcase workload: PostgreSQL as a job queue").

*Problem:* B6 justifies not merging by saying an IOT "does not shrink the way a heap's does". A job queue is the opposite: rows are inserted with rising keys and deleted within minutes. Purge removes the dead rows but never the leaves, so the left side of the tree becomes an ever-growing run of empty leaves. Every scan for the oldest job crosses them, and the space is never returned. That is exactly the bloat, and the queue workload, that product definition §4.1 promises to beat.

*Fix:* Version 1 needs at least the free-and-unlink of an empty leaf, which is much simpler than a general merge. Or the product definition should drop the queue showcase for version 1. Either way, B6's cost paragraph should name this workload.

**Fixed (2026-11-27):** Version 1 now unlinks empty leaves, which the queue workload produces, leaving a tombstone that scans pass through and whose page is reused only after the cleanup watermark; half-full merge stays out. Btree §5.5, record 28, decision B12; register D-13.

#### V12. The status table has a bounded design the layer document does not consider

*Where:* layer §13.1 ("No sound way is known"); architecture §4.2 (`cascade-page` owns "transaction status pages").

*Problem:* Layer §13.1 says no way is known to bound the status table except stamping through the log. But the architecture already lists *transaction status pages* among the page formats: a persistent, logged status array indexed by transaction id, like PostgreSQL's commit log. With 63-bit ids that never wrap, its size grows by one entry per transaction, not per still-live row. It is paged like any other data, so it never has to fit in memory. That is a real option (c), with its own costs: about 8 bytes per transaction for the commit timestamp, and one page read per uncached lookup. It is missing from the analysis, and from register D-1.

*Fix:* Add it to §13.1 and D-1 and compare the three options on space, lookup cost and recovery. It may also be the cheapest path to V2 (savepoints).

**Fixed (2026-11-27):** Option (c), transaction status pages, is added to layer §13.1 with a comparison on space, read cost, commit cost, recovery and savepoints. It is now the recommendation, with lazy logged freezing to truncate old pages. Register D-1 updated.

### Medium

#### V13. There is no way to wait for a speculative insertion to be aborted

*Where:* layer §6.9 ("Waiters for a key are woken when its delete word lands"), layer §6.5 (`WaitFor` has `Txn` and `SharedLocks`), api §4.2.

*Problem:* The design promises that a transaction waiting on another's speculative key is woken by `abort_speculative`, not by the end of that transaction. But `WaitFor` offers only "until the transaction ends" and "until shared locks are released". PostgreSQL has a speculative-insertion lock for exactly this purpose.

*Fix:* Add `WaitFor::Key(LockKey)`, which the engine signals when it stamps the delete word. Add it to the suite's S8.

**Fixed (2026-11-27):** Added `WaitFor::Key(LockKey)` and `WaitSource::signal(key)`, which the engine calls right after stamping an aborted speculative insert's delete word. Layer §6.5 and §6.9, heap §4.5, api §4.2, suite S8.

#### V14. A sequential scan asserts on `Unused` slots, which are normal after vacuum

*Where:* heap §5.1 ("`Unused` is a bug: assert"), page D1.

*Problem:* Vacuum turns slots `Unused`, and slots are never removed (page D1). A sequential scan walks every slot, so it will meet them in normal operation. An `Unused` slot is a bug only when reached through a locator (heap §5.2).

*Fix:* In the scan, skip `Unused` slots. Keep the assertion in the fetch path only.

**Fixed (2026-11-27):** The sequential scan skips `Unused` slots; only a fetch through a locator asserts on them. Heap §5.1.

#### V15. Speculative abort on an IOT has no deleter word to stamp

*Where:* layer §6.9 point 3 ("stamps the row's deleter and the entry's delete word"), layer §11.3, btree §2.2 (the IOT leaf has no deleter word).

*Problem:* The text applies to the heap. On an IOT the row *is* the primary-key entry, and an IOT delete is a `DELETE_MARK` bit plus an undo record. The IOT path for `abort_speculative` is not described.

*Fix:* State it: a delete-mark and an undo record, in one group, with the waiters of V13 woken.

**Fixed (2026-11-27):** The IOT path is stated: a delete-mark on the leaf row and an undo record carrying the deleting word, in one group with undo first, then the key waiters are signalled. Layer §6.9.

#### V16. The IOT row-lock rule contradicts layer §8 and adds a lookup to every write

*Where:* layer §6.10 ("an IOT write consults the lock manager for its row"), layer §8 item 3 ("Row conflicts do not go through a central lock table").

*Problem:* The two statements contradict each other for the default engine. Every IOT update and delete probes a shared, per-scope lock table, which is the cost A11 cites as the reason to keep heap exclusive locks in the row. That cost is not acknowledged for the IOT, and the lock table is shared by every write core of the scope.

*Fix:* Correct layer §8. Either give the IOT leaf row a lock word, using a spare header field while kind 3 is still unfrozen, or state the cost and make the lock manager's lookup a bounded, sharded structure. Consider having writers skip the lookup when a per-row "has explicit locks" flag is clear.

**Fixed (2026-11-27):** Layer §8 item 3 is corrected to say write conflicts are found in the row and explicit locks in the lock manager. An IOT locker sets a `HAS_LOCKS` row flag, and a heap shared locker a `HAS_SHARED_LOCKS` hint, so writers probe the sharded lock manager only when a flag is set. Layer §6.10, §8; register D-9b.

#### V17. The stamp record's flags can be overwritten by the relink record

*Where:* heap §4.2 (group `[DELETE_MARK(… XMAX_UPDATES_KEY), RELINK(next = …), HEAP_INSERT_V2]`), heap §9 records 11 and 12, layer §6.10 (the flags live in the `next` word's flag field), page D4 (`apply_delete_mark(img, slot, xmax)` takes no flags).

*Problem:* `XMAX_LOCK_ONLY` and `XMAX_UPDATES_KEY` live in the `next` word. Record 11 sets them, then record 12 writes the whole `next` word, which clears them unless the engine copies them in. The page primitive for record 11 has no flags parameter. Three names are also used for one operation: `DELETE_MARK`, `HEAP_STAMP_DELETER` and `apply_delete_mark`.

*Fix:* Specify that `HEAP_RELINK` preserves the two flag bits, or carry them only in record 11's apply and have record 12 write only the pointer and the HOT flags. Use one name.

**Fixed (2026-11-27):** Record 12 now writes only the pointer and `HEAP_ONLY` and preserves the lock flags; record 11 carries the flags. One name: `STAMP_DELETER` / `HEAP_STAMP_DELETER` / `apply_stamp_deleter`, with "delete-mark" kept for the IOT. Heap §4.2, §9; page design corrections and D4.

#### V18. The mini-transaction text says "log first" but the order it describes is page first

*Where:* layer §9.1 ("For each record the log comes first and the page second"; `apply` changes the frame, `seal` later hands the group to the log writer).

*Problem:* As specified, the page changes at `apply` time and the log receives the group at `seal`. The real invariant is weaker and correct: a page cannot be *evicted* or *written* until its records are durable. Saying "log first" will mislead a reader comparing this design with classic write-ahead logging.

*Fix:* Replace the sentence with the actual rule: records are applied to the cached page before the group is sealed, and no page whose LSN is above the durable LSN leaves the cache.

**Fixed (2026-11-27):** Layer §9.1 now states the real rule: records are applied to the cached page before the group is sealed, and no page whose LSN is above the durable LSN leaves the cache.

#### V19. The commit path takes a mutex the architecture says it never takes

*Where:* layer §8 item 2 ("The commit path takes the scope's commit-section mutex"), layer §7 step 9; architecture §4.8 ("a commit … takes no lock").

*Problem:* Layer §8 acknowledges the mutex and redefines the architecture's budget as "the uncontended case". Architecture §1.1 says that when a design changes the architecture, the architecture is updated first. This change is not in layer §13.2's table of proposed changes.

*Fix:* Add it to layer §13.2, or replace the mutex with a design that meets the budget.

**Fixed (2026-11-27):** The commit-section mutex is added to layer §13.2's table of proposed architecture changes, with a note on the lock-free alternative; layer §7 and §8 point to it. Register D-5 item 4.

#### V20. How a read timestamp is chosen is not specified, and the default costs every statement a commit latency

*Where:* layer §6.7 ("A new snapshot, waited for until its read timestamp is closed"), heap §5.2.1.

*Problem:* If each read-committed statement takes `read_ts = now` and waits for the closed timestamp to pass it, every statement waits for every concurrent commit to become durable, about 2 ms at p99 (architecture §6). That is a large regression from PostgreSQL for short statements. Choosing `read_ts` as the current closed timestamp needs no wait, but it then needs the session's read-your-writes token (architecture §4.5) to cover the session's own last commit.

*Fix:* Specify the rule. Recommended: `read_ts = max(closed_ts, session token)`, with a wait only when the token is ahead.

**Fixed (2026-11-27):** `read_ts = max(closed timestamp, session's last commit timestamp)`, waiting only when the token is ahead, and a commit is acknowledged only once the closed timestamp reaches it. Layer §6.7, api §4.3.

#### V21. A follower's tree handle has no way to learn about root splits

*Where:* btree §2.1 and §4 ("both are refreshed when a group that changes them is applied"); architecture §2.3 (a follower applies records only to resident pages).

*Problem:* A follower drops records for pages it does not hold. If the metapage is not resident, a root split never reaches the follower's handle, and every lookup there starts at the old root page, which after the split is only the left child. Keys in the right half are then reported absent. Btree §3.2's root-pointer check compares against that same stale handle, so it does not catch this.

*Fix:* Pin the metapage resident on followers, or have the follower's apply path update the handle from `BT_SPLIT` and `BT_META` records whether or not the page is resident.

**Fixed (2026-11-27):** On a follower, the apply path updates the tree handle from root-splitting `BT_SPLIT` and every `BT_META` record even when the pages are not resident. Btree §2.1, §4; new test.

#### V22. Out-of-line values shared between versions have no lifetime rule

*Where:* heap §8.

*Problem:* A non-HOT update that does not change a large value either copies its chunks, which is expensive, or shares the pointer. If it is shared, "deleting the row delete-marks its chunk rows" will kill chunks a surviving version still uses. The rule must say who owns the chunks.

*Fix:* State that an update copies the pointer and does not delete-mark the chunks, and that chunks die only with the last version that names them. Or copy, and state the cost.

**Fixed (2026-11-27):** Chunks belong to the value: an update that keeps a value copies the pointer and leaves the chunks alone, and the chunks are stamped deleted only when the row is deleted or the value replaced, as PostgreSQL's TOAST does. Heap §8.

#### V23. Sequence ranges must be durable before their first value is used

*Where:* layer §11.5.

*Problem:* "Allocating a range is one log record" does not say that the record must be durable before any value from the range is returned. If it is not, a crash or failover can hand out the same values again, breaking the stated promise ("never reuses a value").

*Fix:* State that allocation waits for durability, and size ranges so that this is rare.

**Fixed (2026-11-27):** Range allocation now waits for its record to be durable before handing out a value; default range 32, growing with use. Layer §11.5; register D-9h.

#### V24. Text keys are compared byte-wise, which breaks ordering for non-C collations

*Where:* layer §5.2 (A7: keys from `cascade-pgtypes`), btree §1 ("Order is bytewise on the normalized key bytes"); architecture §4.2 (`cascade-datum`: "collation-aware string keys"); product definition §5.2.

*Problem:* Today's `encode_text` is byte-wise. An index on a `text` column with any non-C collation would order differently from PostgreSQL, so `ORDER BY` served from an index, and range scans, would return a different order. Uniqueness under case-insensitive types such as `citext` would also differ.

*Fix:* State in A7 that collation-aware keys are a prerequisite of Step 8, or that version 1 indexes support only the C collation. Either way, record it.

**Fixed (2026-11-27):** A7 now states that text indexes support only the `C` collation until `cascade-datum` provides collation-aware keys, and that other collations are refused at `CREATE INDEX`. Layer §5.2 and §4; register D-9g.

#### V25. `TableAccess` reads take `&TxnCtx` but must be able to poison it

*Where:* api §4.3 and §6 (`fetch(&self, …, txn: &TxnCtx, …)`, `seq_scan(&self, txn: &TxnCtx, …)`), layer §6.11 ("`Io` and `Fenced` poison the transaction").

*Problem:* A read that meets an I/O failure must poison the transaction, but it has only a shared reference. And a scan returned by `seq_scan` outlives the call, so its errors cannot reach the context at all.

*Fix:* Make the poison flag interior (a `Cell` inside `TxnCtx`, which is `!Send` anyway), or have the executor poison on any `Io` or `Fenced` error it receives. Say which one.

**Fixed (2026-11-27):** The poison flag is a `Cell` inside `TxnCtx` and `poison` takes `&self`, so reads and live scans can poison through a shared reference. Layer §6.7, api §4.3.

#### V26. `end_statement` returns ids that do not say which table they belong to

*Where:* api §4.3 (`end_statement(&mut self) -> Vec<SpecId>`), api C6.

*Problem:* `abort_unresolved` must route each token to its table, but a `SpecId` carries no relation id. The contract's `SpecToken` does, but L4 cannot name it.

*Fix:* Have `TxnCtx` record `(rel: u64, id: SpecId)` pairs, and have `cascade-access` rebuild the tokens from them.

**Fixed (2026-11-27):** `TxnCtx` records `(RelId, SpecId)` pairs and `end_statement` returns them, so `cascade-access` rebuilds each `SpecToken`. Layer §6.7, api §4.3, suite S8.

### Low

#### V27. An aborted creator has no commit timestamp for the prune rule to compare

*Where:* heap §7.1 ("both resolved … with commit timestamps older than the cleanup watermark").

*Problem:* An aborted transaction has no commit timestamp, so the rule is undefined for the commonest prune case.

*Fix:* Say that a version whose creator aborted is prunable once the abort is resolved, since no snapshot can ever see it.

#### V28. The multi-page pin order is borrowed from the B-tree, where it does not apply

See V6. Heap §4 needs its own pin order.

#### V29. The stated code baseline is out of date

*Where:* layer, the "Code baseline" paragraph.

*Problem:* It says storage M7 is uncommitted. `cascade-redo` and `cascade-pagestore` are on `main` now. It also says the runtime's `txnmgr` holds a status table and a closed timestamp; the code has only a clock (register D-2).

*Fix:* Update the paragraph, and the glossary rows for "Status table" and "Closed timestamp".

#### V30. `CREATE INDEX CONCURRENTLY` is a non-goal without a product statement

*Where:* layer §1.2, layer §6.12; product definition §5.3 and §6.

*Problem:* Migration tools and ORMs emit `CREATE INDEX CONCURRENTLY` routinely. Under this design it would block writes for the whole build, which is an outage on a large table. The product definition does not list this difference in §5.5.

*Fix:* Add it to product definition §5.5, and say what the statement does in version 1: accepted as a blocking build, or refused.

---

## Part 2. Consistency with the product definition and the architecture

Findings above that are about consistency: V2 (savepoints, product §5.3), V8 (followers, architecture D4), V11 (bloat and the queue showcase, product §4.1), V12 (status pages, architecture §4.2), V16 (layer §8), V19 (commit budget, architecture §4.8), V24 (collation, architecture §4.2), V30 (product §5.5). Three more:

#### V31. The page design still contradicts three accepted decisions (Medium)

*Where:* page §2 (B-tree formats "live in cascade-btree"), page "Concurrency, in one paragraph" ("runs serialized on the owning core … message-to-owner reads"), page D1 and its glossary ("vacuum: index entries first, then the slots"), page D2 (glossary "stub").

*Problem:* Layer §16 lists the first two as "owed". They contradict A3, A4 and heap H6, and the page design is still cited as the authority for the heap format. A new reader who starts there, as heap §1 tells them to, learns three wrong things.

*Fix:* Correct the page design now, rather than at its module split. It is a short edit.

**Fixed (2026-11-27):** A corrections block at the top of the page design now overrides the three contradictions (B-tree formats in `cascade-page`, no owning core, vacuum does not wait for index entries), and the sentences, the glossary and D4's primitive names are corrected in place.

#### V32. The architecture still describes the replaced designs (Medium)

*Where:* architecture §4.5 (`cascade-lock`: "Row locks themselves live in version words"), §4.6 (a trait `TableAccessMethod`; `storage-wp5-table-access-api.md`, which no longer exists; "HLD v2 §11–§13", retired), §4.4 (the mini-transaction "appends … then applies").

*Problem:* Layer §13.2 proposes these changes, but the architecture has not been updated, and it is "the document that wins". Its links to retired documents are dead.

*Fix:* Decide register D-5, then update the architecture in the same change.

**Fixed (2026-11-27):** Without waiting for D-5, the architecture's stale passages (§4.4 mini-transaction order, §4.5 row locks and savepoints, §4.6 `TableAccessMethod`, the commit budget, and the dead WP5 and HLD v2 §11–§13 links) now carry the proposed text marked "pending D-5" and point to the layer design. Register D-5 lists the five items.

#### V33. Stale crate descriptions contradict the designs (Low)

*Where:* `cascade-heap/Cargo.toml` ("in-place MVCC rows, undo references"), `cascade-btree/Cargo.toml` ("Optimistic Lock Coupling"), `cascade-page/Cargo.toml` ("checksums"; the CRC is `cascade-storage`'s).

*Fix:* One-line edits. Heap's is already listed as owed at Step 5; it costs nothing to do now.

---

## Part 3. Readability for an outside reader

The documents have improved: there is a glossary, a reading order and a worked write. But an engineer from outside the project still has to stop every few lines. The main problems, with examples:

#### V34. Terms used without definition (Medium)

Missing from the layer glossary, though used throughout: **HOT** (the glossary row for "Chain" says "a HOT chain is one that never leaves its page" but never says what HOT stands for or *when* an update is HOT: no indexed column changed and the new version fits on the page); **IOT** as an acronym; **LSN**, **durable LSN**; **HLC**; **TOAST** ("a toasted column is fetched on demand", layer §6.4); **OpCtx**; **verdict**; **stub**; **replay boundary**; **physiological record** (page §7); **SSI** (`SsiReporter`); **EvalPlanQual**, which the re-check reimplements without naming. The page design uses **WP1 F3**, **WP8**, **M6**, **§9** and **§12 invariant 2**, which all point into retired documents.

*Fix:* Add the missing terms to the glossary, and have the page design adopt layer §2.2's citation convention.

**Fixed (2026-11-27):** The layer glossary gains IOT, HOT, LSN and durable LSN, HLC, subtransaction id and statement horizon, TOAST, `OpCtx`, verdict, stub, replay boundary, physiological record, SSI and EvalPlanQual. The status table and closed timestamp rows now say they are not yet in code. The page design gains a key that maps its retired WP1/WP5/WP8/M6/HLD citations to current ones.

#### V35. Citation density makes paragraphs hard to read (Medium)

*Example:* layer §9.2 has eight citations in four sentences ("architecture §3.2, Rule 5", "heap §9, btree §8", "page design's D4", "layer §13.3"…). Heap §2's H9 paragraph cites "`cascade-redo`'s sequential-fill rule", "`SegmentFill::route`", "storage M8" and "FR-46" before saying what the problem is.

*Fix:* Lead each paragraph with the claim in plain words, and move citations to the end of the paragraph or to a "Sources" line. Cite a decision once per section, not once per sentence.

**Fixed (2026-11-27):** Layer §9.2 and the heap's H9 paragraph now lead with the claim in plain words and move citations to a closing "Sources" line. New text added in this round follows the same pattern.

#### V36. Background is assumed, not given (Medium)

The documents assume the reader knows PostgreSQL's heap internals (HOT, line pointers, `t_ctid`, MultiXact, `HEAP_XMAX_LOCK_ONLY`), InnoDB's implicit locks, ARIES mini-transactions and Aurora's log-is-the-database design. That is fair for a reviewer, but not for the "external reader" the documents now aim at. Two examples:

- Layer §6.10's "Why this split" assumes the reader knows why one 64-bit word cannot list several lockers and what MultiXact's "wraparound emergencies" are.
- Btree §3 says the latch protocol is "optimistic" without saying what the pessimistic alternative (latch coupling) is, or why holding a parent's latch across a page fetch is impossible here (pins cannot be held across an `await`).

*Fix:* Add a one-paragraph "Background" box at the start of each engine document: the prior art in two sentences, and what differs here.

**Fixed (2026-11-27):** Heap and btree each open with a "Background" paragraph (PostgreSQL's heap, HOT and prune; latch coupling against optimistic descent, and why pins cannot cross an `await`). Layer §6.10 now explains why one deleter word cannot list several lockers and what MultiXact's wraparound emergencies are.

#### V37. Each document mixes four kinds of content (Low)

Layer has the contract specification, the rationale, the migration plan and the status of other documents interleaved: §16 is a change log, §12 a schedule, §13.2 an architecture change request. A reader looking for "what must an engine do" has to separate them. At about 1,000 lines, layer is the longest document in the repository after the architecture.

*Fix:* Move §12 (migration), §13 (open questions) and §16 (provenance) into the decision register, or into a separate plan document, and keep the layer document as the specification.

#### V38. There is no picture of the components (Low)

The only diagram in the layer document is a three-line text sketch in §9.1. One diagram of the crates, the three injected seams and the path of a write would replace several paragraphs of §5 and §9.

#### V39. "Kept §" is an odd name for a document of record (Low)

`engine-semantics.md` is the only write-up of the IOT, the default engine, but it is cited as "kept §", which tells a newcomer nothing.

*Fix:* Cite it as "semantics §", and rename the IOT section "IOT semantics (pending detailed design)".

#### V40. Status lines are history, not orientation (Low)

Every document opens with a long status paragraph about which review round changed what, before saying what the document is. Layer's first paragraph is 140 words, mostly provenance.

*Fix:* Open with one sentence on what the document specifies and who should read it, and move the history to the end.

---

## Summary

| Severity | Correctness | Consistency | Readability |
| :--- | :--- | :--- | :--- |
| High | V1, V3, V4, V5, V6, V7, V9, V10 | V2, V8, V11, V12 | — |
| Medium | V13, V14, V15, V17, V18, V20, V21, V22, V23, V25, V26 | V16, V19, V24, V31, V32 | V34, V35, V36 |
| Low | V27, V28, V29 | V30, V33 | V37, V38, V39, V40 |

All High and Medium findings are fixed (2026-11-27). V28 is also covered by V6's fix (a heap pin order). The remaining Low findings are open.

**Suggested order.**

1. V1, V2, V3: they change the contract and the transaction layer, so decide them before the contract crate freezes `Snapshot` (Step 1).
2. V4, V5, V6, V7, V14, V17: the heap paths, before Step 4.
3. V12 together with register D-1, since one decision may answer both.
4. V8, V9, V20, V13, V25, V26.
5. V10, V11, V21: the tree, before Step 6.
6. V31–V33 and V34–V40, while the documents are being edited for the above.

**What is good.** The contract's value-versus-error rule (layer §6.5) is clear and correct. Redo is applied by one function on the leader and on replay (A10), which removes a whole class of drift bugs. The B-tree's optimistic protocol, with `mod_count` stored in the page image, is carefully argued, and the root-pointer check closes a real hole. The decision register now makes the open work visible.
