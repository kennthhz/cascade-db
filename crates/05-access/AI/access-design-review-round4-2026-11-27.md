# Access layer design review, round 4 (2026-11-27)

**What this is.** A fourth adversarial review of the design documents under `crates/05-access`, written as an outside senior principal engineer would write it. It covers the versions in PR #5, which applied the fixes for round 3 ([`access-design-review-2026-11-27.md`](access-design-review-2026-11-27.md)). At the owner's request it reports **only high-impact issues**: designs that would return wrong results, lose a lock or a guarantee, hang, or break a product promise if they were built as written. Readability and low-severity points are left out on purpose.

**Documents reviewed.** The layer design, the heap design, the btree design, the access-api design, the page design and the decision register, all at commit `b882b75` on `claude/access-review-fixes`. Citation prefixes follow layer §2.2.

**Fix status.** All nine findings were fixed on 2026-11-27, each marked below with **Fixed** and a one-line description. The choices that need the owner's ratification are in the [decision register](decision-register.md): D-1 (recommendation reversed), D-9b, D-12, D-13 and the new D-14.

**Summary.** There are nine findings, W1–W9. Five of them come from the round-3 fixes themselves, or from how those fixes interact (W2, W4, W5, W8, W9). Three are older defects that earlier rounds missed (W1, W3, W6). One is a gap that remained in a round-3 fix (W7). The two that most need attention are **W1**, where a B-tree insert can retry forever on ordinary data, and **W2**, where a statement can commit with only part of its index work done, which leads to wrong query results once vacuum runs.

---

## Findings

### W1. A B-tree insert can retry forever when its key falls in a gap

*Where:* btree §3.3, in the writer's split check: `if leaf is not the rightmost leaf and key > leaf.last_tree_key: unpin; continue`.

*Problem:* The check is meant to notice that the leaf was split after the descent chose it. But it compares the key with the last key *present* in the leaf, and that is not the leaf's upper bound. Take a leaf L holding keys 10 and 20, whose parent separator for the next leaf is 50. Inserting 25 routes to L, which is correct. But 25 > 20 and L is not the rightmost leaf, so the writer unpins and starts again, reaches L again, and loops forever. Purge-compact produces exactly this shape all the time. A queue deletes the upper keys of a leaf, and an empty leaf that B12 cannot unlink, because it is its parent's only child, has no last key at all. So the round-3 queue fix (V11) makes the case more common, not less.

*Impact:* Inserts hang on ordinary data. This is the default engine's write path.

*Fix:* Give every leaf a **high key** (an upper fence, as in Lehman–Yao's B-link tree), set by the split that creates it and kept by purge. The writer then checks `key >= leaf.high_key`. Alternatively, the writer can validate the leaf's own `mod_count` against the copy taken during the descent, since a split changes it. In both cases, drop the comparison with `last_tree_key`, and add a test that inserts into the gap left by purge.

**Fixed (2026-11-27):** Every leaf but the rightmost now carries a high key (an upper fence) set by splits and unlinks and kept by purge, and the writer checks its key against that bound instead of the last key present. Btree §2.2, §3.3, decision B14, and a purge-gap test.

### W2. A write that stops after its row step leaves a partial write that commits

*Where:* layer §11.4 (A16: each index change is its own mini-transaction, after the row's), layer §6.5 and §6.11 (conflicts and unique violations are values), heap §4.2 and §4.3, heap §7.3 (H6).

*Problem:* Since round 3, a heap update or delete writes the row first and then each index in turn. Each index step can end in a value rather than an error. A unique index returns `UniqueViolation`, or `Conflict::InProgress` once the engine's wait bound expires, and under `DontWait` it returns `Conflict::InProgress` at once. The contract tells the executor that a value means it "can still decide what happens to this row", for example by skipping it. If the executor skips the row, the statement goes on and commits with:

- an old version stamped deleted whose remaining index entries never received their delete words. H6 then frees the slot while live entries still point at it, and a later reader follows such an entry into whichever row reuses the slot. That is a wrong result;
- a new version with index entries in only some of its indexes, so index scans miss the row.

Subtransaction ids do not save this, because the sub-id is per statement and the statement goes on.

*Impact:* Silent wrong results after vacuum, and rows missing from index scans.

*Fix:* Choose one of these.

- (a) Run every unique check before the row's mini-transaction, as PostgreSQL's speculative path does, so that no index step after the row can conflict.
- (b) Declare that any non-`Done` outcome from an index step after the row step is an error, `TamError::StatementFailed`, which forces the executor to roll back the statement's subtransaction.
- (c) Give each row operation its own subtransaction id, so that the executor can abort just that row.

Option (b) is the smallest change and matches PostgreSQL, where a unique violation inside `UPDATE` raises an error. State the rule in layer §6.5, and add a suite case in which an update's second index conflicts.

**Fixed (2026-11-27):** An index step after the row step now always waits, whatever the `WaitPolicy`, and a unique violation or expired wait there is `TamError::StatementFailed`. That error marks the transaction failed, and it cannot commit until it rolls back to a savepoint (aborting the statement's sub-id and its partial write) or aborts, as in PostgreSQL. Layer §6.5, §6.11, §11.4, A17; heap §4.6; api §4.3, suite S7; register D-14.

### W3. Rolling back to a savepoint silently releases an earlier exclusive row lock

*Where:* layer §6.6 (savepoints through subtransaction ids), layer §6.10 (exclusive locks live in the row's deleter word), heap §4.4.

*Problem:* Transaction A runs `SELECT … FOR UPDATE` in statement 1, so the deleter word holds sub-id a1 with `XMAX_LOCK_ONLY`. A then sets a savepoint and updates the same row in statement 2, which overwrites the deleter word with a2. A then runs `ROLLBACK TO SAVEPOINT`, which marks a2 aborted. The row's only lock word now names an aborted id, so the row looks unlocked, although A still holds its `FOR UPDATE` from before the savepoint. Another transaction can then update or delete it. PostgreSQL had this exact bug until 9.3 and fixed it by keeping both the locker and the updater (MultiXact). The design has one deleter word and no fallback.

*Impact:* An application's explicit row lock is lost. The inconsistency stays silent, and job queues that use `FOR UPDATE SKIP LOCKED` together with savepoints are exactly where it would bite.

*Fix:* When a write overwrites a lock word that names an earlier subtransaction of the same top-level transaction, first move that lock into the lock manager, so the lock has a second home that survives the rollback. Alternatively, a deleter word whose sub-id was rolled back can fall back to a lock-manager entry recorded at the overwrite. Add a suite case: lock, savepoint, update, roll back, and then a second transaction must still be blocked.

**Fixed (2026-11-27):** A write that overwrites a lock word naming an earlier subtransaction of its own transaction first records that lock in the lock manager and sets the row's `HAS_MANAGER_LOCKS` hint, in the same group, so the lock survives the rollback. Layer §6.10, A18; heap §4.4; suite S6.

### W4. Lazy freezing breaks the round-3 chain-hop check

*Where:* heap §5.2 (V5: hop only if `creator(succ)` equals `deleter(this)`, compared as exact words), layer §13.1 and register D-1 (the new recommendation: freeze old words to commit timestamps lazily when prune or vacuum touches a page).

*Problem:* A cross-page update stamps v1's deleter and v2's creator with the same uncommitted word. Freezing rewrites the words on one page at a time, whenever that page happens to be pruned, so v1's deleter can be frozen to `committed(ts)` while v2's creator still reads `uncommitted(x)`, or the other way round. The exact comparison then fails, the walk treats the link as stale, and the row disappears for every reader that reaches it through v1's Tid. That includes every index entry made before the update, if the update was HOT on v1's side.

*Impact:* Rows vanish from fetches and index lookups. The failure comes from combining two round-3 fixes, V5 and V12.

*Fix:* Compare *identities*, not raw words. Either never freeze a deleter word whose `next` link crosses pages until its successor is frozen too, or let the hop check accept `uncommitted(x)` against `committed(ts)` when `status(x)` resolves to `Committed{ts}`. Status pages keep that answer for as long as any unfrozen word exists, and that is exactly the condition under which the check is needed. State the rule in heap §5.2, and add the case to the chain property test with freezing interleaved.

**Fixed (2026-11-27):** The round-3 word comparison is gone. A pure delete now clears `next` in its stamp record, so the walk only ever hops from a version whose visible deleter is an update, which wrote the link itself. A debug assertion compares resolved identities. Heap §5.2, record 11, H10.

### W5. Bounding the status pages needs the table-wide freeze the product promises to avoid

*Where:* layer §13.1 (recommendation (c), with old status pages truncated "below the oldest unfrozen id"), heap §7.3 ("There is no index work, no table-wide pass and no freeze"), product definition §4.1 ("tables do not bloat", no vacuum emergencies).

*Problem:* Truncating status pages below an id requires proof that no word anywhere still names an id below it. Prune and vacuum touch only pages that hold dead versions. A cold page whose rows never change is never touched, so its uncommitted words are never frozen. Index entries carry version words too, and nothing in the design freezes index pages at all. So the "oldest unfrozen id" never advances unless something periodically scans *every* heap page and *every* index page. That is PostgreSQL's anti-wraparound vacuum, which the product sets out to avoid. Without such a scan the status pages grow forever, at about 16 bytes per transaction id. At 10,000 writing statements per second (each gets a sub-id since round 3) that is about 14 GB a day.

*Impact:* Either unbounded growth, or a table-wide maintenance pass that contradicts heap §7.3 and the product's promise. D-1's recommendation was made without this cost.

*Fix:* Put the full cost in the layer §13.1 comparison. Options: a background freeze pass with a bounded budget, made a stated product behavior; or option (a), stamping at commit, which needs no scan, at the price of one record per touched row; or status pages kept forever, with tiering to cold storage. Count index pages in every option. Reopen D-1 with this added.

**Fixed (2026-11-27):** Layer §13.1 now states the cost of truncating status pages: a pass over every heap and index page. The recommendation is reversed to post-commit background stamping, with status pages as the bounded store for transactions whose stamps are pending; no whole-table pass is needed. Register D-1 updated.

### W6. Snapshots can change under a reader: commit publication is not ordered with the closed timestamp

*Where:* layer §7 step 9 (commit: append, wait for durability, then mark the status table), layer §6.7 (round 3: `read_ts = max(closed_ts, token)`), layer §6.6 (subtransactions commit "with" their top), access-api §4.3.

*Problem:* There are two gaps.

1. **Publication against the closed timestamp.** A snapshot at `read_ts` must classify every word the same way for its whole life. If the closed timestamp can pass a commit's `ts` before the status table says `Committed{ts}`, a reader first sees `InProgress` (not born), and later, within the same snapshot, `Committed{ts ≤ read_ts}` (born). That breaks repeatable read, and a scan can emit a row that an earlier fetch reported absent. Nothing in the documents orders "mark committed" before "advance the closed timestamp".
2. **Subtransaction commit is not atomic as written.** "A commit commits the top-level transaction and every subtransaction of it that is not aborted." If each sub-id's entry is flipped to `Committed` one by one, a reader can see the top committed but some sub-ids still in progress. A transaction would then be partly visible.

*Impact:* Non-repeatable reads inside one snapshot, and partly visible transactions. Both break PostgreSQL's isolation guarantees.

*Fix:* (1) Make it an invariant of `cascade-txn-api` that the closed timestamp passes `ts` only after every commit at or below `ts` is published in the status table, and test it in S1/S2 with the scripted world. (2) Resolve a sub-id through its top: a sub-id entry stores only `{top, aborted?}`, and `status(sub)` returns the top's state unless the sub-id itself was rolled back. A commit then flips one entry. Change `TxnStatus` accordingly in layer §6.6 and access-api §4.1.

**Fixed (2026-11-27):** A subtransaction's entry holds only `{top, rolled_back}` and resolves through its top, so a commit flips one entry. The closed timestamp passes `ts` only after every commit at or below `ts` is published, which is now an invariant of `cascade-txn-api`. Layer §6.6, api §4.3.

### W7. The replica-aware watermark still ignores snapshots a follower has not taken yet

*Where:* layer §11.7 and register D-12 (round 3: the watermark collects "each follower's and replica's oldest snapshot"), heap §7.1.

*Problem:* A follower takes new snapshots at its *own* closed timestamp, which lags the leader's. Suppose the leader computes the watermark W from the snapshots it currently knows about, then prunes and logs the verdict at LSN p. A follower that has applied only up to q < p can still open a new snapshot with `read_ts` < W. When it later applies the prune at p, that snapshot loses versions it is entitled to see. Collecting existing snapshots is not enough. The watermark must also stay at or below every follower's *future* snapshot floor.

*Impact:* Wrong results on followers and read replicas, the D4 configuration of the managed service. This is the hole V8 meant to close.

*Fix:* Define the watermark as at most the minimum, over followers and replicas, of `min(oldest active snapshot, applied closed timestamp)`. Alternatively, have a follower refuse, with "snapshot too old", a new snapshot below the watermark carried by any prune record it has not yet applied, which means the records carry W. State the rule in layer §11.7 and D-12.

**Fixed (2026-11-27):** The watermark is now at most the minimum, over leader, followers and replicas, of `min(oldest open snapshot, applied closed timestamp)`, which covers snapshots a follower has not taken yet. Layer §11.7, register D-12.

### W8. The round-3 lock hint bits have races that lose locks

*Where:* layer §6.10 (V16: the IOT `HAS_LOCKS` bit, "cleared lazily by the next writer that finds no holder"; the heap `HAS_SHARED_LOCKS` hint, set by a logged one-bit record "the first time the row is share-locked"), layer §8 item 3.

*Problem:* The bits were added so that writers can skip the lock manager. But the documents never say how a locker's bit and lock-manager entry are ordered against a writer's check and write.

- *Heap, setting the bit:* a shared locker registers in the lock manager and then sets the bit in a separate mini-transaction. A writer that runs in between reads the bit as clear, skips the lock manager, and updates a row that is share-locked.
- *IOT, clearing the bit:* a writer finds the bit set, looks in the lock manager, finds no holder, and clears the bit. Meanwhile a locker registers and, seeing the bit already set, does not set it again. The bit is now clear while a lock is held, and the next writer skips the lock manager.

*Impact:* `FOR SHARE` and `FOR KEY SHARE` locks, which foreign-key checks rely on, are ignored. The result is referential-integrity violations, and possibly orphaned child rows.

*Fix:* Specify the protocol. Under the row page's exclusive pin, a locker sets the bit (or confirms it is set) and then registers in the lock manager while still holding the pin. A writer reads the bit and consults the lock manager under the same pin, before it stamps. Only a writer that holds the pin and has seen no holder may clear the bit, and a locker always re-checks under the pin. Add a `loom` test, as architecture Rule 11 requires for shared structures.

**Fixed (2026-11-27):** A protocol is specified: lockers register in the lock manager and set the hint while holding the row page's exclusive pin; writers check the hint and consult the manager under the same pin before stamping; only a writer holding the pin that has just found no holder may clear an IOT hint. To be model-checked with `loom`. Layer §6.10, A18.

### W9. Reusing tombstone pages breaks A6 and the B-tree's validation arguments

*Where:* btree §5.5 (B12: a tombstone's page number "is not reused until the cleanup watermark has passed the unlink"), btree §3.1 ("a node's count never repeats"), btree §3.2 ("a root pointer that returns is the same root, because page numbers are never reused"), layer A6 ("Ids are never reused").

*Problem:* B12 brings in page reuse, which A6 forbids and on which two of the tree's safety arguments depend:

- When a reused page is formatted as a new node, nothing says what its `mod_count` starts at. If the count restarts, a reader that copied the old page with count c can see c again after the reuse, accept a stale path, and descend into the wrong subtree.
- A reused number can become a new root, which reopens the root-pointer ABA case that §3.2 closed.
- The guard is the cleanup watermark, which bounds *snapshots*. But B-tree descents also run without a snapshot: purge, index vacuum, structure changes, and a follower's apply-time handle updates. The watermark does not hold any of them back.

*Impact:* Readers can be misrouted, which means missed or wrong rows from the default engine and from every index.

*Fix:* Either never reuse page numbers, as A6 says, and let the page store release the storage of tombstones; 2³² pages per segment is 32 TiB of churn before the segment must be rebuilt, so state that limit and the rebuild. Or keep reuse but (i) carry `mod_count` forward, so a reused page's first image has the old count plus one; (ii) never reuse a tombstone as a root; (iii) gate reuse on an epoch that every descent pins, not on the snapshot watermark. Update A6 or B12 so that they agree.

**Fixed (2026-11-27):** Page numbers are never reused, as A6 requires. The page store releases a tombstone's storage, and a tree that exhausts its 2³² page numbers through churn (about 1.3 years at 10,000 queue jobs a second) is rebuilt online, which is added as an open item. Btree §5.5, §10, B12; register D-13.

---

## What is good

The round-3 fixes for V1/V2 (subtransaction ids) and V3 (the latest-state unique check) are the right mechanisms. W3 and W6 sharpen them rather than replace them. The HOT-root derivation (V4) is sound as written. Moving index maintenance into separate mini-transactions (V6) was necessary, but it needs W2's rule to be safe.

## Suggested order

1. **W1 and W2** before any code: W1 is a hang in the default engine, and W2 is silent corruption.
2. **W6**, before the contract crate freezes `Snapshot` and `TxnStatus` (Step 1).
3. **W3 and W8**, with A11, before Step 4.
4. **W4 and W5**, with D-1, before D-1 is decided.
5. **W7**, with D-12. **W9**, with D-13, before Step 6.
