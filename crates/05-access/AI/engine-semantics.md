# Access engines: semantics of record (kept from HLD v2)

**Status:** Kept document, 2026-11-26. This is the engine semantics of the storage HLD v2 — its §11 (IOT and undo), §12 (heap and vacuum) and §13 (secondary indexes) — copied back into the tree. HLD v2 was retired on 2026-11-22, so until now these sections could be read only through `git show 381ee5b^:crates/02-storage/cascade-storage/AI/storage-subsystem-hld-v2.md`.

**How it relates to the other documents.** The access contract (the layer design, §6) replaces HLD v2 §10 and supersedes two of its statements: "all calls run on the core that owns the scope" (see layer §8) and its trait-shaped sketch (see layer §6.1). The detailed designs refine these sections without contradicting them:

- §1 (IOT) is still the only write-up of that engine. The index-organized engine's own detailed design has not been written; layer §11.3 lists what is already fixed for it.
- §2 (heap) is refined by [`cascade-heap/AI/heap-design.md`](../cascade-heap/AI/heap-design.md). Two of its statements are sharpened there: vacuum does not need to remove index entries before a slot is reused (heap design §7.3), and a row lock is recorded on the row (heap design §4.4).
- §3 (indexes) is refined by layer §11.4 and by [`cascade-btree/AI/btree-design.md`](../cascade-btree/AI/btree-design.md).

**One statement that depends on an open decision.** §2 says "Abort: write the abort record only", relying on the status table to make an aborted transaction's rows invisible. That holds only while status entries can be kept for as long as a page word names them; layer §13.1 explains why that cannot hold forever and what has to be decided.

Terms are defined in the glossary of the layer design (§3).

---

## 1. IOT engine and undo (HLD v2 §11)

**Responsibility.** Store rows in a primary-key-clustered B-tree with the newest version in the leaf, and keep older versions in undo for MVCC reads and rollback.

**Key decisions**

* Every IOT row has a primary key. The row lives in the leaf of the primary-key B-tree.
* The leaf holds the **newest** version (committed or not). Updates are in place.
* Row header: version word, `roll_ptr` to the previous version's undo record, flags (delete-mark, has-TOAST).
* **Undo records are deltas:** the prior values of changed columns, the prior version word, and the prior `roll_ptr`. Records of a row form a chain from newest to oldest.
* Undo lives in **undo pages** in the same scope. Undo pages go through the page cache and every change to them is WAL redo, so the page store materializes them like any other page. Undo is never written separately.
* Operations:
  * Insert: new leaf row + an insert-undo record (rollback removes the row).
  * Update: write a delta undo record, then overwrite the leaf row in place.
  * Delete: set delete-mark + undo record. Physical removal waits for purge.
  * Primary-key update: delete-mark the old key + insert the new key. The locator moves.
* Read: if the leaf version is not visible to the snapshot, walk `roll_ptr` and apply deltas until a visible version (or none) is found.
* Rollback: walk the transaction's undo chain, restore each row, log a compensation record per step, then write the abort record.
* **Purge:** removes delete-marked rows and frees undo once no snapshot can need them. Undo is truncated at **segment** granularity: a segment is freed when all transactions in it are resolved and older than the cleanup watermark.

**Invariants**

* The leaf row plus its undo chain can reconstruct every version that any active snapshot may read.
* At most one uncommitted version per row (the leaf), owned by the transaction in its version word.
* An undo record is durable (in the WAL) before or with the leaf change that depends on it.
* Purge never frees undo newer than the cleanup watermark.

**Open questions (carried to the index-organized engine's detailed design)**

* B-tree structure-change atomicity and latching, and how structure-change redo stays page-local. The tree mechanism's design answers the mechanism half (btree design §3, §5); the IOT side is still open.
* Undo segment layout (per transaction, per core, or shared) and how recovery finds in-flight transactions' undo without a checkpoint.
* Long-running readers holding back purge (cleanup watermark across scopes and replicas).
* Row size limit before TOAST kicks in.

## 2. Heap engine and vacuum (HLD v2 §12)

**Responsibility.** Store rows as append-only versions in heap pages, with indexes separate (PostgreSQL-like), and reclaim dead versions incrementally.

**Key decisions**

* Tuple header: creator version word, deleter version word (empty if live), next pointer to the newer version, flags (HOT-updated, heap-only, has-TOAST).
* Insert: new tuple. Update: new tuple + set the old tuple's deleter word and next. Delete: set the deleter word.
* **HOT:** if no indexed column changed and the new version fits on the same page, it is a heap-only tuple chained from the old one. No index entries are written; indexes keep pointing at the chain root.
* Abort: write the abort record only. Aborted tuples are dead by status lookup and removed by pruning or vacuum. (See the note above.)
* **Opportunistic prune:** when a page is accessed and is low on space, remove dead versions older than the cleanup watermark and collapse HOT chains. It is a WAL redo record like any page change.
* **Background vacuum:** per page and incremental, never a table-wide stop. It removes dead tuples, removes their index entries, and updates the free space map.
* **No freeze and no wraparound vacuum.** Transaction ids are 63-bit per scope and the commit timestamp is a hybrid logical clock, so nothing wraps.
* Free space map and any heap bookkeeping pages are ordinary WAL-logged pages.

**Invariants**

* A tuple is removed only when its deleter (or its aborted creator) is older than the cleanup watermark.
* An index entry pointing at a tuple id is removed before or with that slot's reuse, so a slot is never reused under a live index entry. (Heap design §7.3 replaces the mechanism: with the cleanup watermark below every entry's delete word, the slot may be freed first.)
* A HOT chain never crosses a page.

**Open questions (carried to the heap design)**

* Vacuum scheduling and how it learns the cleanup watermark across scopes and replicas.
* Whether index cleanup during vacuum can rely on index entries' own delete timestamp instead of tuple-id lookups. (Heap design §7.3: it can.)
* Unlogged and temporary heap tables (the non-logged path).

## 3. Secondary indexes and index MVCC (HLD v2 §13)

**Responsibility.** Provide B-tree secondary indexes for both engines where each entry carries its own visibility, so an index scan can decide visibility without visiting the row.

**Key decisions**

* Entry = (index key, locator, create word, delete word). Locator = primary key for IOT tables, tuple id for heap tables. Create and delete words use the same version-word format as rows.
* For a heap table the primary-key index is just a unique secondary index. For an IOT table the primary-key B-tree is the table itself and versions live in the row + undo, not in index entries.
* Insert row: insert entry with create word = writer's transaction id (uncommitted).
* Update that changes an indexed column: set the delete word on the old entry, insert a new entry. Update that changes no indexed column: no index write (HOT for heap; same primary key for IOT). (For a heap update that could not be HOT, heap design §4.2 writes new entries anyway.)
* Delete row: set the delete word on every entry of the row.
* Visibility: an entry is visible if create is visible and delete is not.
* Unique check: a key conflicts if any entry with that key is visible-live in the latest state or uncommitted by another transaction. Uncommitted-by-another leads to `Conflict::InProgress`; committed-live leads to `UniqueViolation`.
* Index entries are cleaned up when their delete word (or an aborted create) is older than the cleanup watermark, by index page pruning or vacuum.
* Sharded tables (pending confirmation): local secondary indexes only; unique only if the key includes the shard key.

**Invariants**

* For every row version visible at a read timestamp there is exactly one visible index entry per index pointing at it (or at its HOT chain root).
* An index entry never becomes visible before the row version it points to.
* At most one uncommitted change per entry, owned by the transaction in its version word.

**Open questions (index MVCC costs)**

* Cost of unique checks when many versioned entries share a key.
* Write cost of setting the delete word on the old leaf for every indexed-column update and delete.
* Deferred index writes (buffer index changes until commit or into the page store) and how they interact with unique checks.
