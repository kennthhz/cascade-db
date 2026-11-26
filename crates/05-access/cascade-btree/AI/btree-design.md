# cascade-btree design — the B+tree mechanism for the IOT and secondary indexes

**Status:** Draft v0.1 for review, 2026-11-26. This document is the detailed design for `cascade-btree` (layer design §8.1): the B+tree mechanism shared by the IOT primary tree (`cascade-iot`) and secondary index trees (`cascade-index`). It owns the tree — descent, latching, structure changes, ordered iteration, byte-level entry operations — and nothing else: page bytes live in `cascade-page` (A3), record payload encodings in `cascade-wal-format`, redo dispatch in `cascade-redo`, and every visibility decision in the engines above it.

**Upstream:** [`crates/05-access/AI/access-layer-design.md`](../../AI/access-layer-design.md) §5–§6, §8.1 (A2/A3/A4/A9); [`crates/02-storage/AI/storage-layer-design.md`](../../../02-storage/AI/storage-layer-design.md) §12.4 (redo contract), §12.5 (page cache rules), §4.3 promise 5 (mini-transaction boundaries); HLD v2 §11 (IOT leaves) and §13 (index entries) — retired from the tree, engine semantics of record; WP1 F1/F4 (version words, key encoding); the page mutation design L1–L3 (page layer, kind registry, version discipline).

**Depends on:** the landed T6.1 page format and T6.2 BPM access model (pin word, copy-out guards, exclusive-pin mutation, no pin across an await); `cascade-page`'s frame/heap module split (page design L3 — prerequisite for the first btree kind).

---

## 1. Scope: what this crate owns and what it must never grow

**Owns:**

- Tree invariants and shape: node kinds, fill, height, root discovery via the segment metapage.
- Descent and the latch protocol (B3) — optimistic, version-validated, no pin across an await.
- Structure changes (B5): split, root split, root collapse; the no-merge policy (B6) and its revisit condition.
- Entry operations on byte keys and byte values: insert, lookup, ordered scan with mark/restore, in-place update, delete-mark, delete-word stamp, purge-compact.
- The redo *record family design* for tree changes (§8): what each record must carry so replay is deterministic. The bytes of those payloads are `cascade-wal-format`'s; the apply functions are `cascade-redo`'s, written against `cascade-page`'s btree views.

**Must never grow:**

- **Visibility.** Version words in leaf payloads are opaque bytes here. `row_visible`, unique-check arbitration, speculative-insert semantics — the engines' (`cascade-iot`, `cascade-index`). This crate cannot answer "is this entry visible", only "here are the entries at this key".
- **Undo.** `roll_ptr` is payload bytes. Chain walking and delta application belong to `cascade-iot` (WP6).
- **Transactions.** No `TxnCtx`, no status table, no snapshots in any signature. Every operation takes an `OpCtx` (tenant/span/deadline) and nothing else transactional.
- **Key semantics.** Ordering is bytewise on normalized `KeyBytes` (WP1 F4 invariant 7) — this crate never decodes a key into a type.

The crate serves two hosts through one generic tree: the IOT leaf kind and the index leaf kind differ only in their payload codec, which the host supplies as a typed view over `cascade-page` bytes (§2.4). Branch nodes are shared verbatim.

---

## 2. Tree shape and node formats

### B1 — One tree, one segment; the metapage is page 0

A tree lives in exactly one segment: every node pointer is a relation-local `PageNo` (u32) — the same packing family as WP1 F4's `Locator::Heap` and F2's `next` word. Consequences taken deliberately: 2³² pages × 8 KiB = 32 TiB per tree (v1 ceiling, far past any v1 table); one segment per index simplifies `ScopePages` allocation, prefetch (storage `read_pages` is sequential over the segment), and bulk load; cross-segment trees are a post-v1 format change (child pointers would grow to u64 segment+page).

`page_no 0` of every tree segment is the **tree metapage**: root `PageNo`, tree kind tag (IOT leaf vs index leaf payload codec), height, and free-format reserved space. It is an ordinary WAL-logged page — a root split logs a metapage update record in the same mini-transaction as the split, so recovery never reconstructs a root by scanning. In-memory tree handles cache the root; the metapage is re-read on open and after any split that logs a new root.

### B2 — Page kinds and headers (proposed; frozen at landing with golden tests)

Three new kinds under `cascade-page`'s prologue registry (layer design A3; page design L1 assigns per-kind version namespaces). Proposed kind numbers, frozen when the first golden lands: **2 = btree branch, 3 = IOT leaf, 4 = index leaf**. Kinds 5 (tree meta) stays open — the metapage can ship as a fourth kind or as a header-flagged branch variant; the landing review picks, and this document does not pre-grow the registry.

All three keep the layer discipline: bytes are the spec (no `#[repr(C)]`), little-endian, compile-time format locks, **total parse at the boundary**, validate-before-mutate, per-kind `page_lsn` at a fixed offset (page design L2's stamp-position rule), cells/entries growing down from the tail with a slot/index array growing up from the header — the heap format's proven geometry, reused so `apply`-style byte transforms and compact-on-purge (§7) have one shape across kinds.

```text
branch (kind 2)                      IOT leaf (kind 3)                index leaf (kind 4)
0: prologue (v/k)                    0: prologue                      0: prologue
4: entry_count u16                   4: entry_count u16               4: entry_count u16
6: free_offset u16                   6: free_offset u16               6: free_offset u16
8: page_lsn u64 (end LSN, S17)       8: page_lsn u64                  8: page_lsn u64
16: mod_count u64 (B3 latch)         16: mod_count u64                16: mod_count u64
24: reserved 4                       24: next_leaf u32 (B7)           24: next_leaf u32 (B7)
28: leftmost_child u32               28: reserved 4                   28: reserved 4
    (valid iff entry_count > 0)
32: slot array ↑ (2 B each)          32: slot array ↑                 32: slot array ↑
    entries ↓:                           entries ↓ (IOT leaf row):        entries ↓ (index entry):
    [sep_len u16][sep KeyBytes]          [key_len u16][key KeyBytes]      [key_len u16][key KeyBytes]
    [child PageNo u32]                   [creator VersionWord u64]        [locator packed]      (F4 shape)
                                     [roll_ptr u64][flags u16]            [create word u64][delete word u64]
                                     [payload…]
```

Format decisions pinned here so the landing goldens have one source:

- **Branch entries route by right-child convention**: entry *i* holds separator `k_i` and the child that owns keys in `[k_i, k_{i+1})`; the header's slot-0 special case is avoided by storing the leftmost child in the header's reserved field at offset 28 (`leftmost_child u32` when `entry_count > 0`, parse-valid only then). Separators carry **full keys in v1** — no suffix truncation, no prefix compression (both are reserved evolutions: truncation is a minor-bump candidate because separators are routing-only; leaf prefix compression is rejected for v1, revisited with the open question in §10).
- **IOT leaf row header** (HLD §11's shape, F1 words): creator `VersionWord`, `roll_ptr` u64 (encoding owned by `cascade-iot`/WP6 — opaque here), flags u16: bit 0 `DELETE_MARK`, bit 1 `HAS_TOAST`, bits 2..15 reserved zero. There is no separate deleter word: an IOT delete is the delete-mark flag plus the undo record's prior version word (HLD §11; the heap's `xmax` shape is deliberately not copied — different mutation model, the page design §2 cell boundary).
- **Index leaf entry** is WP1 F4's logical shape verbatim: `[key][locator][create word][delete word]`, locator tagged (`Pk` | `Heap`). The delete word is `EMPTY` while live. HLD §13's delta-encoding-against-page-base optimization stays deferred (WP1 F4's stance), reserved via the minor-bump rule.
- **Entry payload ceilings**: entry length prefix is u16 like the heap cell; entries above ~2 KiB are a TOAST problem (IOT: `HAS_TOAST`, `cascade-iot`'s design; index keys: refuse at the descriptor — the catalog layer enforces index key width limits, as PostgreSQL does).
- Fill targets: leaves split at physical fullness (§5.1); branches split when an inserted separator would not fit. No fill-factor knob in v1 — the split-point rule (§5.1) is the only tuning, and it is fixed, not configurable (product pillar: no tuning settings).

### B2.1 — Slot arrays and markers on tree pages

Tree pages reuse the heap's slot-state machine (page design D1) only where its semantics transfer: `Unused` slots after purge-compact, lowest-first reuse. `Redirect`/`Dead` do **not** exist on tree pages — nothing outside vacuum points at a heap slot, but *index entries* point into IOT leaves only by key (never by position), and branch children point at whole pages, so no tree-slot aliasing hazard exists. Purge-compact (§7) rewrites offsets and never renumbers live entries' **keys**; scan resumption (B7) is by key, not slot, so renumbering is invisible to readers.

---

## 3. Descent and the latch protocol (B3)

The BPM's v1 pin word is page-level: exclusive pin or copy-out shared read, `Contended` on conflict, no pin across an await. A btree needs multi-level coupling on top of that, and the no-await-pin rule forces the optimistic shape — a descent that hits a page fault must not hold its parent pinned while it awaits the fetch. The protocol is Optimistic Lock Coupling (Graefe-style) with the node's `mod_count` as the validation word:

```text
descent(key):
  loop:                                   # retry loop; bounded, seeded by SMO frequency
    node = pin_copy(root)                 # copy-out read; includes mod_count
    loop down the tree:
      (child, version) = route(node, key) # pick child by separators
      child_node = pin_copy(child)        # may await on a miss — no pin held from `node`
      node = child_node
      if leaf: return (leaf_image, leaf_version)
```

- **Readers hold nothing across levels.** Each `pin_copy` is a coherent 8 KiB snapshot (BPM copy-out guard); the only cross-level state is the version chain. Before the leaf is *used* (visibility decision, mutation decision), the walker re-reads each ancestor's `mod_count` (one shared copy per level, cheap) — any changed count invalidates the walk and retries. This is OLC's validate-on-use, minus the shared latches: our pin word *is* the short shared latch, taken and released per level.
- **Writers take the exclusive pin only on the node they byte-edit**, after a validated descent. The exclusive pin serializes concurrent writers to that node (the frame mutex under it makes the mutation atomic); between exclusive pin and release there is no await — record-and-mutate is a synchronous block (A1's `MiniTxn::record`/`mutate` never await; only `durable()` does, and pins are dropped before it, layer design §6.1).
- **Concurrent writer vs reader**: the reader either copied before the write (sees the old version, validated stale only if an *ancestor* also changed — splits change ancestors; in-place leaf edits do not, so an in-flight leaf edit is invisible to a validated reader until it re-validates) or after (new `mod_count`, copy sees new bytes). A torn image is impossible: copy-out is under the frame mutex.
- **Concurrent writer vs writer on one node**: the second exclusive pin attempt fails `Contended` and the caller retries its descent (fail-fast is the BPM's contract; the engine's retry is bounded and bias-free — no spin, one yield through `Rt::yield_now` between attempts).
- **Multi-frame writers** (SMOs, §5) pin parent-then-left-then-right, always in that order — the only multi-pin order in the crate, so no cycle exists (Rule 11 model check, §9).

**What lands in the BPM:** nothing yet. The v1 pin word stays; OLC here is a *usage protocol* over copy-out + exclusive pins, not a new latch type. The BPM CONTEXT's "the OLC latch lands with the btree" is satisfied by this protocol; a fused per-node RW-latch inside the frame (saving one memcpy per level) is a measured optimization behind the same interface, not a v1 dependency.

*Why version re-validation and not pin-coupling:* pin-coupling (hold parent shared while pinning child) is impossible without holding a pin across the child's potential `await` — the F3 discipline forbids it structurally. Every surveyed OLC implementation that allows I/O during descent uses exactly this validate-later shape; ours differs only in that the "version" lives in the node image instead of a latch word.

---

## 4. Root discovery and tree open (B4)

`open(segment)` reads the metapage (root, kind, height) and returns a `TreeHandle { segment, root: AtomicU32, meta }`. The root is re-read after any operation that logs a metapage record (root split, root collapse); readers pick up the new root through the atomic on their next descent retry — a stale root merely costs one wasted walk, never a wrong answer (the old root's page remains a valid node; only its *role* changed, and `mod_count`/re-read-from-root catches the routing miss).

---

## 5. Mutations

### 5.1 Insert and split

```text
insert(tree, key, payload):
  loop:
    (leaf, v) = validated_descent(key)
    exclusive-pin leaf
    if fits(key, payload): slot-append entry; record LEAF_INSERT; unpin; return
    else: split(leaf) → retry insert            # B5's split, then the insert re-descends
```

**B5 — a split is one mini-transaction.** The group carries every record the split needs — new-node init, old-node rewrite, parent separator insert, and, when the parent is the root, the new-root init + metapage update — so the durable LSN and every replay boundary never land inside a split (storage-layer-design §4.3 promise 5 names this exact case). Sequence: validated descent → exclusive-pin parent, left, right-new (that order; the new page comes from `ScopePages::extend`'s hint, one `fetch_add`, before pinning) → byte transforms under the pins → release pins → `durable()`.

**Split point** (fixed policy, B2's no-knob rule): split at the median entry for random workloads; when the inserting key sorts **past the last entry** (the append pattern), split so the new right node receives only the new key — the sequential-insert optimization that keeps append-heavy loads at ~full pages instead of ~half. Detection is per-split (compare against last entry), not a remembered mode.

**Root split**: when the splitting node is the root, the group adds the new-root init and the metapage record; the in-memory handle's root atomic updates at group apply.

### 5.2 Lookup and ordered scan

`lookup(key)` is a validated descent + binary search over the leaf's slots (entries are slot-indexed, keys in slot order). Scans (B7) walk leaves in key order: position by descent to the first key ≥ the range's lower bound, emit entries, then follow `next_leaf`. A concurrent split ahead of the cursor is benign — the split copies entries *forward* and the cursor re-validates by key (below) — a split *behind* the cursor cannot remove entries the cursor hasn't emitted (splits never delete).

**B7 — mark/restore is key-anchored.** `mark()` saves the last emitted key (and its page, as a hint); `restore()` re-descends to the first key ≥ the marked key. This survives splits, compactions, and renumbering by construction, which is why §B2.1 can allow purge-compact to renumber slots freely. Cost: one descent per restore instead of an O(1) positional restore — accepted; merge-join restore frequency is bounded by the join, and `supports_mark_restore()` is advertised statically (`AmCapabilities`).

### 5.3 In-place update (IOT leaves only)

`update_in_place(key, new_payload)` — the HLD §11 update path, exposed byte-level: validated descent, exclusive pin, and:

- `len(new) ≤ len(old)` → overwrite the payload in place; the length prefix and slot offset stay; the freed slack stays where it is until purge-compact. Record `LEAF_UPDATE` (carries the new bytes; replay overwrites the same region — deterministic because the record names the key and replay locates the entry by key).
- `len(new) > len(old)` and the slack fits → same, with the entry's slot offset rewritten (the entry grows downward; still one page, one record — `LEAF_UPDATE` carries the new full entry bytes and replay rewrites the whole entry region for that key).
- Otherwise → the engine's fallback: delete-mark + insert of the new version at the same key position (two records in the same group). The btree exposes this composition; `cascade-iot` decides when to use it (its WP6 design).

An update that changes the **key** is never in-place — that is delete-mark + insert at the new key, the locator-move case (`WriteOutcome::Done` with the new `Pk`), engine-composed from the same two primitives.

### 5.4 Delete-mark, delete-word, purge-compact

- **Delete-mark** (IOT leaf): set the flag bit — one u16 write under the exclusive pin, record `DELETE_MARK` (names the key). Clearing it happens only in purge-compact (physical removal).
- **Delete-word stamp** (index leaf): write the `delete word` u64 — record `ENTRY_DELETE_WORD` (key + the word). Same shape as the heap's `set_xmax`, on a different kind.
- **Purge-compact** (§7): the engines compute, per page, which entries are physically removable (their verdicts — dead delete-marked rows, entries with words older than the cleanup watermark); the btree rewrites the page keeping surviving entries verbatim, renumbers slots, resets `free_offset`, zeroes the gap — the heap's compact-on-prune transform (page design D3) generalized to tree kinds, with the same law: **verdicts computed by the engine, never replayed** (page design D2's redo argument applies verbatim: deadness needs the status table, which replay lacks). Record `PURGE_COMPACT` carries the keep-list implicitly by carrying the surviving entries verbatim; replay rebuilds the page from the record + the old image deterministically.

---

## 6. What the engines build on this

- **`cascade-iot`** (WP6): leaf rows carry the creator word + `roll_ptr`; insert = `LEAF_INSERT` + undo-append in one group (undo-before-leaf, layer design §6.1); update = §5.3 with an undo delta first; delete = §5.4's mark + undo; read = validated descent, then the undo walk *outside* the tree (the engine applies deltas to a copied row buffer — no page pin during the walk; a concurrent purge of an undo segment the walk still needs is prevented by the cleanup watermark, not by latching).
- **`cascade-index`**: entries per §2; unique check = validated descent to the key, decode entries, `row_visible(create, Some(delete))` per entry — `Conflict::InProgress` (uncommitted-other) or `UniqueViolation` (committed-live); speculative install = `LEAF_INSERT` of an entry whose create word is the writer's own uncommitted word (the arbiter is the word, so a conflicting inserter's check finds it — the PG speculative shape with our version words, WP5 D12's engine half).
- **`cascade-maintenance`/purge**: drives §5.4 through `Maintenance::step` budgets; the btree exposes purge as per-page work items discovered by an amortized scan (dirty-leaf bitmap maintained by delete-mark/delete-word operations — a page enters the purge candidates list when a mark/stamp lands on it).

---

## 7. Purge-compact and slot hygiene

Inherits the page design's compact-on-purge decisions, generalized: surviving entries are copied **verbatim** into a fresh tail run; slot values are rewritten; `entry_count` shrinks only by trailing-`Unused` truncation (slot indices of surviving entries are preserved — not for locator stability, which keys provide, but so replay's keep-list indices are unambiguous); `free_offset` resets exactly; the gap zeroes. `mod_count` increments — in-flight validated descents that already passed this page re-validate on use and retry. The transform validates the whole plan against the current image and refuses atomically (the layer's validate-before-mutate rule, page design L2).

---

## 8. Redo record family

Record type bytes extend WP1 F3's extend-never-renumber table. **Proposed allocation: heap family 10–19 (heap design §9), btree family 20–29, undo family 30–39 (`cascade-iot`/WP6), index shares the btree family, FSM 40–41 (heap design §7); frozen at landing.** Every record obeys the four §12.4 rules; each names its target page(s); payloads are `cascade-wal-format`'s.

| Byte (proposed) | Record | Carries | Self-containment |
| :--- | :--- | :--- | :--- |
| 20 | `BT_LEAF_INSERT` | page kind (3 or 4), full entry bytes, insert position as **key** | Replay locates the position by key in the old image and inserts. Index-naming would also be deterministic (the LSN-ordered replay sees this group at its exact prefix, so no concurrent group can have touched the page), but key-naming is kept as defense-in-depth and for diagnosability — a replay that cannot find its key fails loudly instead of inserting at a shifted position |
| 21 | `BT_LEAF_UPDATE` | kind, key, full new entry bytes (and old length) | Overwrite-in-place by key |
| 22 | `BT_DELETE_MARK` / `BT_ENTRY_DELETE_WORD` | kind, key, the flag/word | One-field rewrite by key |
| 23 | `BT_SPLIT` | left page, new page no, split key, both nodes' post-images (verbatim), parent page + inserted separator, optional new-root image + metapage image | The one record carries the whole SMO; replay applies node images verbatim — deterministic by construction, no routing logic on the replay side |
| 24 | `BT_ROOT_COLLAPSE` | old root, new root, metapage image | Verbatim images |
| 25 | `BT_PURGE_COMPACT` | kind, page, surviving entries verbatim, new `free_offset` | Verbatim rebuild |
| 26 | `BT_META` | segment, metapage image | Root/kind/height rewrite |

The SMO-as-one-record choice (`BT_SPLIT` carrying post-images instead of step-by-step deltas) is deliberate: replay applies images and stays free of tree logic entirely — the `cascade-redo` btree family is then byte-copies plus header stamps, the simplest possible apply, at the cost of larger records (a split logs two full node images, ~16 KiB worst case). Ingest bandwidth is the page store's budget, not the commit path's; the alternative (delta SMO records) would put tree logic in replay, which §12.4's determinism rule makes risky and Rule 5 makes multi-owner. Revisit only if page-store ingest profiling ever shows split-heavy logs dominating.

*Idempotence*: every record's apply checks `record.lsn ≥ page_lsn` (S17 end-LSN rule) and no-ops otherwise — the same caller-side check the heap family uses (page design D4's stance: the format primitives are LSN-agnostic; the replay loop owns the check).

---

## 9. Testing

- **Golden bytes** per kind and per record payload at their authorities (`cascade-page` views, `cascade-wal-format` payloads) — the landing commits freeze kinds 2/3/4 (and the metapage form) with byte-literal tests, per the layer discipline.
- **Replay equivalence**: random insert/update/delete/purge workloads through the fake `MiniTxnScope`; pages rebuilt by applying the recorded groups to empty images must be byte-identical to the live pages.
- **Tree property tests**: after any operation sequence, an ordered walk yields sorted unique keys; every `next_leaf` chain is complete and acyclic; every branch separator routes correctly against its children (checked exhaustively on small trees).
- **Concurrency**: `loom` on the multi-pin order (parent→left→right) and the validated-descent retry loop (Rule 11); seeded interleaving tests for reader-vs-split (a validated reader never observes a half split — structurally impossible, asserted by re-reading the post-split invariants mid-suite).
- **Engine differential**: the same logical workload through IOT leaves and (fake-coded) index leaves agrees on observable outcomes; the generic `AnyTable` suite covers the contract promises.
- **Mark/restore under concurrent SMOs**: restores land on the same logical position across splits and compactions (the key-anchor property, B7, tested directly).

---

## 10. Open questions

| Question | Owner / needed before |
| :--- | :--- |
| Metapage: fourth kind vs header-flagged branch | Landing review of this document — a format one-way door, decided before the first golden |
| Merge/rebalance (underfull pages after deletion-heavy workloads) | Deferred by B6; revisit when a deletion-heavy benchmark shows height inflation or scan slack past a bound |
| Leaf prefix compression; separator suffix truncation | Post-v1 format evolutions (minor-bump candidates); revisit with text-heavy index benchmarks |
| Fused OLC latch inside BPM frames (one memcpy per level saved) | Measured optimization behind B3's interface; BPM CONTEXT's open question resolves to "this protocol now, fusion if profiled" |
| Hot-node contention (single root/branch under one skew hot spot) | The contention budget (layer §5) covers the commit path; a hot *leaf* is the engines' partitioning concern, not the tree's |
| `BT_SPLIT` record size (two node images) vs delta SMO records | Revisit on page-store ingest profiling only (§8's rationale) |

---

## 11. Landing plan

1. **Prerequisite**: `cascade-page`'s frame/heap module split (page design L3) — the btree consumes the layer contract, never a forked prologue.
2. **Kinds + views**: kinds 2/3/4 (metapage form decided), typed views + parse + goldens in `cascade-page`; `cascade-btree` gains descent, insert/split, lookup, scan (B1–B3, B5, B7) over the fake `MiniTxnScope`, with the full test battery of §9.
3. **The rest of the surface**: in-place update, marks/stamps, purge-compact, root collapse, metapage records (§5.3–§5.4, §8) — landed with replay-equivalence from the first commit, not retrofitted.
4. **Consumers**: `cascade-iot` (WP6) and `cascade-index` adopt the tree; their landing freezes the undo family (30–39) and the index-entry specifics respectively.

Each step is one commit with the workspace gate green; task numbers are assigned when the landing plan is authored (the M-plan discipline).

---

## 12. Decisions

| # | Decision | Status |
| :--- | :--- | :--- |
| B1 | One tree = one segment; u32 relation-local child pointers; metapage at page 0 as the recoverable root | Proposed — DRAFT |
| B2 | Kinds 2/3/4 (branch, IOT leaf, index leaf); headers per §2; full separators in v1; no fill-factor knob; append-aware split point | Proposed — DRAFT |
| B2.1 | Heap slot-state machine reused only for Unused; no Redirect/Dead on tree pages | Proposed — DRAFT |
| B3 | Optimistic Lock Coupling over copy-out + exclusive pins; version re-validation on use; no pins across levels or awaits; fixed multi-pin order | Proposed — DRAFT |
| B4 | Root discovery via the metapage; in-memory root atomic, re-read after root-changing groups | Proposed — DRAFT |
| B5 | Split = one mini-transaction, one `BT_SPLIT` record carrying post-images | Proposed — DRAFT |
| B6 | No merge in v1; root collapse only | Proposed — DRAFT |
| B7 | Scans walk `next_leaf`; mark/restore is key-anchored | Proposed — DRAFT |

Carried from the crate's CONTEXT (resolved here): key-prefix compression — rejected for v1 (§2, §10); suffix truncation — deferred as a minor-bump candidate; hash variant — sibling crate behind `cascade-index`'s seam when needed (layer design §13), never a tagged enum inside this crate (a hash table is not a B+tree; the only shared code would be the page-frame discipline).
