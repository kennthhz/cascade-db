# cascade-heap design — the heap engine

**Status:** Draft v0.2 for review, 2026-11-26, amended 2026-11-27 for the round-3 review ([`access-design-review-2026-11-27.md`](../../AI/access-design-review-2026-11-27.md): V2, V4–V8, V13, V14, V17, V22, V35, V36). v0.2 applies the review of v0.1 ([`access-design-review.md`](../../AI/access-design-review.md)): the chain walk, row locks, speculative abort, the re-check, vacuum's treatment of indexes and the free-space map are corrected (R3, R4, R5, R10, R11, R21, R24, R27).

**What this document is.** The detailed design of `cascade-heap`, the PostgreSQL-style engine behind the `AnyTable::Heap` arm: row versions appended to pages, HOT chains, incremental prune and vacuum. The *format* half is the page design's — the heap page, its slot states, its prune plans and its redo primitives (page D1–D6), already pinned by golden tests. This document is the *engine* half: the DML paths, the visibility walks, free space, vacuum, out-of-line storage, and the landing sequence.

**Background for readers new to PostgreSQL's heap (round-3 fix V36).** PostgreSQL stores a table as unordered pages of *tuples* (row versions). Each page has an array of *line pointers* (here, slots) so that a row's address, its Tid, stays fixed while the bytes move within the page. An update never overwrites a tuple: it writes a new version and links the old one to it (PostgreSQL's `t_ctid`, here the `next` word). Each version carries the id of the transaction that created it (`xmin`) and the one that deleted or replaced it (`xmax`); a reader decides which version it sees from those two words alone. When an update changes no indexed column and the new version fits on the same page, it is a *HOT* ("heap-only tuple") update: no index entry is added, and readers reach the new version through the old one's link. Dead versions are removed in two steps: *prune*, a quick per-page cleanup on access, and *vacuum*, which frees slots. **What differs here:** version words are commit timestamps or in-flight transaction ids rather than 32-bit transaction ids, so there is no wraparound and no freezing for correctness; index entries carry their own visibility words, so there is no visibility map; every page change is a logged mini-transaction applied by the same redo function the page store runs; and there is no undo.

**Upstream:** the [layer design](../../AI/access-layer-design.md) — §6 (the contract), §7 (a worked write), §8 (execution model), §9 (the write path; §9.1 defines the mini-transaction), §11.2, §13.1 (a blocking dependency). The storage design's §12.1–§12.6. The [kept document](../../AI/engine-semantics.md) §2. The page design. Citation prefixes are defined in layer §2.2; terms in layer §3.

**Depends on:** the landed page format; the cross-core buffer pool; `cascade-redo`'s sequential-fill rule (heap H9); the contract (layer §6); the mini-transaction seam (layer §9.1); `cascade-index`, which lands at Step 8 and is this engine's only dependency on another engine.

---

## 1. Scope

The page design owns the bytes: the heap page layout, the slot states (page D1: live, redirect, dead, unused), the plan-carried prune (page D2), compaction (page D3), the redo primitives (`apply_heap_insert`, `apply_stamp_deleter`, `apply_relink`, `apply_prune`, `apply_slot_free`; page D4), and the traversal vocabulary (page D5). This engine obeys those bytes. A format change is the page design's to make.

This document says who calls each primitive and when, what the engine computes as against what it merely applies, the relation-level structures, the DML paths, and the landing sequence.

**Two things this engine is not.** It makes no in-place updates: every update writes a new version. It keeps no undo: rollback is the abort record plus the status table (kept §2, and layer §13.1 for what that costs). `ROLLBACK TO SAVEPOINT` works the same way: every word written since the savepoint names a subtransaction id that the rollback marks aborted (layer §6.6, round-3 fix V2), so those versions read as an aborted transaction's do, with nothing on the page undone. The row header's third word is the forward chain pointer, not an undo reference (format F2).

---

## 2. Relation Structure (H1)

A heap relation has one **main segment** and, once out-of-line storage lands, one hidden segment for it (§8). Pages are `(segment, page_no)` inside the scope. A sequential scan walks pages in `page_no` order from 0 to the segment's extent (`ScopePages::extent`, layer §10). There is no page chain: the segment's order is the scan's order, which also makes the storage layer's sequential page reads the natural way to fill a miss.

**Segment allocation** goes through `ScopePages` (layer §10): a new segment when the relation is created and at `truncate`, a page hint for the tail, and `extend` to grow.

### Placement while the insert record is type 5 (H9 — accepted)

**The problem.** Today's insert record (type 5) says which segment a row went to, but not which page. So when the log is replayed, replay has to work the page out again, and the leader's pages must come out the same as replay's, or a page fetched back from the page store would not match what the leader had.

**The rule.** Replay places inserts by **sequential fill**: a segment's inserts fill page 0, then page 1, and so on, using the page format's exact space arithmetic, with whole cells only. One function in `cascade-redo`, `SegmentFill::route`, decides it, for the leader and for replay.

The rule binds this engine. Until the record names its page and slot (type 10, §9), every insert goes where the fill cursor says. There is no free-space-map choice, no slot reuse and no prune.

**The cursor advances in LSN order.** The page is chosen inside the same critical section as the record's LSN reservation: a short per-relation mutex over reserve, route and fill, never held across an `await`. If two cores reserved LSNs in one order and chose pages in the other, the leader would hold a layout replay cannot reproduce. That matters because, once storage M8 lands, the leader fetches an evicted page back from the page store, in replayed layout: a scan crossing the eviction could skip or repeat rows, and a fetch by Tid could return a different row.

On open, the cursor is rebuilt from the segment's open page (`SegmentFill::from_page_header`). The mutex is a lock on the insert path, accepted because the record format leaves no alternative. It is deleted at the switch.

*Sources:* `cascade-redo` (`SegmentFill`); storage M8 (the page store serves evicted pages back); the runtime plan's FR-46. Round-3 fix V35 moved these out of the opening sentences.

*Provenance.* H9 was decided on 2026-11-26. It replaces the runtime plan's earlier acceptance (FR-46) that live layout may differ from replay layout, which predates the page store serving pages back to the leader. The plan is updated.

### Placement after the switch (type 10 and later)

Once the record names its page and slot, replay takes the placement from the record and the engine may choose freely: §4.1's insert path and the free-space map of §7.2 apply from here.

### Pages are self-describing

A heap page needs no per-relation header and no metapage (the B-tree's B1 has no counterpart here). The relation's only state above the page is its segment id, its extent and its page hint, which `ScopePages` keeps.

### Tids are stable

A slot's index is never renumbered and the slot array never shrinks (page D1). A Tid is what index entries carry. A slot is reused only after it has become unused (§7.3).

---

## 3. Row Identity, Locators and the Chain Law (H2)

A heap row's versions form a **chain**, linked old → new by each version's `next` word. A HOT chain is one that never leaves its page.

**A locator names one version.** `RowLocator::Tid` addresses a version, and the engine resolves it by walking forward from that version (heap §5.2). What a locator names depends on where it came from:

- A scan returns the Tid of the version it showed.
- An index entry names the version that was current when the entry was written, which is always the **root** of a HOT chain: the first version on its page. A HOT update writes no entries, so they keep naming the root. An update that is not HOT writes entries for the new version, which becomes the root of a new HOT chain on its page.

**Finding a version's root (round-3 fix V4).** A delete or a non-HOT update must stamp the row's existing index entries, and in a non-unique index the tree key includes the root's Tid (btree B8). The engine may hold a later version, reached by a scan or a chain walk, and chains have no back pointer. But a HOT chain never leaves its page (law 1 below), so the root is on the same page as the version: under the exclusive pin, the engine builds the page's predecessor map from every slot's `next` word and redirect, and walks back from the version until it reaches a slot that is not `HEAP_ONLY`. That slot is the root, and its Tid is what the entries carry. PostgreSQL derives the root the same way (`heap_get_root_tuples`). The key values to look up are the version's own: a HOT chain shares its indexed values, because a HOT update changes none of them. The cost is one pass over the page's slot array, paid only when the version is `HEAP_ONLY`.
- An update's `Done` returns the new version's Tid when the locator moved, and the same locator when it did not.

**Two laws bind the chain.**

1. **A HOT chain never crosses a page.** §4.2 chooses a HOT update only when the new version fits on the old version's page. `HEAP_ONLY` is set if and only if the link stays on the page.
2. **A fetch on a dead slot answers "no row".** The slot state `Dead` (page D1) means the cell is gone. The walk returns `None`; it never reads whatever a later append put at those bytes.

A locator is valid for the statement that produced it, in the same transaction (layer §6.5).

---

## 4. DML Paths

Every path begins with the epoch check (layer §6.7) and works through the mini-transaction seam (layer §9.1): the engine builds records, and the seam applies them to frames the engine has pinned exclusively. When a group touches two heap pages, both frames are pinned first: made resident, then pinned fail-fast in ascending `(segment, page_no)` order, and on a refusal both pins are dropped and the group starts again after a yield (round-3 fixes V6, V28). Index changes are never in the same group as the row's records: each is its own mini-transaction, after the row's, in the same statement (layer §11.4).

### 4.1 Insert

This is the path once the record names its page and slot (type 10). Before that, placement is the fill cursor's (§2).

```text
insert(txn, row):
  cell = encode_cell(uncommitted(txn.xid), row_image)     # the page format's own encoder
  if the whole cell exceeds 8158 bytes: refuse it (§9)     # never logged
  page = fsm.find(cell_len + 2) or the hint page            # §7.2
  loop
    pin page exclusively
    if the page is low on space: prune it under this pin     # §7.1
    if the cell fits: break
    unpin; page = the next candidate, or a new page
  slot = the lowest unused slot, or the next slot
  apply HEAP_INSERT_V2(segment, page, slot, cell)           # the seam stamps the page
  unpin
  write the row's index entries                             # §4.6
  return Done { locator: Tid(page, slot) }
```

### 4.2 Update — the HOT decision

The engine first finds the row's newest version by walking forward from the locator (§5.2), and runs the conflict check of §4.4 on it.

```text
update(txn, loc, new_row, changed):
  newest    = the newest version of the row
  conflict  = check(newest)                                 # §4.4
  if conflict: return it
  indexed   = changed ∩ indexed_columns ≠ ∅
  key       = changed ∩ key_columns ≠ ∅
  hot       = not indexed and the new version fits on newest's page
  if hot:
      group = [ STAMP_DELETER(newest, xmax = uncommitted(own)),
                RELINK(newest, next = (same page, new slot) with HEAP_ONLY),
                HEAP_INSERT_V2(new slot, cell) ]
      return Done { locator: loc }                          # entries and locators unchanged
  page      = fsm.find or a new page
  group     = [ STAMP_DELETER(newest, xmax = uncommitted(own); XMAX_UPDATES_KEY if key),
                RELINK(newest, next = (page, new slot)),
                HEAP_INSERT_V2(page, new slot, cell) ]
  then, one mini-transaction per index:                    # §4.6, layer §11.4
        delete words on the entries of root(newest); entries for the new version
  return Done { locator: Tid(page, new slot) }
```

The three row records are **one mini-transaction group**. The old version's death, the chain link and the new version's birth are never separated by a replay boundary. The two records on the old version are on one page; the insert may be on another. `root(newest)` is the HOT chain root of §3.

**One name per operation (round-3 fix V17).** Stamping a heap version's deleter word is called `STAMP_DELETER` in pseudo-code, `HEAP_STAMP_DELETER` as a record, and `apply_stamp_deleter(img, slot, word, flags)` as a page primitive. "Delete-mark" is reserved for the IOT's `DELETE_MARK` flag. Record 11 writes the deleter word and the two lock flags. Record 12, `HEAP_RELINK`, writes only the pointer and the `HEAP_ONLY` bit of the `next` word and preserves every other flag bit, so a relink after a stamp cannot clear `XMAX_LOCK_ONLY` or `XMAX_UPDATES_KEY`.

**Why a non-HOT update writes entries even when no indexed column changed.** The new version lives at a new Tid. Index entries must reach it, so they are written for the new version, and the old version's entries receive delete words. Older snapshots still see the old entries, because their delete words are not yet visible to them. A HOT update avoids all of that, which is its point.

`changed` comes from the executor, and the engine trusts it: a set that omitted an indexed column would make an update HOT that is not. Debug builds can check it by comparing the encoded key columns of the two versions.

When the row moves, its locks move with it: none is left behind on the old version (§4.4).

### 4.3 Delete

Find the newest version, run §4.4, then one group: `STAMP_DELETER(newest, xmax = uncommitted(own))` with `XMAX_UPDATES_KEY`. Then, one mini-transaction per index, delete words on every entry of the row, found through the chain's root (§3, §4.6). The tuple's bytes stay where they are. Prune removes them once the cleanup watermark has passed the delete (§7.1).

### 4.4 Conflicts, waits and row locks (H3)

**The conflict check.** The engine reads the newest version's words and the lock manager's entry for it:

| What the newest version shows | `Wait` | `DontWait` | `SkipLocked` |
| :--- | :--- | :--- | :--- |
| Another transaction's deleter, or its exclusive locker, still in progress | Wait on it (layer §6.5), then start over: the row may have moved | `Conflict::InProgress { holder }` | `NotVisible` |
| Another transaction's shared locker, in a mode that conflicts with ours | The same | The same | The same |
| Another transaction's creator, still in progress | The same | The same | The same |
| A creator or a deleter that committed after our snapshot | `CommittedAfterSnapshot { commit_ts, latest }`, with `latest` the newest version's locator | The same | `NotVisible` |
| Deleted by our own transaction, or the slot is dead, or the chain ends dead | `NotVisible` | `NotVisible` | `NotVisible` |
| None of the above | Proceed | Proceed | Proceed |

A holder that has committed or aborted is not a holder. The wait has the bound the transaction layer configures; when it expires the result is `Conflict::InProgress`.

**`lock_row` records the lock.** A lock that leaves no trace excludes nobody, so every mode writes something (layer §6.10):

| Mode | What is recorded |
| :--- | :--- |
| `FOR UPDATE` | The version's deleter word is set to `uncommitted(own)`, with the flags `XMAX_LOCK_ONLY` and `XMAX_UPDATES_KEY` |
| `FOR NO KEY UPDATE` | The same, with `XMAX_LOCK_ONLY` only |
| `FOR SHARE`, `FOR KEY SHARE` | A lock-manager entry for the relation and the version's locator. Nothing is written to the page |

`XMAX_LOCK_ONLY` tells a plain read to ignore the deleter word: the engine passes no deleter to the visibility rule, so readers never wait and never see a locked row as deleted. Every write and every exclusive locker consults both places — the header word and the lock manager's shared holders — for the version it targets. The two flags are bits in the `next` word's flag field, which the page layer assigns: format F2 assigns three, and the rest are unassigned.

`lock_row` runs the conflict check first and returns `Locked { outcome, row }`, with the row present if the lock was taken. One lookup serves `SELECT … FOR UPDATE`.

**Locks move with the row.** An update never leaves a lock on the old version. Afterwards the engine records on the new version every lock the old one carried: its own, and other transactions' shared locks, which a non-key update does not conflict with. That is how a later key-changing update on the new version still finds them.

### 4.5 Upsert by speculative insertion

The contract is layer §6.9. The heap's part of it:

- **`insert_speculative`** is an insert (§4.1). Its arbitration is the primary-key index's entry, whose create word is the writer's uncommitted word. A second transaction inserting the same key meets that entry and waits, or does not.
- **`complete_speculative`** changes nothing.
- **`abort_speculative`** stamps the tuple's deleter word and the index entry's delete word with `uncommitted(own)`, as one mini-transaction. The transaction goes on running, so the row has to disappear now: it is `Deleted { own: true }` for the writer and invisible to every other transaction whatever becomes of the writer, because one transaction both created and deleted it. An entry whose create and delete words name the same in-progress transaction is not a conflict for the unique check, and a waiter for that key, sleeping on `WaitFor::Key`, is woken when the engine signals the key right after the delete word lands (layer §6.9, round-3 fix V13).

### 4.6 Index maintenance

Every insert, update and delete changes the row's index entries in the same statement, through `cascade-index`. Layer §11.4 has the table of which write changes which entry. Until `cascade-index` lands (Step 8), a heap table cannot have a primary key or a unique constraint (layer §12).

---

## 5. Reads

### 5.1 Sequential scan

The scan walks pages in `page_no` order up to the segment's extent. For each page it takes a copy, which it holds nothing from once it moves on; it yields between pages. For each slot:

- `Unused` is skipped: vacuum leaves unused slots behind in normal operation, and slots are never removed (page D1). Only a fetch through a locator treats `Unused` as a bug (§5.2; round-3 fix V14).
- `Dead` and `Redirect` are skipped. The scan reads every live cell, so a redirect's target is visited anyway.
- `Live`: decode the cell and apply `row_visible(xmin, Some(xmax))` — with no deleter when `XMAX_LOCK_ONLY` is set (§4.4). A `Live` result is emitted, with **this version's own Tid** as the locator.

Every version of a row is visited, and the visibility rule shows exactly one of them to a snapshot, so the scan emits rows, never versions. `SeqScanOpts.resume` carries the page number to resume from.

### 5.2 Fetch by Tid — the chain walk (H10)

```text
fetch(snap, loc):
  (page, slot) = loc
  loop
    switch slot_state(page, slot)
      Unused      → bug: assert
      Dead        → return None
      Redirect(t) → slot = t
      Live        → switch row_visible(xmin, deleter)             # layer §6.6; no deleter if XMAX_LOCK_ONLY
                      Live       → return this version
                      Deleted    → if not next.has_next(): return None
                                   succ = next
                                   if succ is not a Live slot, or creator(succ) ≠ deleter(this):
                                       return None                         # a stale link (V5)
                                   (page, slot) = succ
                      NotBornYet → return None
```

**A hop is checked (round-3 fix V5).** The walk follows `next` only if the successor's creator word is exactly the predecessor's deleter word: the same (sub)transaction id, since one update stamps both. Otherwise the link is stale and the walk ends. A stale link arises when an update that crossed pages is rolled back: the old version stays live with `next` still set, the successor is pruned and its slot vacuumed and reused, and a later transaction deletes the old version without relinking. Without the check, a reader that sees that delete would follow the link to whatever row now occupies the slot. PostgreSQL guards the same case by comparing the next tuple's `xmin` with the previous one's `xmax`. An update always overwrites `next`, so only a delete or a lock can leave such a link behind.

**`Deleted` is not "gone".** When an update replaces a version, the old version is `Deleted` and its successor is the row. The hop is what makes an update reached through an old locator work.

**`NotBornYet` is "gone".** Chains run old → new. If this version is younger than the snapshot, every later one is too, and older ones cannot be reached. A locator this transaction holds never names a version younger than its snapshot (layer §6.5), so this case means the row did not exist then.

A chain v1 (created at t1), v2 (at t2), v3 (at t3):

| Snapshot | From v1's Tid | From v2's Tid |
| :--- | :--- | :--- |
| before t1 | `NotBornYet` → none | `NotBornYet` → none |
| t1 to t2 | v1 | `NotBornYet` → none |
| t2 to t3 | v1 is `Deleted` → hop → v2 | v2 |
| after t3 | v1 is `Deleted` → v2 is `Deleted` → v3 | v2 is `Deleted` → v3 |
| v2's update rolled back, v2's slot reused by row R, then v1 deleted by Y at t4; snapshot after t4 | v1 is `Deleted`; R's creator is not Y → none | (v2's Tid now names R: an ordinary fetch of R) |

The walk is bounded by the chain's length. A link to the page it is on that revisits a slot is corruption and asserts.

### 5.2.1 The re-check after a conflict: `fetch_latest_committed` (H13)

Read committed uses this after `CommittedAfterSnapshot`. "Now" needs a definition, so the procedure is:

1. **Wait for the row's holders.** Any transaction still writing the row, or holding a lock that conflicts, is waited for, with the bound; each is resolved through `VisibilitySource` and `WaitSource`.
2. **Then take a fresh snapshot** (`TxnCtx::fresh_snapshot`, layer §6.7). Taking it after the wait is what makes a holder that committed during the wait visible: its commit timestamp is below the new read timestamp (round-3 fix V7).
3. **Walk forward from the locator** with the fresh snapshot, by the walk above, and return the first `Live` version with its own Tid. If the walk meets a version whose writer is again in progress, go back to step 1.

The executor then re-evaluates its predicate on that row and decides.

### 5.3 Rows and projection

A `RowRef` owns a decoded copy of the visible columns. No reference into a page outlives the call. It also keeps the engine handle and the locator, so that a stored-out-of-line column can be fetched on demand (§8). The `ColumnSet` an update receives comes from the executor; which columns are indexed comes from the descriptor (layer §6.12).

---

## 6. Visibility Integration

The engine has no visibility logic of its own. `Snapshot::row_visible` with the injected `VisibilitySource` (layer §6.6) is the whole rule. The engine's obligations are three:

- **Stamping.** A creator and a deleter are stamped `uncommitted(txn.xid)`. The engine never writes a committed word; resolving one is the status table's. **This is why layer §13.1 blocks the MVCC step:** for as long as a word is never rewritten, the status table must keep an entry for every transaction a page names.
- **The reader's own writes** are visible through `Snapshot.own`. The engine never special-cases its own rows.
- **Deadness comes from the status table and nowhere else.** Prune asks it, and asks the injected cleanup watermark. On a follower the same interfaces answer from the status table rebuilt from the log. The engine's mutation paths do not run there; page changes arrive through the log feed.

---

## 7. Prune, Vacuum and the Free Space Map

### 7.1 Prune — trigger and plan (H11)

**Trigger.** An insert or update that finds its page low on space prunes that page before it looks elsewhere, under the pin it already holds. `Maintenance::step` prunes pages from the list of pages that hold dead versions.

**The plan is computed while the page is pinned exclusively**, from the pinned copy. Its verdicts must describe the image they are applied to. The plan is the page design's `PrunePlan { free, redirects, relink }` (page D2).

**Eligibility.** A version may be removed only when its creator and its deleter are both resolved — committed or aborted — with commit timestamps older than the cleanup watermark. A stamp that is still in progress is never eligible, because its transaction may still roll back and a read-committed re-check may need the row.

**Mechanics** follow the page design's worked example: a same-page successor makes the root a redirect; a cross-page successor keeps a stub; a standalone dead tuple becomes dead; a survivor whose successor was removed is relinked.

**Applying.** The plan goes through `apply_prune`, which validates it against the image and transforms or refuses as a whole (page D2). The plan is the `HEAP_PRUNE` record (§9), so replay reproduces the verdicts without a status table.

The **cleanup watermark** is injected as a function, never derived here. It is the oldest read timestamp still in use on the leader, its followers and its replicas, because prune verdicts are replayed there (layer §11.7, round-3 fix V8).

### 7.2 The free space map (H12)

The map is a **tree of maxima** over the segment's pages, on ordinary logged pages (page kind 6).

- A **span page** covers a fixed run of 2048 heap pages. It holds one `u16` per page — that page's free space, rounded down to a multiple of 32 bytes — and, in its header, the largest of them.
- A **root page** holds one maximum per span.
- **`fsm.find(needed)`** scans the root for the first span whose maximum is at least `needed`, then that span's page for the first heap page with room. A root covers 4 Mi pages, 32 GiB; a larger segment adds a level, and the shape repeats.

**Why a tree.** The map has one entry per heap page. A linked list would make every insert past the tail scan it. The tree costs one small page per 2048 heap pages and a short search.

**Why the values may be lossy.** A stale "has room" costs one failed append and a prune attempt. A stale "full" costs one skipped page. Both are benign, which is what allows values to change only when a page's free space crosses a 32-byte boundary. A change is one `FSM_SPAN_UPDATE` record (§9); when a span's maximum changes, an `FSM_ROOT_UPDATE` changes the root's entry in the same group.

Most inserts never ask the map: they go to the hint page.

### 7.3 Vacuum (H6)

**Claim.** A heap slot may be freed as soon as its row's death is older than the cleanup watermark. Vacuum does not wait for the row's index entries to be removed.

**Why that is safe.**

1. Every index entry of a row receives its delete word from the same transaction, in the same statement, as the row's deleter stamp (§4.6). The entries are found through the HOT chain's root (§3), so every entry is reached however the engine found the row. What matters is the transaction, not the log group: the entry's delete word commits or aborts exactly when the row's deleter does. A crash between the two leaves the transaction in flight, and recovery aborts it, so neither stamp ever takes effect (round-3 fixes V4, V6).
2. An entry's visibility is decided by its own two words (layer §6.6). No reader consults the heap to decide whether an entry is visible.
3. Once the delete word's commit is older than the cleanup watermark, no snapshot that can still run sees the entry. So no reader follows the entry to the Tid. An entry that is still physically present points at a reused slot, but it is skipped before that.

**The one way the argument fails**, and why it cannot happen: an index built while writes were possible on the table could hold entries for rows that died before the build finished, and those entries would never receive a delete word. A build blocks writes on its table for its duration (layer §6.12), so no row dies during a build. A build that ran concurrently would need a catch-up step; it is out of version 1.

**So vacuum is small.** One bounded unit per `Maintenance::step`, resumable page by page:

1. Pick a page with dead or redirected slots, from prune's list.
2. For each slot whose row's death is older than the watermark: `apply_slot_free`. A dead slot becomes unused. A redirect becomes unused only once the row itself is gone.
3. One `FSM_SPAN_UPDATE` for the page's new free space.

There is no index work, no table-wide pass and no freeze.

**Index vacuum is its own pass**, in `cascade-index`: it removes entries whose delete word, or aborted create, is older than the watermark, one index page at a time, to reclaim space (layer §11.4). The heap does not wait for it.

---

## 8. Out-of-line Storage (H7)

A value too large for a row is stored apart from it, in a **hidden segment of the same scope**. The write stays single-scope, and shares the row's log and key (kept §2).

- **Chunks.** The value is cut into chunks, each a row of a hidden heap relation with a unique index on `(chunk_id, chunk_no)`. That is why this lands with `cascade-index` (Step 8) rather than with a page kind of its own.
- **The pointer.** Where the value would sit in the row, the payload holds `[magic u32][chunk_id u64][total_len u32]`, and the row's `HAS_TOAST` flag is set. The magic word and the flag are the only format commitments made now; the chunk format, and whether values are compressed, are decided when this lands (§12).
- **Writing.** A row's chunks are written in the row's statement.
- **Who owns the chunks (round-3 fix V22).** Chunks belong to a *value*, not to a version. An update that does not change a stored-apart value copies the pointer into the new version and leaves the chunks alone, so all versions that carry the pointer share them. The chunks are stamped deleted, in the same statement, only when the value stops being carried forward: when the row is deleted, or when an update replaces that value. They are then ordinary dead versions, and the usual watermark rule keeps them for exactly as long as an older snapshot can still see a version that names them. This is PostgreSQL's rule for TOAST.
- **Reading.** `RowRef::value` reports that a column is stored apart, and `toast_value` fetches its chunks through the index and into the row's own buffer.

## 9. Redo Records (H8)

Record type bytes extend `cascade-wal-format`'s table and are never renumbered. **Proposed allocation, frozen at landing: 10–15 for the heap family, 40–41 for the free-space map.** Payload encodings belong to `cascade-wal-format`. The apply functions are the page design's primitives (page D4), which `cascade-redo` dispatches to (layer A2).

| Byte | Record | Payload | Replay |
| :--- | :--- | :--- | :--- |
| 5 | `HEAP_INSERT` | `[segment u32][cell verbatim]` | Landed. Kept forever for replay of existing logs; **not emitted** once Step 4 lands |
| 10 | `HEAP_INSERT_V2` | `[segment][page_no][slot][cell verbatim]` | Places the cell at the named slot. Replay reproduces the leader's layout exactly |
| 11 | `HEAP_STAMP_DELETER` | `[segment][page_no][slot][deleter word][flag bits]` | Writes the deleter word and the flags. Used by delete, update and an exclusive row lock |
| 12 | `HEAP_RELINK` | `[segment][page_no][slot][pointer and HEAP_ONLY bit]` | Writes the chain pointer and `HEAP_ONLY`; preserves the lock flags (round-3 fix V17) |
| 13 | `HEAP_PRUNE` | `[segment][page_no][the plan's verdicts]` | Transforms the page by the plan. Verdicts, never a watermark (page D2) |
| 14 | `HEAP_SLOT_FREE` | `[segment][page_no][slot]` | Marks the slot unused |
| 40 | `FSM_SPAN_UPDATE` | `[segment][span page][heap page][value]` | Writes one span entry |
| 41 | `FSM_ROOT_UPDATE` | `[segment][root page][span][maximum]` | Writes one root entry |

Three rules apply to all of them: the four redo laws (storage §12.4); a page's LSN is the end of its last record (layer §9.3); and replay applies a record only if its start is at or above the page's LSN.

A group is the unit of atomicity. A HOT update is one group of records 11, 12 and 10, and so is the row part of an update that is not HOT; its index changes follow as their own groups (layer §11.4). Nothing below a group is ever a replay boundary.

**The gate before the log.** The cell encoder's own ceiling is the 16-bit length field, 65 509 bytes of payload, which is far above what a page holds. The engine refuses a cell larger than the most a fresh page holds — 8158 bytes, 8132 of payload — before anything is logged (§4.1). Until §8 lands such a row is refused with PostgreSQL's `54000`. The page format's fit rule, which determines that number, counts the slot entry an append writes for itself; it did not always do so, and the fix that made it is recorded in layer §16.

---

## 10. Testing

The page design's §6 pins the format: marker goldens, the parse refusal matrix, the slot transition table, the prune round trip, plan-refusal atomicity, and redo read-back. The engine adds:

- **The chain walk.** For every chain shape and every snapshot time, `fetch` from every Tid in the chain returns what §5.2's table says. A property test over random update chains and random snapshots.
- **Visibility end to end.** Random interleavings of begin, write, commit and abort through the in-memory seam and a scripted `VisibilitySource`; every snapshot's scan result equals the reference model's.
- **Chain invariants.** A `HEAP_ONLY` link never leaves its page. No cycles. A stale link left by a rolled-back cross-page update is never followed after its successor's slot is reused (V5). The root found for any version is the slot its index entries name (V4).
- **The conflict table**, every row under every `WaitPolicy`, including the expiry of the wait bound and the poisoned paths.
- **Row locks.** Two transactions cannot hold conflicting locks on one row; several can hold `FOR SHARE`; a plain read never waits; a lock survives an update that moves the row.
- **Speculative insertion.** Two transactions race on one key; the loser's wait ends when the winner aborts the speculative insert, not when the winner's transaction ends.
- **Prune.** The page design's worked example, driven through the engine, under a snapshot that must still see every surviving version.
- **Vacuum.** A slot is reused only after the watermark passes. An index entry left in place after its slot is reused is never followed to a live row.
- **Replay equivalence.** Live pages and replayed pages are byte-identical over random DML, prune, vacuum and free-space workloads.
- **The fill cursor.** Before the switch to type 10, concurrent inserts from several cores leave the leader's pages equal to replay's.
- **Contract conformance.** The suite of layer §6.13 against this engine.
- **Wire parity.** The runtime's query, commit and transaction suites pass unchanged across each step until visibility behavior is deliberately pinned.

---

## 11. Landing Plan

The steps are those of layer §12. What each one lands in this engine:

| Layer step | In this crate |
| :--- | :--- |
| Step 3 | The first slice (plan T6.4): insert and scan through the buffer pool, the creator stamped, no visibility yet, record type 5, placement by the fill cursor in LSN order (H9). The speculative and scan-positioning calls exist as signatures. |
| Step 4 | The MVCC milestone: slot states (page D1), visibility, the DML paths with real outcomes, row locks, prune, and the switch to record type 10. The switch retires the fill cursor and its mutex, and is the earliest point at which free-space placement and slot reuse may begin. |
| Step 5 | Hygiene: the two dead dependencies go, the crate description is corrected, the context file is synced. |
| Step 8 | `cascade-index` lands: index maintenance and index vacuum start, and out-of-line storage with them. |

Each step is one commit with the workspace gate green.

---

## 12. Open Questions

| Question | Owner, and needed before |
| :--- | :--- |
| How version words become self-describing | The transaction layer. **Blocks Step 4** (layer §13.1) |
| The two row-lock flag bits, assigned by the page layer | The page layer; before Step 4 |
| The free-space map's page layout, frozen | Before Step 4 |
| The chunk format and compression for out-of-line storage | Before Step 8 |
| How prune is scheduled in the background, and a bound on the dead-page list's memory | Tuned with `cascade-maintenance`; correctness never depends on it |
| Unlogged heap tables | Later; the record type is reserved |

---

## 13. Decisions

Statuses mean: **Proposed**, awaiting the project owner; **Accepted**, by whom and when; **Superseded**, by what.

| # | Decision | Status |
| :--- | :--- | :--- |
| H1 | One main segment per relation, and a hidden one for out-of-line storage. Pages are scanned in `page_no` order; there is no page chain | Proposed |
| H2 | A locator names one version. A HOT chain never crosses a page. A dead slot answers "no row" | Proposed |
| H3 | The conflict check of §4.4. An exclusive row lock is recorded in the row's header; a shared one in the lock manager. `abort_speculative` stamps the deleter word and the entry's delete word | Proposed |
| H2a | A delete or non-HOT update finds the row's index entries through the HOT chain root, derived from the page (§3) | Proposed (round-3 V4) |
| H4 | A version is prunable when its creator and its deleter are resolved and older than the cleanup watermark. An in-progress stamp never is | Proposed |
| H5 | The free-space map is kept on ordinary logged pages, kind 6, with values rounded to 32 bytes | Proposed |
| H6 | Vacuum frees a slot once the row's death is older than the cleanup watermark. It does not wait for the row's index entries, because every entry is stamped by the same transaction that stamps the row (§7.3). Index vacuum is a separate pass | Proposed. It replaces kept §2's ordering rule. Restated in round 3 (V4, V6) |
| H7 | Out-of-line values are chunks of a hidden heap relation with a unique index. The pointer format is reserved now and frozen at Step 8 | Proposed |
| H8 | `HEAP_INSERT_V2` (type 10) names its page and slot. Emission switches at Step 4; type 5 stays replayable forever | Proposed |
| H9 | While the record is type 5, placement follows `cascade-redo`'s sequential-fill rule and the fill cursor advances in LSN order. Free-space placement and slot reuse begin with type 10 | **Accepted** (Kenneth, 2026-11-26). It supersedes the runtime plan's FR-46 acceptance that the two layouts may differ |
| H10 | The chain walk of §5.2: `Deleted` hops to a successor whose creator equals the deleter, `NotBornYet` is "no row" | Proposed (hop check added in round 3, V5) |
| H11 | A prune plan is computed while the page is pinned exclusively | Proposed |
| H12 | The free-space map is a tree of maxima: span pages over 2048 heap pages, a root over spans | Proposed |
| H13 | The re-check waits for the row's holders, then takes a fresh snapshot, then walks (§5.2.1) | Proposed (order corrected in round 3, V7) |
