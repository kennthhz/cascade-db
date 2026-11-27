# Access layer design review — 2026-11-26

**Status:** all 30 findings of round 1 (below) are applied in v0.2 of the three designs. Round 2, a top-down review of v0.2, found twelve more and they are applied too; it is recorded at the end of this file. Layer §16 of the layer design maps every finding to its change.

**Reviewed:** `access-layer-design.md`, `cascade-btree/AI/btree-design.md`, `cascade-heap/AI/heap-design.md` (Draft v0.1, as committed in `1246443` and `54b30c4`), read against `cascade-page/AI/storage-page-mutation-design.md` and the storage layer design. **Stance:** a senior principal engineer new to the project, reviewing for understandability, consistency, and correctness of algorithms and design. Code is out of scope (the three engine crates are stubs).

## Verdict

The decomposition is sound: one contract, engines that differ only below it, page bytes owned by the format authority, redo dispatched in one place. As specifications to implement from, the documents are not ready:

- They are not self-contained. The contract's central types and the engines' semantics of record live in documents that were deleted from the repository (R1), and the shorthand cannot be followed without a legend (R2).
- Nine findings would give wrong results or break a stated guarantee if implemented as written (R3–R11). Five of them sit in the heap engine's everyday read and write paths.
- The transaction-layer dependency that decides whether MVCC is feasible at all is filed as an open question (R10).

Recommended order: R1–R2 (so the documents can be judged), then R3–R11, then the rest before the first engine code lands.

## How to read the findings

**High** — wrong results, a lost guarantee, or a blocked milestone. **Medium** — underspecified, a cost the design does not acknowledge, or a contradiction that will mislead an implementer. **Low** — clarity. Tags: **[A]** algorithm or design correctness, **[C]** consistency, **[U]** understandability.

## High

### R1 [U] The "single source of truth" cannot be read from the repository

*Where:* layer design §4.3, §4.5, §8.3; heap design preamble; btree design preamble.

*Problem:* The contract's outcome, conflict, lock, scan and capability types are "WP5 §5 verbatim … not restated here". WP5, WP1, WP2 and HLD v2 were retired from the tree on 2026-11-22; they exist only through `git show 381ee5b^:…`. The three documents cite them on 67, 16 and 6 lines respectively. Fourteen type names the contract depends on (`WriteOutcome`, `Conflict`, `RowLockMode`, `WaitPolicy`, `Locked`, `ScanRow`, `TableScan`, `AmCapabilities`, `TableStats`, `Projection`, `ColumnSet`, `IndexDef`, `TxnCtx`, `SpecToken`) are defined nowhere in the tree. The same goes for the engine semantics: "HLD v2 §11/§12/§13" is named as the semantics of record for IOT, heap and indexes.

*Why it matters:* A reader cannot tell what `WaitPolicy::SkipLocked` returns from `update`, or what an IOT delete-mark means for an older snapshot, without archaeology. A new engineer — or a new agent session — will fill the gaps with assumptions.

*Fix:* Restate the contract types in an appendix of the layer design, and move HLD v2 §10–§13 (access API, IOT, heap, indexes) into `05-access/AI/` as a kept document. Cite sections of documents that exist.

### R2 [U] Identifier collisions and unlabelled cross-references

*Where:* all three documents.

*Problem:*

- "L0–L9" are migration steps in the layer design §10 and the heap design §11, architecture layers in `architecture-design.md`, and "layer decisions L1–L3" in the page design.
- "D1–D6" are page-design decisions, "D1–D15" WP5 decisions, "D1–D5" WP2 decisions, "D1–D11" architecture decisions.
- "A1–A9", "B1–B7", "H1–H9", "F1–F4", "S16–S21", "FR-39…", "T6.x", "M0–M9", "WP1–WP11" are used with no legend saying what each family numbers or where it is defined.
- Bare section numbers change referent mid-paragraph. The layer design has 34 references to "§9", "§10", "§11" or "§12" without naming the document, and it has its own §9–§12, as do the storage design and HLD v2. The heap and btree designs have 76 and 45 bare section references.

*Why it matters:* "L4 is the coupling point" reads as a layer. "heap design §4" is a section of a different document from the "§4" two sentences earlier.

*Fix:* Rename the migration steps (Step 1…). Add a legend at the top of the layer design: ID family → what it numbers → where it is defined. Qualify every cross-reference with its document.

### R3 [A] Heap chain walk stops too early and hops the wrong way

*Where:* heap design §5.2.

*Problem:* The walk says `Deleted` → "stop with `None` — every later version is younger", and `NotBornYet` → "hop forward … the snapshot's version, if any, is further down the chain". Both are backwards for a heap, whose chains run old → new.

- **`Deleted`.** When the current version's deleter is committed at or before `read_ts`, the row often still exists at `read_ts` as the *newer* version the update created. Row updated at t2, snapshot T ≥ t2, locator = the old Tid: the old version is `Deleted` and its successor is `Live`. Stopping returns "no row" for every update reached through an old locator. The hop is needed when `next.has_next()`; the row is gone only at chain end.
- **`NotBornYet`.** If the current version's creator is younger than `read_ts`, every later version is younger still. The snapshot's version would be an *older* one, which the format cannot reach. The answer there is `None`: a locator held by this transaction never names a too-young version, and `Conflict::…{ latest }` is for `fetch_latest_committed`.

*Fix:* At a Live version: creator not visible → `None`; deleter visible → hop if `has_next()`, else `None`; otherwise return it. Add the update chain as a worked example and a property test (every snapshot × every Tid in a chain).

### R4 [A] `lock_row` records nothing, so row locks do not exclude

*Where:* heap design §4.4 (last sentence), §13 H3; btree design §2 (IOT row header).

*Problem:* "`lock_row` runs this protocol with no tuple change." If `SELECT … FOR UPDATE` leaves no trace on the row, the next transaction's lock or update attempt finds no conflicting word and succeeds: two transactions both hold the row. PostgreSQL records the locker in `xmax`, with lock-only and lock-mode bits and a multi-locker id for shared lockers. Neither row header here can express that. The heap header has one `xmax` word doing double duty as deleter and three flag bits, all assigned. The IOT leaf row has no deleter word.

*Why it matters:* Foreign-key checks take key-share locks on referenced rows for the transaction's duration, and `FOR SHARE` is ordinary SQL. This is a format question, and the heap format is still in its amend-in-place window (major version 0): cheap now, a format change later.

*Fix:* Decide the row-lock representation before the MVCC milestone. Minimum: `FOR UPDATE` and `NO KEY UPDATE` stamp `xmax = uncommitted(own)` with a lock-only flag, so readers ignore it and only lockers and writers consult it. State what version 1 does about `FOR SHARE` and `FOR KEY SHARE`, which need a second id space.

### R5 [A] `abort_speculative` must change bytes

*Where:* heap design §4.5, §13 H3.

*Problem:* "`abort_speculative` changes no bytes either — the tuple's creator word resolves to `Aborted` at rollback". The transaction does not roll back here: `ON CONFLICT DO UPDATE` and `DO NOTHING` abort the speculative insert and continue. Until the transaction ends, the tuple and its index entry carry `uncommitted(own)`, which means the same transaction's later statements see a row that was never inserted (own writes are visible), and any other transaction inserting the same key keeps waiting on a key that was released. PostgreSQL's `Heap_AbortSpeculative` marks the tuple dead immediately for this reason.

*Fix:* `abort_speculative` stamps `xmax` on the tuple and the delete word on the index entry (`uncommitted(own)`), as one mini-transaction group. The conflict protocol treats "create and delete word are the same in-progress transaction" as no conflict, and waiters are woken when that delete word lands. Correct H3.

### R6 [A] B-tree writers validate before they pin, which leaves a window

*Where:* btree design §3 ("after a validated descent"), §5.1.

*Problem:* Insert: validate the path, then take the leaf's exclusive pin. Between the two, another writer can split that leaf. The pinned leaf is then the current left image, which no longer owns keys in the right half; the insert lands in the wrong leaf and the tree's ordering breaks. A split has the same shape: "validated descent → pin parent, left, right", but the parent may have been split in between, so the left node may no longer be its child.

*Fix:* Acquire the pins first, then validate. For a leaf write: pin the leaf exclusively, then read the ancestors' versions, and retry on any change. (A leaf's key range can change only through a split of the leaf itself, which needs the pin now held.) For a split: after pinning parent, left and right, re-read the grandparent's versions and check under the pins that the parent's entry for the left node still routes the split key to it. Write down the argument the design already relies on: every node of a split is rewritten under its pin, so a reader can never copy a post-split child together with a pre-split parent.

### R7 [A] A root split can produce a false "not found"

*Where:* btree design §3 (first line of the descent), §4.

*Problem:* The descent loads the root page number from the handle, then copies that page. If a root split happens between the two steps, the reader copies the old root page after it was rewritten as the left child. A key in the right half is absent there, and validation cannot catch it: the reader has no earlier version of that page to compare, and "key absent from a leaf" is a legitimate result. §4 claims a stale root never gives a wrong answer and that "re-read-from-root catches the routing miss". Nothing in the protocol does.

*Fix:* Treat the root pointer as the top-level version. After the walk, reload `handle.root` and compare it with the page read as the root (also compare the height from the handle with the kind of node read); on mismatch, retry. Alternatively store a high key in every node, so a search beyond the node's range is detectable.

### R8 [A] The visibility rule has no truth table, and its vocabulary fits only the IOT

*Where:* layer design §4.5.

*Problem:* `RowVisibility::NotBornYet` is annotated "walk to older" and `Deleted` "walk to older". That is the IOT's undo direction; the heap's chains run the other way (R3). `Deleted { in_progress: Option<TxnId> }` is undefined: a deleter still in progress must leave the row visible to other readers, so it is unclear when this variant is returned. The same paragraph says `row_visible(creator, None)` "is exactly the index-entry rule", while §9.1 defines entry visibility as `row_visible(create, Some(delete))`.

*Fix:* Give the full table in the layer design: for the creator and for the deleter, across {empty, own in progress, other in progress, committed ≤ read_ts, committed > read_ts, aborted}, what `row_visible` returns. Then give each engine's mapping. IOT: walk undo toward older on both outcomes. Heap: hop newer on `Deleted` with a successor; return none on `Deleted` at chain end and on `NotBornYet`.

### R9 [A] The unique-check mapping is inverted

*Where:* layer design §9.1.

*Problem:* "a key conflicts if an entry with that key is visible-live or uncommitted-by-another — `Conflict::InProgress` for the former, `UniqueViolation` for the latter." That maps visible-live to `InProgress` and uncommitted-by-another to `UniqueViolation`. It is the other way round, as btree design §6 and HLD v2 §13 have it: a committed live entry is a unique violation; another transaction's in-flight entry is `InProgress`, because it may still abort.

*Fix:* Swap the two outcomes.

### R10 [A] Without commit-time stamping the status table has no bound, and this is filed as an open question

*Where:* layer design §13 (first row); heap design §6, §12.

*Problem:* The engines never write a committed word, so every `uncommitted(xid)` on a page is resolved through the status table for as long as that entry exists. The documents call the retention rule "purge-coupled". It is not: a row inserted by transaction X and never updated keeps `xmin = uncommitted(X)` for the life of the table, so X's entry can never be retired. The table grows with the number of transactions that have ever written a still-live row, and every visibility check is a lookup. PostgreSQL avoids exactly this with hint bits (the first reader rewrites the word) and a compact commit log. WP2 deferred stamping as optional; for the MVCC milestone it is a requirement.

*Why it is hard here:* Rewriting a word to `committed(ts)` after commit is a page change. A1 forbids a page change without a log record, and an unlogged rewrite makes the leader's page differ from the replayed one, which the byte-identical-pages goal and the differential tests forbid.

*Fix:* Make this a blocking decision for the transaction-layer pass, with these constraints stated, before any MVCC code: stamp at commit (the transaction must remember the positions it wrote), or stamp lazily under a rule that keeps replay identical, or change what the word stores.

### R11 [A] Vacuum's index-cleanup step cannot be done the way it is described

*Where:* heap design §7.3 step 2, §13 H6.

*Problem:* "find and remove index entries pointing at `(page_no, slot)` — via `cascade-index`'s per-page entry scan". A secondary index is ordered by key, not by heap position. Finding every entry that points at one heap slot means scanning the whole index; PostgreSQL makes one full pass per index per vacuum. That is incompatible with "one page at a time, never table-wide": the cost per heap page is proportional to the index.

*Observation that resolves most of it:* Entries carry their own delete words, and the engine delete-words every entry of a row in the same mini-transaction group as the heap delete-mark. Once that delete word is older than the cleanup watermark, no snapshot that can still run sees the entry. The slot can therefore be freed without waiting for the entries' physical removal, and index vacuum becomes an independent space-reclamation pass. The strict ordering in H6 is stronger than correctness needs.

*Remaining hole:* An index built concurrently inserts entries for rows that die before the build commits. The deleter never delete-words those entries, so they stay live and point at a slot vacuum may reuse.

*Fix:* Replace H6 with the watermark argument, define index vacuum as its own pass, and close the concurrent-build hole (the build delete-words rows that died since its snapshot, or an index pass consults the heap).

## Medium

### R12 [A] Duplicate keys in the trees are undefined

*Where:* btree design §5.2, §5.3, §9.

*Problem:* The tree is specified for unique keys: the property test expects "sorted unique keys" and `lookup` is one binary search. Three cases produce equal keys. A non-unique secondary index has several entries per key. An IOT update that does not fit in place becomes delete-mark + insert *at the same key* (§5.3). A primary-key update leaves a delete-marked twin. With two entries at one key, `lookup(key)` has no defined result, and splits must keep equal keys ordered.

*Fix:* Define the tree key as the key bytes for unique indexes and the IOT, and key + locator bytes for non-unique indexes (a prefix range then still finds all of a key's entries). Define the order among equal keys (oldest first) and make `lookup` return the run of equal keys. Restate the property test.

### R13 [A] The node-format sketches have no entry length

*Where:* btree design §2.

*Problem:* Leaf entries carry a key length but no entry length. Payloads are variable, and an index entry's locator is a variable-length primary key. The page layer validates totally at parse ("cell length sane, no tail overrun"), and §5.3 keeps slack inside an entry after an in-place shrink. Neither is possible without each entry's length.

*Fix:* A `len u16` prefix on every entry, as the heap cell has.

### R14 [A] `mod_count` is not defined: not where it lives, not how replay reproduces it

*Where:* btree design §2, §3, §7.

*Problem:* It sits in the page image (offset 16) and is the validation word for every read, but no rule says who increments it. If it is in the image, every redo record must reproduce the increment exactly, or leader and page store diverge. If it is in memory (the BPM already keeps one per frame), it resets when a page is evicted and fetched again, and a reader can mistake a refetched page for the one it read. Separately, "re-reads each ancestor's `mod_count`" means an 8 KiB copy per level per lookup.

*Fix:* Pick one. In memory is cheaper; it then needs a generation that changes on refetch. If in the image, add "every redo record increments it" to §8.

### R15 [C] Page LSN: the group's end, or the last record for that page?

*Where:* layer design §6.1 step 3; btree design §2 (header sketch).

*Problem:* Step 3 stamps each changed page with "the group's end LSN". The storage design's rule (S17) and the landed `cascade-redo` stamp a page with the end of the last record *for that page*. A group that touches pages P and Q would leave P with two different page LSNs on leader and page store, so the pages are not byte-identical.

*Fix:* Say "end LSN of the group's last record for that page".

### R16 [A] The mini-transaction text contradicts its own sketch

*Where:* layer design §6.1.

*Problem:* The text says the engine's records are applied to the pinned pages through the redo path, which makes leader and replay identical by construction. The sketch has the engine mutate pages through its own closure, with the record payload used only at replay. That is two code paths which must agree: the classic log-versus-page drift. The sketch also appends the group in `durable()` although step 1 says it is reserved and filled first. It does not show when pins are released before that await, nor how the LSN is available at `record` time, which heap design H9 needs.

*Fix:* Choose one model. Recommended: the engine supplies records, and the seam applies them through `cascade-redo` to the pinned frames. Redraw the sketch in the order reserve → pin → apply → stamp → release → wait for durability.

### R17 [C] The migration ladder is not a valid schedule, and it leaves a window without unique constraints

*Where:* layer design §10; heap design §11.

*Problem:* "Steps L0–L3 are independent of the storage migration's M7/M8; L4 … must land before M8 touches the BPM." The first heap slice depends on `cascade-redo`'s fill rule (H9), which is part of M7, and M7 and M8 are being built now, ahead of every step here. The L5 row says engines "need no change — they hold the seam", which is false for an engine built before L4. Separately, the MVCC step promises "real `WriteOutcome` conflicts", but `cascade-index` lands two steps later. Between them, a heap table's primary key and unique constraints are not enforced.

*Fix:* Re-sequence against the storage work as it is happening, and say what happens to tables with primary keys until indexes exist.

### R18 [C] The contention budget is stated more strongly than the design delivers

*Where:* layer design §5 items 2–3.

*Problem:* "a commit uses at most one contended atomic operation and takes no lock", with conflicts resolved through "atomics on the row's own bytes". In this design every page touch takes the frame mutex and a compare-and-swap on the pin word. The commit path takes the per-scope commit-section mutex (WP2 D1). H9 adds a per-relation insert mutex. A hot leaf (the rightmost one under sequential inserts) serializes every writer to it. Version words are stamped under the frame mutex, not with atomics.

*Fix:* State the budget for the uncontended case, list the serialization points, and correct the mechanism.

### R19 [C] `AnyTable` has two arms, but a third engine must implement the contract

*Where:* layer design §1.2, §4.1, §4.11, §9.3.

*Problem:* "exactly two arms until a third engine justifies reopening", yet `cascade-temptable` "implements the same `AnyTable` contract". The fake engine must pass a suite "written as generic functions over `AnyTable`-like dispatch", although D1 rejects traits.

*Fix:* Say how the executor holds a temporary table (a third arm, or the heap arm over a memory-only store), and that the conformance suite is written over a test-only trait.

### R20 [A] Nothing can wake a waiter, and lock outcomes have no error representation

*Where:* layer design §4.4, §4.9, §4.5.

*Problem:* `Wait` "blocks bounded on the holder's resolution through the `VisibilitySource`", but `VisibilitySource::status` is a poll with no notification. The lock manager's queues are named but are not part of the contract. `TamError` is "infrastructure-only", so there is no representation for lock-wait timeout, a `NOWAIT` failure, being chosen as a deadlock victim, or statement timeout.

*Fix:* Add a wait seam to the contract (`wait(target, deadline)` → resolved or timed out), and decide which of these are outcomes and which are errors.

### R21 [A] `fetch_latest_committed` does not define "now"

*Where:* heap design §5.2.

*Problem:* It returns the newest version "visible to {own uncommitted writes ∪ committed ≤ now}". "Now" needs a defined timestamp that is at least the conflicting commit, waited for through the closed timestamp, and a rule for rows whose writer is still in progress.

*Fix:* Define it.

### R22 [A] An IOT delete-mark is a flag with no version

*Where:* btree design §2; layer design §8.3.

*Problem:* The leaf row has `DELETE_MARK` as a bit. Whether the row is gone for a snapshot depends on the deleting transaction's commit time, which only the undo record at `roll_ptr` carries. That works only if every delete writes that record and purge keeps it while the mark exists. The btree design presents the flag as self-contained.

*Fix:* State the dependency in the format section, and settle it with the IOT design.

### R23 [A] The no-merge policy is never defined, and its cost is not stated

*Where:* btree design §1, §10, §12 (B6).

*Problem:* B6 is listed as a decision and "root collapse" appears in four places, but no section defines either. After a deletion-heavy workload the tree keeps its near-empty leaves and its height, so every lookup pays the full height and range scans cross empty pages.

*Fix:* Add the section: when root collapse runs, what is accepted, and which measurement reopens merge.

### R24 [A] The free space map is a linear chain

*Where:* heap design §7.2.

*Problem:* `fsm.find` scans a linked list with one entry per heap page. Once the tail page is full, each insert may cost a scan proportional to the segment. PostgreSQL's free space map is a tree of per-page maxima for this reason.

*Fix:* A tree over the same lossy values, or a documented first-fit strategy with a bounded search.

### R25 [U] Decision statuses overstate authority

*Where:* layer design §14; btree design §12; heap design §13.

*Problem:* The layer design marks A1–A9 "Decided" while its own status is "Draft v0.1 for review". Only H9 was ratified by the project owner. The other two documents say "Proposed — DRAFT" for everything, including decisions that only restate the architecture.

*Fix:* One vocabulary — Proposed; Accepted (by whom, when); Superseded — applied the same way in all three.

### R26 [U] No reading order, glossary or worked example at the layer level

*Where:* all three documents.

*Problem:* The page design opens with "Terms, in plain words" and a worked prune. The new documents open into cross-referenced shorthand. Terms used as if known: mini-transaction, group, scope, core set, fill cursor, verdict, cleanup watermark, version word, locator, HOT, speculative token, closed timestamp.

*Fix:* A glossary, and one worked write (an insert through commit) that all three documents refer to.

## Low

### R27 [C] Internal contradictions

- Layer A6 says `ScopePages` includes "the heap's free space map"; §7 says it "knows only segment extents".
- Layer §8.1 says the btree does "split/merge"; btree B6 says no merge.
- Layer §4.2 comments `Tid` as "packs relation-locally" while the struct holds a full `PageId`, as the next paragraph says.
- Layer §4.10: the key is "None for a heap table"; heap design §4.1 says a heap's primary key is a unique secondary index.
- Layer §3.1: `cascade-btree` depends on "`cascade-storage-api` (page fetch seam)", but engines reach pages through the page cache. The same row says `cascade-bpm` where the architecture's `cascade-pagecache` is meant, and lists "the log/mini-txn seam" as separate from the cache that owns it.
- Layer §9.1 says "no indexed column changed → no index write" for both engines; heap design §4.2 writes entries on a non-HOT update.
- Heap §13 H2 says "Locators name chain roots"; heap §3 says a locator names a version.
- Btree B2.1 mentions "`Unused` slots after purge-compact"; btree §7 compacts, which leaves none.
- Layer §13 lists the hash variant under open questions and "Decided here" in the same row.

### R28 [C] A stale epoch is both a debug assertion and a runtime error

Layer §5 item 4 calls a call on a stale epoch a debug assertion; §4.9 defines `TamError::Fenced`. An epoch changes under a running transaction by design (failover, reopen), so it is a runtime outcome.

### R29 [U] Dangling references, and session-log text inside the design

- Layer §1.2 points to "Section 9.2" for the index-kind seam; it is §9.1. Layer §4.8 has "Section heap design §4".
- Btree §1 cites "§2.4", which does not exist. Btree §2 has "Kinds 5 (tree meta) stays open".
- Layer §12 is titled "Corrections Proposed Elsewhere" but holds applied fixes, another session's uncommitted crate, and a broken sentence ("…expects `Applied`. and `SegmentFill::route`…"). Move status notes to the commit record. Keep in the design only what is still open, with an owner.

### R30 [C] `Datum` is listed as landed in `cascade-types`

Layer §4.2. `cascade-types` defines no `Datum`.

## What is good

- Ownership is explicit and defensible: btree bytes in the format authority (A3), a transaction-blind tree (A9), one redo dispatcher (A2). The supersession of WP5 D8 is argued, not asserted.
- The heap's format half is separated from its engine half, and the format is already golden-tested.
- The fit-rule fix came with a test that sweeps every page state.
- H9 notices something the plan missed: a leader whose layout differs from replay will be served different bytes after an eviction.
- Open questions name owners.

## Suggested order

1. R1, R2, R25, R26 — make the documents readable and honest about status.
2. R3, R4, R5, R8, R9 — the heap's read and write paths.
3. R6, R7, R12, R13 — the tree's write protocol and format.
4. R10 — before any MVCC code; it decides the transaction-layer contract.
5. R11, R14–R24.
6. R27–R30, while editing.

---

# Round 2 — top-down review of v0.2

**What was done.** After the 30 fixes, the three designs were read again from the top, as a newcomer would, for consistency across documents, gaps, and readability. A script also checked every cross-reference: 256 section references with a document prefix, none unresolved, and every decision id referenced is defined.

**Twelve findings. All are applied.**

## Consistency

**T1. The IOT had nowhere to put an exclusive row lock.** Layer §6.10 told every engine to stamp the row's deleter word, and the IOT leaf row has none (layer §11.3, btree §2.2). *Fix:* every IOT row lock lives in the lock manager, keyed by the primary key. An IOT write consults it. A11 and the architecture-change table now say so.

**T2. A primary-key update on an IOT rewrites every secondary entry**, because every entry's locator is the primary key. The index-maintenance table had no row for it, and the row above said "none, the key did not change". *Fix:* a row added, and the other row qualified (layer §11.4).

**T3. Two shape errors in the contract.** `index_scan` took the scan direction twice, as a parameter and inside its options. `TableHandle` carried a storage kind, which a temporary table does not have. *Fix:* one direction, and the handle is the relation and its scope (layer §6.3).

**T4. Names that disagreed.**
- Btree §5.1 described a split as several records while btree §8 made it one. Now one.
- The branch header sketch carried a `next_leaf` that a branch never uses. Now reserved.
- The free-space map's records were `FSM_UPDATE` in heap §7.2 and `FSM_SPAN_UPDATE` in heap §9. Now the latter.
- "page F2" named a decision that belongs to the format decisions F1–F4, not to the page design. Now "format F2", with a legend row.

## Gaps

**T5. Locks did not follow a row that someone else's update moved.** Only the lock owner's update moved its lock. A `FOR KEY SHARE` lock left on the old version would be missed by a later key-changing update on the new one — the case the lock exists for. *Fix:* no update leaves a lock behind (layer §6.10, heap §4.4).

**T6. A mini-transaction could end half applied on the leader.** A10 has the seam apply each record as the engine builds it, so a failing third record would leave two applied. *Fix:* the engine checks each change against the pinned copy with the apply's own function first, so a checked apply does not fail; if it does, the image is corrupt and the scope is handled as after a log failure (layer §9.1).

**T7. The first slice would have shown rolled-back rows.** It has no visibility. The worked write applies a row at insert time, so a rolled-back insert would sit on the page in plain sight. *Fix:* in that slice the record is reserved and applied in the commit path, as the runtime plan has it (layer §7, layer §12).

**T8. The scan-under-split argument was wrong for moved entries.** Btree §5.2 said entries a split moves were written after the snapshot. They are not; they are old entries that changed leaf. *Fix:* the argument is now by cases — whether the cursor had already copied the leaf — and correct in both (btree §5.2).

**T9. Two things were implied but not written where an implementer needs them.**
- A reader ignores a deleter word that has `XMAX_LOCK_ONLY`. That was in layer §6.10 and not in the two places that implement it, heap §5.1 and heap §5.2. Now in both.
- The heap engine trusts the executor's `changed` set to decide whether an update is HOT. Now said, with the debug check (heap §4.2).

**T10. Missing glossary entries**: epoch, seam, pin, poisoned transaction, undo, out-of-line storage, tree key, run. Added, with the F1–F4 legend row.

## Readability

**T11. A change to a retired design sat in the table of changes proposed to the architecture.** "Reads take the transaction context" departs from the WP5 sketch, not from the architecture. Moved to layer §6.8.

**T12. Decision numbers B8–B11 sit out of numeric order** in the B-tree design, because numbers are stable and the new ones went where their subject is. Now explained in its status paragraph.

## What is still open after v0.2

These are open by design, and each has an owner in the document:

- **Blocking:** how version words become self-describing (layer §13.1). It blocks the MVCC step and it is the transaction layer's decision.
- **Dependencies on other layers:** the two row-lock flag bits (the page layer), the lock manager's per-row lists, the free-space map's layout, the final shape of the mini-transaction seam, and how `ScopePages` survives a restart (layer §13.3, heap §12, btree §10).
- **Not re-reviewed:** the page design. It was not rewritten; the two corrections it still owes are listed in layer §16.
