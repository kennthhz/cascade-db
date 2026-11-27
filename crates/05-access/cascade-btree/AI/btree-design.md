# cascade-btree design — the B+tree mechanism

**Status:** Draft v0.2 for review, 2026-11-26, amended 2026-11-27 for the round-3 review ([`access-design-review-2026-11-27.md`](../../AI/access-design-review-2026-11-27.md): V3, V10, V11, V21, V36). v0.2 applies the review of v0.1 ([`access-design-review.md`](../../AI/access-design-review.md)): writers now pin before they validate, the root pointer is checked, tree keys and equal keys are defined, every entry carries its length, `mod_count` has a rule, and the no-merge policy is written down (R6, R7, R12–R14, R22, R23, R27–R29). Decision numbers are stable identifiers, so B8–B11, added by v0.2, sit where their subject is rather than in numeric order.

**What this document is.** The detailed design of `cascade-btree`: the B+tree mechanism shared by the IOT's primary tree (`cascade-iot`) and by every secondary index (`cascade-index`). It owns the tree — descent, latching, structure changes, ordered iteration, and operations on byte keys — and nothing else. Page bytes belong to `cascade-page` (layer A3), record payloads to `cascade-wal-format`, redo dispatch to `cascade-redo`, and every visibility decision to the engines above it.

**Background for readers new to B-tree concurrency (round-3 fix V36).** A B+tree keeps its entries in leaf pages ordered by key, linked left to right, with branch pages above them that route a search to the right leaf. The classic way to make it safe under concurrent writers is *latch coupling*: hold the parent's latch while taking the child's, then release the parent. That needs a latch to be held while the child is fetched. Here a page that is not in memory must be fetched with an `await`, and a buffer-pool pin may never be held across an `await` (pins are spin-style holds measured in microseconds, and a suspended task would block every other session on that core that wants the page). So this tree is *optimistic*: a reader copies each page without holding its parent, and afterwards checks that nothing it passed through has changed, starting over if something has (the idea behind optimistic lock coupling and Lehman–Yao's B-link trees). Writers pin only the leaf they change, and a split pins its few pages together after making them resident.

**Upstream:** the [layer design](../../AI/access-layer-design.md) — §6.4 (locators and keys), §8 (execution model), §9 (the write path; §9.1 defines the mini-transaction), §11.1. The storage design's §12.4 (redo contract) and §12.5 (page cache rules). The [kept document](../../AI/engine-semantics.md) §1 and §3. The page design's L1–L3. Citation prefixes are defined in layer §2.2; terms in layer §3.

**Depends on:** the landed page format and the cross-core buffer pool (pin word, copy-out guards, exclusive-pin mutation, no pin across an `await`); the page design's module split (page L3), which must land before the first B-tree page kind.

---

## 1. Scope

**Owns**

- Tree shape and invariants: node kinds, fill, height, and finding the root through the metapage.
- Descent and latching (B3): optimistic, validated after the fact, no pin across an `await`.
- Structure changes: the split (B5), and the decision not to merge (B6).
- Operations on byte keys: insert, lookup, ordered scan with mark/restore, update in place, delete-mark, stamping a word, and purge-compact.
- The design of the redo record family for tree changes (§8). The bytes of those records belong to `cascade-wal-format`; their apply functions belong to `cascade-redo`, written against `cascade-page`'s B-tree views.

**Must never grow**

- **Visibility.** Version words inside leaf payloads are opaque bytes here. `row_visible`, the unique check, and speculative insertion belong to `cascade-iot` and `cascade-index`. This crate answers "which entries are at this tree key", never "is this entry visible".
- **Undo.** `roll_ptr` is payload. Walking and applying undo belong to `cascade-iot`.
- **Transactions.** No transaction context, status table or snapshot appears in a signature. Every operation takes an `OpCtx` and nothing else transactional.
- **Key meaning.** Order is bytewise on the normalized key bytes (`cascade-pgtypes`). This crate never decodes a key.

One generic tree serves both users. An IOT leaf and an index leaf differ only in the codec of their entries, which the host supplies as a typed view over `cascade-page` bytes (§2.2). Branch nodes are shared verbatim.

---

## 2. Tree Shape and Node Formats

### 2.1 B1 — One tree, one segment; the metapage is page 0

A tree lives in exactly one segment, and every node pointer is a page number inside it (`u32`). That caps a tree at 2³² pages of 8 KiB, 32 TiB, which no version-1 table approaches. It also makes allocation, prefetch (the storage layer reads a segment's pages sequentially) and bulk load simpler. A tree across several segments would need wider child pointers and is a later format change.

Page 0 of every tree segment is the **metapage**: the root's page number, the tree's height, and a tag that says which leaf codec the tree uses. It is an ordinary logged page. A root split records the new root and the metapage in the same mini-transaction as the split, so recovery never has to find a root by scanning. A tree handle in memory keeps the root's page number and the height; both are refreshed when a group that changes them is applied (§4), on a follower too, whether or not the metapage is resident there (§4, round-3 fix V21).

### 2.2 B2 — Page kinds and headers

Four kinds under `cascade-page`'s registry, each frozen with golden tests when it lands: **2 = branch, 3 = IOT leaf, 4 = index leaf, 5 = metapage**. The metapage gets its own kind rather than a flag on a branch, so that parsing never depends on a flag.

All of them keep the page layer's discipline: the bytes are the specification (no `#[repr(C)]`), little-endian, total validation at parse, validate before mutate, a page LSN at a fixed offset (page L2), and entries growing down from the tail under a slot array that grows up from the header — the heap page's geometry, so that the transforms below have one shape.

```text
branch (kind 2)                    IOT leaf (kind 3)              index leaf (kind 4)
 0: format version, kind            0: format version, kind        0: format version, kind
 4: entry_count u16                 4: entry_count u16             4: entry_count u16
 6: free_offset u16                 6: free_offset u16             6: free_offset u16
 8: page_lsn u64                    8: page_lsn u64                8: page_lsn u64
16: mod_count u64   (§3.1)         16: mod_count u64              16: mod_count u64
24: reserved 4                      24: next_leaf u32              24: next_leaf u32
28: leftmost_child u32             28: reserved                    28: reserved
    (valid once entry_count > 0)
32: slot array, growing up          32: slot array                 32: slot array

entries, growing down:
 [len u16][sep_len u16]            [len u16][key_len u16]         [len u16][key_len u16]
 [separator key bytes]             [tree key bytes]               [tree key bytes]
 [child page u32]                  [creator word u64]             [locator bytes]
                                   [roll_ptr u64][flags u16]      [create word u64][delete word u64]
                                   [payload…]
```

- **Branch entries and routing.** Entry *i* holds a separator `k_i` and child *i*. The invariant is: every tree key in child *i−1* is ≤ `k_i`, and every tree key in child *i* is ≥ `k_i`. Equal keys may sit on both sides of a separator, because a long run can be cut (§2.3). The header's `leftmost_child` is child 0. A descent has two modes (round-3 fix V10): **leftmost**, used by lookups, scans and every operation on an existing entry, goes to child *i−1* when the key equals `k_i`, so it reaches the first leaf that may hold the key; **rightmost**, used only to insert a new entry, goes to child *i* on equality, so it reaches the end of the run, where a new entry belongs (§2.3). Separators are full keys in version 1: no suffix truncation, no prefix compression (§10).
- **IOT leaf row.** The creator is a version word. `roll_ptr` is opaque here; its encoding belongs to the IOT design. The flag word has bit 0 `DELETE_MARK` and bit 1 `HAS_TOAST`; the rest are zero. An IOT row's lock lives in the lock manager, not in this header (layer §6.10). There is no deleter word. **A delete-mark is not self-contained:** whether a snapshot sees the row as deleted depends on when the deleting transaction committed, and only the undo record at `roll_ptr` carries that (layer §11.3). This crate sets and clears the bit and reads nothing into it.
- **Index entry.** The locator is the primary key for an IOT table and a Tid for a heap table (`cascade-pgtypes`' `Locator`). Its length follows from the entry's: it is everything between the key and the two words.
- **Sizes.** An entry is at most 8158 bytes, the most a fresh page holds (layer §7, step 2). A larger IOT row is stored out of line; a larger index key is refused when the index is defined.
- **Fill.** Leaves split when an entry does not fit; branches when a separator does not. There is no fill-factor setting. The split-point rule in §5.1 is fixed.

### 2.3 B8 — Tree keys, and entries with equal keys

A tree orders its entries by a **tree key**.

| Tree | Tree key | Why |
| :--- | :--- | :--- |
| IOT | the primary-key bytes | the primary key is unique |
| Unique index | the key bytes | unique by definition |
| Non-unique index | the key bytes followed by the locator bytes | several entries share a key; adding the locator makes the tree key unique, and a range over the key's prefix still selects all of a key's entries |

The host builds the tree key; the tree stores it as opaque bytes.

**Equal tree keys still occur**, in every kind of tree, for two reasons. An update that cannot be made in place leaves a delete-marked entry beside its replacement at the same key (§5.3). And a dead entry stays in place until purge-compact removes it, so an entry inserted again at the same tree key meets its dead twin — in a non-unique index, after a heap slot is reused. Three rules cover them:

- Among equal tree keys, order is by insertion: the older entry sorts first, and a new entry goes last.
- `lookup(key)` returns the **run** of entries with that tree key, oldest first. The host chooses among them by visibility.
- A split does not cut inside a run, unless one run alone fills a page. A lookup descends in leftmost mode (§2.2), so it starts at the first leaf that holds the run, and follows `next_leaf` while entries carry the same tree key. Under the right-child rule alone a lookup would land right of the cut and miss the older part of the run (round-3 fix V10).

### 2.4 B9 — Every entry carries its own length

The first field of every entry is `len u16`, as it is for a heap cell. The page layer validates a page totally at parse, which it cannot do without each entry's extent, and an update in place that shrinks an entry leaves slack inside it (§5.3), which it cannot represent otherwise.

---

## 3. Descent and Latching (B3)

The buffer pool's pin is page-wide. A reader takes a coherent copy of a page; a writer takes the exclusive pin; either may meet `Contended` and try again; and no pin may be held across an `await`. A descent that meets a page which is not resident must await a fetch, so it cannot hold its parent pinned while it descends. The protocol is therefore optimistic: copy first, validate afterwards.

### 3.1 The validation word (B10)

Every node's header carries `mod_count u64`, and one rule governs it: **every redo record that changes a node increments that node's `mod_count` by exactly one, and nothing else writes it.** The record's apply does the increment, so the seam does it on the leader and `cascade-redo` does it on replay. `mod_count` is then a function of the node's redo history alone. The leader, a follower and the page store compute the same value, and a node's count never repeats.

A validator reads the field through a page-cache accessor that copies the eight bytes, not the page.

An in-memory counter was considered and rejected. A counter in the frame resets when a page is evicted and fetched again, and a reader could then take a refetched page for the one it read.

### 3.2 Reads

```text
lookup(tree, key):
  loop
    root_no = tree.root.load()            # the root pointer is the top-level version
    node    = copy(root_no)               # a coherent copy; note (root_no, node.mod_count)
    path    = [(root_no, node.mod_count)]
    while node is a branch:
        node = copy(route(node, key, mode)) # leftmost or rightmost on equal keys (§2.2)
        path.push((node.page_no, node.mod_count))
    if tree.root.load() != root_no:       # the root changed during the walk
        continue
    for (page_no, version) in path, except the leaf:
        if peek_mod_count(page_no) != version:
            continue                      # an ancestor changed since it was copied: start over
    return node                           # the leaf copy, now known to be the right leaf for key
```

The copies come first and the checks after the leaf copy is in hand. So any change to an ancestor between the reader's copy of it and its copy of the leaf is seen, and the reader starts over. A node's routing for a key cannot change without the node changing, so an unchanged path means the leaf is the right one.

**The root is checked by pointer, not by version.** A root split rewrites the old root page in place, as the left child, and installs a new root. A reader that loads the root pointer just before the split and copies the page just after gets the left child, with no earlier version to compare against; a key in the right half would be reported absent. Comparing the pointer catches it, and a root pointer that returns is the same root, because page numbers are never reused. The handle's height is checked too: a root that copies as a leaf when the handle says the tree is taller is stale.

### 3.3 Writes

```text
write(tree, key, make_record):
  loop
    leaf = lookup(tree, key)                          # §3.2, validated
    pin leaf exclusively                              # fail-fast: Contended → yield and retry
    if leaf is not the rightmost leaf and key > leaf.last_tree_key:
        unpin; continue                               # the leaf was split between lookup and pin
    # (an insert descends rightmost; an operation on an existing entry descends leftmost and
    #  moves right along next_leaf, one leaf copy at a time, until it finds the entry)
    if any node on the lookup's path, except the leaf, changed since the lookup copied it:
        unpin; continue
    record = make_record(leaf)                        # reads the pinned copy
    seam.apply(leaf, record)                          # layer §9.1: the redo function changes the page
    unpin
    return
```

**The pins come first and the validation after.** A leaf's key range can change only through a split of that leaf, and a split needs the leaf's exclusive pin — the one now held. So once the leaf is pinned, two checks are enough: the ancestors copied on the way down have not changed, and the key is not beyond the leaf's last key. Validating before pinning would leave a window in which another writer splits the leaf, and the insert would land in the wrong half.

Nothing awaits between the pin and the unpin. The durability wait comes after the group is sealed (layer §9.1).

### 3.4 Structure changes: acquiring several pins (B11)

A split changes three nodes — the left node, the new right node and their parent — or four when the left node is the root: the old root, the new right node, a new root and the metapage. All of them are rewritten under their exclusive pins, and nothing may await while a pin is held — but pinning a page that is not resident awaits a fetch. The order is therefore:

1. **Make every page resident.** Take a shared copy of each and release it at once.
2. **Take the exclusive pins** — parent, left, right — in that order, each fail-fast. For a root split they are the old root, the new right node, the new root and the metapage. If any refuses, release all, yield, and start over.
3. **Validate under the pins.** The nodes above the parent have not changed since the descent copied them, and the parent's entry whose child is the left node still routes the split key to it. Otherwise release all and start over.
4. **Build the records and apply them** through the seam.
5. **Release the pins.**
6. **Seal the group.** A commit waits for durability later.

The new right page's number is allocated before step 2. A split that then starts over leaves that number unused: a hole in the segment, harmless.

Parent → left → right is the only multi-pin order in the crate. Two structure changes therefore cannot wait on each other, and a writer that holds only a leaf pin never holds anything a structure change needs first.

### 3.5 Why this is sound

A reader copies nodes one at a time and checks each ancestor's version after its leaf copy is in hand.

- A plain write changes a leaf only under that leaf's exclusive pin, and no reader can copy a leaf during its exclusive pin. A reader sees the leaf either as it was before the write or as it was after.
- A structure change changes all of its nodes under their pins at once. No reader can copy a child after the change together with a parent from before it. A reader that copies a node after the change copied its parent either before the change — the parent's version then differs, and the reader starts over — or after, in which case both are current.
- The one node that changes without a version to compare is the old root during a root split. Its pointer is compared instead (§3.2).

So every interleaving that could mislead a reader changes an ancestor's `mod_count` between that reader's copy of the ancestor and its validation, or changes the root pointer. In each case the reader starts over. The model check of §9 enumerates these interleavings.

---

## 4. Root Discovery and Opening a Tree (B4)

`open(segment)` reads the metapage and returns a handle with the root's page number and the height. A root split's group updates both when it is applied. Readers validate against the root pointer on every lookup (§3.2). A stale handle costs one repeated lookup, never a wrong answer.

**On a follower (round-3 fix V21).** A follower drops records for pages it does not hold (architecture §2.3). If it dropped a root split because the metapage was not resident, its handle would keep naming the old root, which after the split is only the left child, and the §3.2 check would compare against that same stale handle. So the apply path has one exception to "drop what is not resident": for a `BT_SPLIT` that installs a new root and for every `BT_META`, the follower's `cascade-redo` hook updates the tree handle's root and height from the record, whether or not any of its pages are resident. The records carry the new root's page number and height for exactly this purpose.

---

## 5. Operations

### 5.1 Insert and split (B5)

```text
insert(tree, tree_key, entry_bytes):
  write(tree, tree_key, leaf ->
      if it fits:  a leaf-insert record
      else:        split the leaf (§3.4), then retry the insert from the top)
```

**A split is one mini-transaction and one record** (layer §9.1; §8). `BT_SPLIT` carries the left node's image, the new right node's image, the parent's new separator, and — when the left node was the root — a new root and the metapage. The durable LSN therefore never falls inside a split (storage §2, S16). §3.4 gives the order of pins.

**Where a split cuts.** At the median entry, except when the key being inserted sorts after the leaf's last entry. Then the new right node receives only the new entry. Sequential inserts keep their pages full instead of leaving every other page half empty. The test is made at each split, by comparing with the last entry; there is no remembered mode. A split never cuts inside a run of equal tree keys unless the run fills a page (§2.3).

### 5.2 Lookup and ordered scan (B7)

`lookup(tree_key)` is the descent of §3.2 and a search of the leaf for the run of entries with that tree key, oldest first (§2.3).

A scan positions itself by a lookup of the range's lower bound, emits the visible entries of each leaf copy in order, and follows `next_leaf`. The cursor emits from copies, one leaf at a time, and never holds a pin.

**A split during the scan loses nothing and shows nothing too new.** Take a leaf L that splits while the scan is under way, moving its upper entries into a new right sibling L′.

- *The cursor had already copied L.* It emits every entry of its pre-split copy, the moved ones included. It then follows that copy's `next_leaf`, to L's old right neighbour, and never visits L′. L′ holds the moved entries, already emitted, and anything inserted after the split — which was written after the snapshot's read timestamp and is invisible to it.
- *The cursor had not yet copied L.* It copies L after the split, emits the entries left in it, follows L's `next_leaf` to L′, emits those, and goes on.

An insert can be visible to a snapshot only if it committed before the snapshot's read timestamp, and the read waited for the closed timestamp. So nothing a split adds during the scan is visible to the scan.

**Mark and restore are anchored to a tree key.** `mark()` remembers the last tree key it emitted; `restore()` looks that tree key up again and resumes inside its run. This survives splits, compaction and any renumbering of slots. It costs one descent per restore, which merge join tolerates. `supports_mark_restore` is advertised statically.

### 5.3 Update in place (IOT leaves)

The IOT keeps the newest version in the leaf (kept §1), so an update overwrites the row when it can:

- **The new entry is not longer than the old.** Overwrite it and set `len` to the new length. The slack stays inside the entry until purge-compact. The record carries the new entry bytes; replay overwrites the same region, which it finds by tree key.
- **Longer, and the leaf has room.** The same, with the slot rewritten to the entry's new start. One record.
- **Otherwise.** The host falls back to a delete-mark and an insert at the same tree key. The tree then holds a delete-marked entry and a live one with equal tree keys (§2.3) until purge removes the dead one. The IOT design decides when.

An update that changes the tree key is never in place. It is a delete-mark at the old tree key and an insert at the new one, which is why the IOT's locator moves on a primary-key update.

### 5.4 Delete-mark, stamp, purge-compact

- **Delete-mark** (IOT leaf): set the flag bit. One record, naming the tree key. The bit is cleared only when purge-compact removes the entry.
- **Stamp a word** (index leaf): write the entry's delete word. One record, naming the tree key and the word.
- **Purge-compact** (§7): the host says which entries of a leaf are removable, and the tree rewrites the leaf without them.

### 5.5 No merge and no root collapse, but empty leaves are unlinked (B6, B12)

Version 1 never moves an entry from one node to another, never merges two half-full nodes, and never collapses the root. It does remove a leaf that has become **empty** (round-3 fix V11).

**Why the empty-leaf case cannot wait.** The product's showcase workload is PostgreSQL as a job queue (product definition §4.1): rows are inserted with rising keys and deleted within minutes. Purge empties the leaves on the left of the tree, and without unlinking them every scan for the oldest job would cross an ever-growing run of empty pages, whose space is never returned. That is the bloat the product promises to beat.

**Unlinking an empty leaf (B12).** When purge-compact leaves a leaf with no entries, and the leaf is not the only child of its parent, maintenance unlinks it as one structure change (§3.4's protocol, one mini-transaction, record `BT_LEAF_UNLINK`):

1. Make resident and pin, in order: the parent, the left sibling (the parent's previous child, or for the parent's leftmost child, the leaf whose `next_leaf` names it, found by a leftmost descent for the empty leaf's old low key), and the empty leaf.
2. Validate under the pins that the leaf is still empty and the links are as found.
3. Remove the parent's entry for the leaf (or, for the leftmost child, make the next child the new `leftmost_child` and drop its separator); point the left sibling's `next_leaf` at the empty leaf's `next_leaf`; and rewrite the empty leaf as a **tombstone**: an empty leaf with an `UNLINKED` header flag that keeps its `next_leaf`.

Readers stay correct without new latching. A lookup that copied the parent before the change sees its `mod_count` change and starts over (§3.2). A scan holding a copy of the left sibling from before the change follows the old link into the tombstone, finds no entries, and continues along the tombstone's `next_leaf`. The tombstone's page number is not reused until the cleanup watermark has passed the unlink, so no reader can still hold a link to it, and the page store releases its storage meanwhile. A branch never loses its last child, so branches never empty and the root never collapses; the empty leaves left behind are at most one per branch, about one in three hundred.

**What is still accepted.** Half-full leaves are not merged, and the tree keeps its height after a mass delete. A lookup pays the full height, and a range scan over a sparsely populated region crosses half-empty pages. Merge is the hardest part of a B-tree to make correct under concurrency and replay; it is reopened when a deletion-heavy benchmark measures the cost.

---

## 6. What the Engines Build on This

- **`cascade-iot`** (layer §11.3). A leaf row carries the creator word and `roll_ptr`. Insert: an undo record and a leaf-insert record in one group, undo first. Update: §5.3, with an undo delta first. Delete: the mark and an undo record. Read: a lookup, then the undo walk outside the tree, on a copy of the row — no pin is held during the walk. Purge of an undo record the walk still needs is prevented by the cleanup watermark, not by latching.
- **`cascade-index`** (layer §11.4). The host supplies the tree key (§2.3). The unique check is a lookup, which returns the run at the key, and then the host's rule on each entry against the latest committed state, not the statement's snapshot (layer §11.4, round-3 fix V3): an entry another transaction created or deleted and has not ended gives `Conflict::InProgress`; one committed and not deleted by a committed transaction gives `UniqueViolation` (or, for a speculative insert whose snapshot cannot see it, `CommittedAfterSnapshot`); one whose delete committed frees the key. A speculative insert is an ordinary insert whose create word is the writer's; aborting it stamps the delete word (layer §6.9).
- **`cascade-maintenance`**. Purge runs through `Maintenance::step` budgets. The host keeps a list of leaves that hold dead entries — a leaf joins it when a mark or a stamp lands on it — and offers them to purge one at a time.

---

## 7. Purge-Compact and Slot Hygiene

Tree pages have no slot markers: every slot points at an entry.

The host says which entries of a leaf are removable. The tree rebuilds the leaf's tail from the entries that remain, in their existing order; rewrites the slot array to match; sets `entry_count` to their number; resets `free_offset`; zeroes the gap; and, through the record's apply, increments `mod_count`. It first checks the list against the pinned leaf and refuses the whole operation if a named entry is absent.

Slot numbers may change. Nothing outside a page refers to a slot, and a scan resumes by tree key (§5.2).

The host's verdicts travel in the record and are never derived again at replay. Deadness needs the status table, which replay does not have (page D2).

---

## 8. Redo Record Family

Record type bytes extend `cascade-wal-format`'s table and are never renumbered. Proposed allocation, frozen when the family lands: **20–29 for the tree family**. Every record obeys the four redo laws (storage §12.4). Three further rules apply to all of them:

- **Page LSN.** A record leaves each node it changes with the end of that record as its page LSN (layer §9.3).
- **`mod_count`.** A record increments the count of each node it changes by one (§3.1). A record that carries a node's whole image — the split — has the new count already inside that image, and the apply copies it.
- **Idempotence.** The apply checks the record's start against the node's page LSN first and does nothing if the node is ahead.

| Byte (proposed) | Record | Carries | Replay |
| :--- | :--- | :--- | :--- |
| 20 | `BT_LEAF_INSERT` | leaf kind, the tree key, the whole entry | Finds the end of the run at that tree key and appends there |
| 21 | `BT_LEAF_UPDATE` | leaf kind, tree key, which entry of the run, the whole new entry | Overwrites that entry |
| 22 | `BT_DELETE_MARK` | tree key, which entry of the run | Sets the flag bit |
| 23 | `BT_ENTRY_DELETE_WORD` | tree key, which entry of the run, the word | Writes the word |
| 24 | `BT_SPLIT` | the left node's image, the new right node's image, the parent's separator and its position, and for a root split the new root's image and the metapage's | Copies the images |
| 25 | `BT_PURGE_COMPACT` | leaf kind, the entries that remain, in order | Rebuilds the leaf from them |
| 26 | `BT_META` | the metapage's image | Copies it |
| 28 | `BT_LEAF_UNLINK` | the parent's change, the left sibling's new `next_leaf`, the tombstone's image | Applies the three changes (§5.5) |
| 27 | `BT_ROOT_COLLAPSE` | — | **Reserved. Never emitted in version 1** (§5.5) |

**Why a split carries images, not steps.** Replay then copies bytes and contains no tree logic at all, which keeps it on the right side of the determinism law and leaves the tree's logic in one crate. The price is size: a split logs two nodes, up to about 16 KiB, inside the 1 MiB group limit (storage §5.4). §10 says when that would be revisited.

Records 20–23 name "which entry of the run on this page", so that a delete-marked twin and its live replacement at one tree key are told apart, even when the run continues on the next leaf.

---

## 9. Testing

- **Golden bytes** for each kind and each record payload, at their authorities: `cascade-page` for the nodes, `cascade-wal-format` for the records. The landing commits freeze kinds 2–5.
- **Replay equivalence.** Random insert, update, delete and purge workloads through the in-memory seam. Pages rebuilt from the recorded groups equal the pages the leader produced, byte for byte.
- **Tree properties**, after any operation sequence: an ordered walk yields tree keys in non-decreasing order, with equal tree keys in insertion order; every `next_leaf` chain is complete and acyclic; every separator routes correctly to its child; and every node's `mod_count` equals the number of records applied to it.
- **Concurrency, model-checked** (`loom`, architecture Rule 11): the multi-pin order; validate-after-pin for a plain write; a lookup racing a root split; a lookup racing a leaf split.
- **Scans under splits.** A scan's result at a snapshot equals the reference result while splits run.
- **Mark and restore** across splits and purge-compact.
- **Equal tree keys.** Runs are kept in insertion order and returned whole, including a run that a split had to cut across leaves (V10).
- **Empty-leaf unlink.** A scan racing an unlink returns every visible entry exactly once; a lookup racing an unlink restarts; a follower's handle follows root splits whose pages it does not hold (V11, V21).

---

## 10. Open Questions

| Question | Owner, and needed before |
| :--- | :--- |
| The metapage's exact layout | Frozen with kind 5's golden test |
| Separator suffix truncation; leaf prefix compression | Later, as minor format versions, when text-heavy index benchmarks justify them |
| A latch fused into the buffer-pool frame, saving one copy per level | A measured optimization behind this protocol's interface |
| A hot leaf (the rightmost one under sequential inserts) serializes its writers | Acknowledged in layer §8. A mitigation is not designed; it is taken up when a benchmark shows it matters |
| The size of `BT_SPLIT` | Revisited if page-store ingest profiling shows split-heavy logs dominating |

---

## 11. Landing Plan

1. **Prerequisite.** The page design's module split (page L3). The tree consumes the page layer's contract and never a copy of its prologue logic.
2. **Kinds and views.** Kinds 2–5 in `cascade-page`, with typed views, parse and golden tests. Then in `cascade-btree`, over the in-memory seam: descent and validation (§3), lookup, insert and split (§5.1), scan (§5.2), with the tests of §9.
3. **The rest of the surface.** Update in place, marks and stamps, purge-compact, the metapage record.
4. **The hosts adopt it.** `cascade-index` freezes the index entry's bytes; `cascade-iot` freezes the IOT leaf row's.

Each step is one commit with the workspace gate green. Task numbers are assigned when the step is scheduled.

---

## 12. Decisions

Statuses mean: **Proposed**, awaiting the project owner; **Accepted**, by whom and when; **Superseded**, by what.

| # | Decision | Status |
| :--- | :--- | :--- |
| B1 | One tree, one segment. Page-number child pointers. The metapage is page 0 | Proposed |
| B2 | Kinds 2–5. Headers as in §2.2. Full separators. No fill-factor setting | Proposed |
| B3 | Optimistic descent. Readers copy, then validate; writers pin, then validate; no pin across an `await` or a tree level | Proposed |
| B4 | The root pointer and the height are validated on every lookup | Proposed |
| B5 | A split is one mini-transaction and one record, carrying images. It does not cut inside a run | Proposed |
| B6 | No merge and no root collapse in version 1 | Proposed (narrowed in round 3 by B12) |
| B7 | Scans follow `next_leaf`. Mark and restore are anchored to a tree key | Proposed |
| B8 | A non-unique index's tree key is key + locator. Equal tree keys are ordered by insertion, returned as a run | Proposed |
| B9 | Every entry carries its own length | Proposed |
| B10 | `mod_count` lives in the node's image and is incremented by every record that changes the node | Proposed |
| B11 | A structure change makes its pages resident first, then takes all pins fail-fast, then validates under them | Proposed |
| B12 | An empty leaf that is not its parent's only child is unlinked, leaving a tombstone whose page is reused only after the cleanup watermark passes (§5.5) | Proposed (round-3 V11) |
| B13 | Two descent modes: leftmost for lookups and existing entries, rightmost for inserting (§2.2) | Proposed (round-3 V10) |
