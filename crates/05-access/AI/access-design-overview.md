# The access layer at a glance

**Who this is for.** A principal engineer from outside the project who wants to know, in about ten minutes, how Cascade DB stores and reads table rows. It summarizes the design as of PR #5 (with the round-3 fixes) and PR #6 (the round-4 review). The detailed documents are the [layer design](access-layer-design.md), the [heap](../cascade-heap/AI/heap-design.md), [B-tree](../cascade-btree/AI/btree-design.md), [page](../cascade-page/AI/storage-page-mutation-design.md) and [access-api](../cascade-access-api/AI/access-api-design.md) designs, and the [decision register](decision-register.md).

## 1. Context

Cascade DB is a PostgreSQL-compatible database. Clients speak the PostgreSQL wire protocol and SQL, but the storage engine is new. Three facts about the wider system shape everything below:

- **The log is the database.** Compute nodes never write data pages to disk. Every page change is described by a redo record, and a separate *page store* builds the durable pages by replaying the log, much as Aurora does.
- **Single writer per scope.** A *scope*, which in version 1 is one database, has one leader node that writes it and followers that serve reads. Inside the leader, a scope is a shared-memory engine that any of its assigned CPU cores may read or write.
- **Thread-per-core runtime.** Sessions are async tasks pinned to a core. Nothing may hold a page latch across an `await`, because a suspended task would block every other session on that core.

The **access layer** (L5) sits between the SQL executor above and the page cache and log below. It owns tables, indexes, row versions, row locks and cleanup.

## 2. The parts

| Part | What it is |
| :--- | :--- |
| `cascade-access-api` | The contract the executor calls: insert, update, delete, fetch, scans, row locks, upsert. Conflicts and unique violations come back as *values*; only fates already decided for the transaction (I/O failure, deadlock, lost ownership) are *errors* |
| `cascade-access` | `AnyTable`, an enum with one arm per engine. The executor dispatches through it; a test-only trait lets one conformance suite run against every engine and an in-memory fake |
| `cascade-txn-api` (L4) | What the access layer needs from transactions: `Snapshot`, the visibility rule, the status table interface, the wait interface, and the per-transaction context |
| **IOT engine** (default) | Index-organized table: rows live in the leaves of a B-tree ordered by primary key. Updates happen in place, and older versions go to undo, as in InnoDB or Oracle |
| **Heap engine** | For tables without a primary key, or that need PostgreSQL heap behavior: append-only row versions, HOT updates, prune and vacuum |
| Temporary tables | Session-private, in memory, unlogged |
| `cascade-btree` | One transaction-blind B+tree, used by the IOT and by every secondary index |
| `cascade-index` | Secondary indexes. Each entry carries its own visibility words, so index-only scans need no visibility map |
| `cascade-page` | The byte layout of every page kind (heap, B-tree, free-space map, undo) and the functions that apply redo to them |

## 3. Four core mechanisms

**Version words and visibility.** Every row version, and every index entry, carries a creator word and a deleter word, each 64 bits. A word is either a commit timestamp from the scope's hybrid logical clock, or an in-flight transaction id (top bit set). A snapshot is a read timestamp plus "who am I". A version is visible if its creator committed at or before the read timestamp, or was written by an earlier statement of the reader's own transaction, and its deleter is not. Words that name in-flight ids are resolved through the status table. Ids are 64-bit and never wrap, so there is no PostgreSQL-style wraparound.

**Statements and savepoints (subtransaction ids).** Each writing statement, and each savepoint, stamps a fresh subtransaction id that the status table links to its top-level transaction. A statement ignores own words at or above its *horizon* (the next id when it began), so `UPDATE t SET v = v + 1` never re-updates its own output. `ROLLBACK TO SAVEPOINT` marks the sub-ids written since the savepoint as aborted, which works even on the heap, which has no undo.

**Mini-transactions: the only way to change a page.** An engine never writes page bytes. It builds redo records and hands them to a seam owned by the page cache. The seam reserves log positions, applies the records to the cached page with the same function the page store uses for replay, then seals the group and passes it to the log writer. The leader, the followers and the page store therefore run identical code on identical bytes. A page cannot leave the cache before its records are durable. Pins are short, page-wide and never held across an `await`, so the B-tree uses optimistic descent: copy each page, then validate version counters instead of holding parent latches.

**Waiting and locks.** Write–write conflicts are found in the row's version words. If the holder is still in flight, the engine sleeps on an injected wait interface implemented by the lock manager, which also detects deadlocks. On the heap, exclusive row locks (`FOR UPDATE`, plus every update and delete) are stamped into the deleter word with a "lock only" flag. Shared locks (`FOR SHARE`, `FOR KEY SHARE`, used by foreign-key checks) live in the lock manager. On the IOT, all row locks live in the lock manager, behind a per-row flag so that unlocked rows cost no lookup. Upsert uses PostgreSQL-style speculative insertion.

## 4. A write, end to end

`INSERT` into a heap table: the engine encodes the row with creator = its subtransaction id, picks a page, pins it, and applies one insert record through the seam. Each index then gets its entry, as a separate mini-transaction in the same statement. Other sessions see the row as in flight and skip it. `COMMIT` appends a commit record, waits for durability and marks the status table. Nothing on the data page changes at commit. Rollback changes nothing either: the status table says aborted, and prune later reclaims the row.

## 5. Cleanup

A *cleanup watermark* is the oldest read timestamp any reader, on the leader, a follower or a replica, may still use. Heap *prune* removes dead versions within one page when the page is accessed. *Vacuum* frees slots without waiting for index cleanup, which is safe because every index entry is stamped deleted by the same transaction. IOT purge removes delete-marked rows and old undo. The B-tree unlinks leaves that become empty, so the job-queue workload does not bloat, but it does not merge half-full leaves. All cleanup decisions are logged as verdicts, so replicas apply them rather than recompute them.

## 6. Where it stands

- **Code.** The heap page format, the shared buffer pool, and the redo path for one record type exist. The contract crates, the engines, the status table, the lock manager and the mini-transaction seam do not exist yet. The runtime's transaction manager is only a clock.
- **Decisions.** Almost every design decision is still *Proposed*. Kenneth has accepted the three-crate split, the concrete transaction context, the handling of speculative tokens, and the placement rule for type-5 inserts. The open owner decisions are in the decision register, D-1 to D-13. The most consequential is D-1: how in-flight words become self-describing, so that the status table stays bounded.
- **Review status.** Four adversarial review rounds have run. Every High and Medium finding of round 3 and all nine high-impact findings of round 4 are fixed in the documents. Round 4's fixes added a leaf high key to the B-tree, made a failure after a write's row step an error that blocks commit, kept earlier locks across savepoint rollbacks, and changed the D-1 recommendation to post-commit stamping.

## 7. Terms

**HOT** ("heap-only tuple"): a heap update that changes no indexed column and fits on the same page, so it adds no index entries. **Tid**: a heap row address, as (page, slot). **Locator**: the executor's opaque handle on a row: a Tid on the heap, the primary key on the IOT. **Undo**: an IOT row's earlier versions, stored as deltas chained from the leaf. **Status table**: transaction id → in progress, committed at a timestamp, or aborted. **Closed timestamp**: the time below which no new commit can appear, so a snapshot read at or below it is stable. **LSN**: a position in the log. **Redo record**: a self-contained description of one change to one page.
