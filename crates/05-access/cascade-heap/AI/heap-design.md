# cascade-heap design — the heap engine

**Status:** Draft v0.1 for review, 2026-11-26. This document is the detailed design for `cascade-heap` (layer design §8.2): the PostgreSQL-style heap engine — append-only row versions, HOT chains, incremental prune and vacuum — as the compatibility engine behind the `AnyTable::Heap` arm. The *format* half of the heap (the kind-1 page, slot states, prune plans, redo byte transforms) is already designed and golden-pinned in [`cascade-page/AI/storage-page-mutation-design.md`](../../cascade-page/AI/storage-page-mutation-design.md) (decisions D1–D6); this document is the *engine* half — DML semantics, visibility walks, free space, vacuum, TOAST, and the landing plan from the T6.4 interim slice to the full MVCC engine.

**Upstream:** [`crates/05-access/AI/access-layer-design.md`](../../AI/access-layer-design.md) §4 (contract), §5 (execution model), §6 (mini-transactions); [`crates/02-storage/AI/storage-layer-design.md`](../../../02-storage/AI/storage-layer-design.md) §12.1–§12.6; HLD v2 §12 (heap engine — retired from the tree, semantics of record); WP1 F1/F2/F4 (version word, row header, key/locator encoding); WP2 D1–D5 (commit section, status table, abort path); the page mutation design D1–D6.

**Depends on:** the landed T6.1 page format; T6.2 BPM; `cascade-redo`'s sequential-fill routing law for the type-5 record (H9); the `AnyTable` contract (layer design §4); the mini-txn seam (§6.1) and `VisibilitySource` (§4.5) contracts; `cascade-index` for vacuum's index-cleanup step (its only cross-engine dependency, landing at migration step L8).

---

## 1. Scope: engine semantics vs the locked format

The page mutation design owns the bytes: the kind-1 layout, the D1 slot-state machine (Live / Redirect / Dead / Unused), the D2 plan-carried prune, D3 compact-on-prune, D4's redo primitives (`apply_heap_insert`, `apply_delete_mark`, `apply_relink`, `apply_prune`, `apply_slot_free`), and D5's traversal vocabulary. This engine must obey those bytes and may not amend them — a format change is that document's to make. What this document adds: who calls each primitive and when, what the engine computes vs what it applies, the relation-level structures (segments, FSM), the DML conflict paths, TOAST, and the landing sequence.

Two corrections this engine makes to the *pre-pivot* world still visible in the crate's stubs and old CONTEXT (fixed by the layer design's §12 sync list): the heap does **no in-place updates and keeps no undo** — every update writes a new version, and `cascade-undo`/`cascade-mvcc` leave its dependency list (rollback is the abort record plus status-table invisibility, WP2 D5; HLD §12's "Abort: write the abort record only"). The row header's third word is the forward `next` chain pointer (F2), not an undo reference.

---

## 2. Relation structure (H1)

A heap relation has one **main segment** (and, once TOAST lands, one hidden TOAST segment — §8). Pages are addressed `(segment, page_no)` within the scope; the scan order is `page_no` order from 0 through the segment's high-water mark. No page chain exists — segment contiguity *is* the order, which is what makes the storage layer's vectored `read_pages` the scan's natural miss path.

- **Segment allocation** goes through `ScopePages` (layer design §7 / A6): `new_segment` at relation creation and at `truncate`; `page_hint` is the engine's append cursor into the segment — one `fetch_add` per extension (the execution model's single contended atomic is shared with the log writer's; they never contend on the same word).
- **Placement while type 5 is the record (H9).** `HEAP_INSERT` names a segment, not a page, so replay has to derive the page — and `cascade-redo` fixes how: the **sequential-fill rule**, under which a segment's inserts fill page 0, 1, 2, … by the page format's exact append arithmetic, spilling whole cells (`SegmentFill::route` is the single authority). Its law binds this engine: *the heap appends under the same rule, or accepts that replay re-lays its rows*. Until the type-10 switch (§9) there is therefore no FSM choice, no slot reuse and no prune — every insert goes where the fill cursor says. The cursor must also advance **in LSN order**: the page is chosen inside the same critical section as the record's LSN reservation (a short per-relation mutex over reserve → route → fill, never held across an await). Two cores that reserve LSNs in one order and pick pages in the other give the leader a layout replay cannot reproduce — and once storage M8 lands, the leader refetches an evicted page *from the page store*, in replayed layout, so a scan crossing an eviction could skip or repeat rows and a `fetch(Tid)` could return a different row. On open, the cursor is rebuilt with `SegmentFill::from_page_header` from the segment's open page. The mutex is an insert-path lock, accepted because the record format leaves no alternative; it is deleted at the switch. *Decided 2026-11-26 (Kenneth). This replaces the M6 plan's T6.4 acceptance that "live layout may differ from replay layout", which predates the page store serving pages back to the leader; the plan's FR-46 is updated to match.*
- **Placement after the switch (type 10 and later).** §4.1's insert path and §7.2's FSM apply from here on, because the record names its page and slot and replay reproduces placement from the record: inserts try the FSM's candidate, then the hint page, then walk forward; when no page in the segment has room (after prune — §7), the engine extends by one page and retries once, then returns the relation-full outcome mapped to PostgreSQL's error at the executor boundary. Fullness is always judged *after* a prune attempt (D3's compaction is where append space comes from), never before.
- **A heap page is self-describing** (kind 1): no per-relation header page, no metapage — the btree's B1 has no heap analogue because the heap's only relation-level state is the segment id and the hint, both in engine memory and reconstructible from the segment extent.
- **Tids are stable for the life of their slot's data** (page design invariant 5, HLD §12 invariant): slot indices never renumber, never shrink; `(page_no, slot)` is the locator index entries carry (F4 `Locator::Heap`), and reuse only through Unused after vacuum's index cleanup (§7.3).

---

## 3. Row identity, locators and the chain law

`RowLocator::Tid(Tid)` names **a version**, and the walk goes forward from it (`next` links run old→new), evaluating versions until the snapshot's answer appears — D5's vocabulary: `slot_state` branch, `Redirect` jump, `HAS_NEXT` hop, cross-page via the relation-local `PageNo`. Index entries name the version that was current when the entry was written: a HOT update writes no entries, so its entries keep naming the original version (the chain root in the HOT sense); a non-HOT update rewrites entries to name the new version — which is exactly why *the locator moves on a non-HOT heap update* (WP5 D2/HLD §10). Scans emit the visible version's own Tid, and every consumer resolves every locator by the same forward walk, so old locators stay resolvable through the chain for their validity window. Two laws bind the walk (page design invariants 5/8, HLD §12):

1. **A HOT chain never crosses a page** — enforced at update time (§4.2 chooses same-page HOT only when the new version fits); the derivation `HAS_NEXT && next.page_no == own page` (F2) is the format's `is_hot_link` convenience, and this engine is its only producer of the `HEAP_ONLY` flag.
2. **A fetch on Dead resolves to NotVisible, deterministically** (D1) — never to whatever bytes a later append leaves in the region. The engine's walk treats `Dead` as terminal-and-invisible; `Unused` is an engine bug (assert — the D1 transition table's `apply_slot_free` refusal is what keeps this true).

Locator validity is the contract's (WP5 D2): same-statement, same-txn; movement only via `WriteOutcome::Done` — and a non-HOT heap update **moves the locator** (the new version's root is the new tuple), which is exactly why `Done { locator }` exists.

---

## 4. DML paths

All four paths share the prelude: epoch/scope debug assert (layer design §5), the mini-txn seam (§6.1 of the layer design), and the conflict protocol (§4.4) — a writer touching a row whose newest version carries an uncommitted word of *another* txn resolves it through `VisibilitySource` + wait policy into `WriteOutcome` values, never errors.

### 4.1 Insert

```text
insert(txn, row):
  page = fsm.find(needed) else hint page          # §7.2
  exclusive-pin page
  if low-on-space: plan+apply prune (§7.1)        # in the same txn group (D2/D3)
  slot = append_cell(uncommitted(txn.xid), payload)   # or append_into(slot) reusing Unused, lowest-first
  record HEAP_INSERT_V2 (segment, page, slot, cell bytes verbatim)   # §9 — one code path with cascade-page's encode_cell
  unpin; durable at commit
  return Done { locator: Tid(page, slot) }
```

Index maintenance (WP5 D5) happens inside the same statement before `Done` is returned: every secondary index — including the heap's PK index, which is just a unique secondary index (HLD §13) — receives its entry through `cascade-index`. The unique-check and speculative arbitration are that crate's; the heap's contract obligation is that the entry lands *after* the tuple exists in the same statement, so an index fetch never misses its tuple (HLD §13 invariant: an entry is never visible before its row — guaranteed here because both are uncommitted-own until commit, and visibility resolves them together).

### 4.2 Update — the HOT decision

```text
update(txn, loc, new_row, changed):
  old = resolve_chain(loc)                         # walk from the root to the newest version (§5.2)
  conflict = check_newest(old)                     # §4.4; returns per WaitPolicy
  if conflict: return it
  hot = changed ∩ indexed_columns == ∅  &&  fits_on_page(old.page, new_row)
  if hot:
      new_slot = append/append_into on old.page    # same page as the old version, uncommitted(own) creator
      group = [ DELETE_MARK(old, xmax=uncommitted(own)),
                RELINK(old, next=chain(old.page, new_slot, HEAP_ONLY)),
                HEAP_INSERT_V2(new_slot, cell) ]
  else:
      new_page = fsm.find/extend; new_slot = append there
      group = [ DELETE_MARK(old, xmax=uncommitted(own)),
                RELINK(old, next=chain(new_page, new_slot, 0)),     # crosses the page: not HOT
                HEAP_INSERT_V2(new_slot, cell) ]
      + index: delete-word every entry of the old locator; insert entries → new locator
  return Done { locator: hot ? loc : Tid(new_page, new_slot) }
                 # hot: entries and locators unchanged — that is the point of HOT;
                 # non-HOT: moved (the new version is where entries now point)
```

The three records are **one mini-transaction group** — the old version's death, the chain hop, and the new version's birth are never separable at a replay boundary (the group-end discipline, layer design §6.1). The chain law makes `RELINK` and `DELETE_MARK` same-page ops on `old`; only the insert may target another page.

### 4.3 Delete

Resolve the chain, run §4.4, then one `DELETE_MARK(old, xmax = uncommitted(own))` group plus index delete-words. The tuple's bytes stay (D2: prune never touches uncommitted verdicts; the deleter may roll back and an RC re-check may need the stub — page design D2's eligibility rule).

### 4.4 The conflict protocol (WP5 D3, instantiated)

The newest version's words decide:

| Newest version state | `Wait` | `DontWait` | `SkipLocked` |
| :--- | :--- | :--- | :--- |
| `xmax` uncommitted-other, row live | bounded wait on holder via `VisibilitySource`/lock queues, then proceed / `CommittedAfterSnapshot { commit_ts, latest }` | `Conflict::InProgress { holder }` | row treated `NotVisible` |
| `xmin` uncommitted-other (creator in flight) | same wait table | `InProgress` | `NotVisible` |
| `xmax` committed after our snapshot | `CommittedAfterSnapshot` with the newest committed version's locator via `fetch_latest_committed`'s walk | same value, no wait | `NotVisible` |
| gone (chain ends dead / root Dead) | `WriteOutcome::NotVisible` | same | same |

Bound expiry under `Wait` returns `InProgress` (bounded waits; the bound is transaction-layer configuration, WP2's). Reads answer not-visible with `None`; writes with `NotVisible` — the asymmetry WP5 D3 pinned. `lock_row` runs this protocol with no tuple change and returns `Locked { outcome, row }` (row present iff acquired — SELECT FOR UPDATE costs one descent).

### 4.5 Speculative insert (WP5 D12, engine half)

The heap side of `insert_speculative` is an **ordinary insert** — the tuple is uncommitted-own and already invisible to others. All arbitration lives in the PK index entry (create word = own uncommitted txn), so a conflicting inserter's unique check resolves it to `KeyBusy(InProgress { holder = us })` and waits on us through the status table. `complete_speculative` changes no bytes (the word is already right; the commit does the rest); `abort_speculative` changes no bytes either — the statement drops its claim on the index entry, the tuple's creator word resolves to `Aborted` at rollback, and prune/vacuum reclaim both as ordinary dead food (§4.3's abort rule extends naturally: an aborted speculative insert is just an aborted insert). This is why the M6-era stub can reserve the speculative API as compile-checked signatures with `UniqueViolation`-only behavior (layer design §10 L1) and fill the semantics in at the MVCC milestone without touching the executor surface.

---

## 5. Reads

### 5.1 Sequential scan

Walk pages in `page_no` order; per page take a copy-out read (no pin held across the batch boundary — the scan yields through `Rt::should_yield` between pages, and the resume hint is the next `page_no`); per slot: `slot_state` branch — `Unused` = assert-bug, `Dead`/`Redirect` = skip (scan reads cells, and `iter()` is live-only per D1), `Live` = decode the cell, `row_visible(xmin, xmax_nonempty)` decides emit/skip, emitting the visible version's **own Tid** as the locator (§3's law — the executor's later update/delete resolves it by the same forward walk). Each visible version is at most one per logical row per snapshot by construction (older versions are `xmax`-stamped or chain-hidden), so the scan emits rows, not versions. `SeqScanOpts.resume` carries the page hint for a resumption after yield or a restart-driven rescan.

### 5.2 Fetch by Tid and the chain walk

`fetch(snap, Tid, proj)`: loop — `slot_state`: `Unused` → bug-assert; `Dead` → `None`; `Redirect(t)` → `slot = t`; `Live` → decode, evaluate `row_visible(xmin, Some(xmax))`: `Live` → materialize `RowRef` and stop; `Deleted` (the deleter is committed at or before `read_ts`) → stop with `None` — every later version is younger, so the row did not exist at this snapshot; `NotBornYet` (the creator is younger than `read_ts`) → hop forward (`next.slot()` same page, else open `next.page_no()`) — the snapshot's version, if any, is further down the chain. Chain end → `None`. (How `row_visible` classifies an *in-flight* deleter — live-at-snapshot vs `Deleted { in_progress }` — is the WP5 D4 composition's decision, not this engine's; the walk mechanically follows whatever it returns.) The loop is bounded by the chain length; a cycle is a corruption (a same-page cycle asserts; the format's parse refuses cross-reference nonsense).

`fetch_latest_committed` (the RC re-check recipe, WP5 D6): walk the chain from the locator collecting versions; resolve every uncommitted word through `VisibilitySource` **under the `Wait` policy** (the re-check waits for in-flight writers by contract); return the newest version visible to `{own uncommitted writes ∪ committed ≤ now}` plus **its own Tid** as the current locator — the executor re-evaluates its predicate on it and decides (EPQ-style). The walk is the engine's most subtle path and gets the dedicated property tests of §10.

### 5.3 RowRef and projection

`RowRef` owns its decoded projection buffer (copy-out discipline — no reference into any page survives the call) plus the engine handle and locator for lazy TOAST materialization (WP5 D15, §8). `ColumnSet` for updates is the changed-column bitmap the executor supplies; the engine derives indexed-column membership from the descriptor (A5).

---

## 6. Visibility integration

The engine holds no visibility logic of its own: `Snapshot` + `row_visible` (layer design §4.5) with the injected `VisibilitySource` (A8) are the whole rule. Engine-side obligations only:

- **Stamp discipline**: creators are stamped `VersionWord::uncommitted(txn.xid)` at write time; deleters `uncommitted(own)` at delete/update time. No committed word is ever written by this engine in the MVCC milestone — committed resolution is the status table's (WP2 D2); whether commit-time or lazy stamping ever rewrites words is the shared open question of §11 (the engines' constraint is recorded there).
- **Own-writes visibility** comes from `Snapshot.own` — the engine never special-cases "my tuple".
- **The status table is the engine's only deadness oracle**: prune eligibility (D2) consults it plus the cleanup watermark, both injected; on a follower, the same interfaces run over the rebuilt status state (architecture §4.5 follower role) — the engine needs no role awareness beyond the mini-txn seam's follower mode (page changes arrive from the `FollowerFeed`; the engines' mutation paths simply don't run).

---

## 7. Prune, vacuum and the free space map

### 7.1 Prune — trigger and plan computation

Trigger (D3's "when the page is nearly full", made concrete): on an insert/update path pin of page P, when `free_bytes(P) < needed` — before declaring the page full — and from `Maintenance::step`'s background budget for pages on the dead-food list. The engine computes the `PrunePlan { free, redirects, relink }` (D2): walk each chain from its root; a version is prune-eligible iff its creator and deleter (if stamped) resolve committed/aborted **older than the cleanup watermark** — uncommitted stamps are never eligible (D2's rule, restated as the engine's eligibility law). Chain mechanics follow the worked example of the page design (§4): same-page successor → Redirect the root and free the middle versions; cross-page successor → retain the stub; standalone dead tuples → Dead; surviving cells whose successor was removed get `relink`. The plan applies through `PageMut::apply_prune` (validate-then-atomic, D2) inside the access's mini-txn group — the plan is also the redo record (§9), so replay reproduces the verdicts without engine state.

The **cleanup watermark** in v1 is the scope's oldest active read timestamp (layer design §9.4); the engine takes it as an injected `fn watermark() -> Hlc`, never deriving it.

### 7.2 The free space map (H5)

FSM pages are ordinary WAL-logged pages (HLD §12) — proposed kind 6, header-only layout per FSM page carrying an array of `(page_no u32, free_bytes u16)` entries at 32-byte granularity (lossy by design: a stale "has room" costs one failed append and a prune attempt; a stale "full" costs a skipped page — both benign, which is what lets the FSM stay simple). FSM pages chain linearly; the map is small (one entry per heap page; a 1 M-page segment = ~6 MiB of FSM). Updates are `FSM_UPDATE` records (§9) — verdict-shaped like everything else, applied verbatim. The engine's `fsm.find(needed)` returns the best candidate ≥ needed, falling back to the hint walk; `fsm.update` runs after every prune/append that changes a page's free bytes materially (past the 32-byte granularity).

### 7.3 Vacuum — the incremental, index-ordered cleaner

`Maintenance::Heap(HeapPruneVacuum)::step(budget)` — one bounded unit per call, driven by `cascade-maintenance` (layer design §9.4; interim driver: the runtime housekeeping tick):

1. Pick a candidate page: one with `Dead`/`Redirect` slots (the list prune appends to).
2. **Index cleanup first**: for every slot the page would free, find and remove (delete-word→purge) index entries pointing at `(page_no, slot)` — via `cascade-index`'s per-page entry scan (its own incremental step, budgeted). This ordering is the HLD §12 invariant ("an index entry pointing at a TupleId is removed before or with that slot's reuse") made structural: the engine does not call `apply_slot_free` until the index work for that slot has completed *and been logged* in an earlier durable group — so a crash between the two never leaves a freed slot under a live entry.
3. `apply_slot_free` per freed slot (Dead → Unused; Redirect → Unused only for roots whose row is fully gone — the D1 table).
4. `FSM_UPDATE` for the page's new free bytes.

No table-wide pass, no lock phase, no freeze — 63-bit txn ids make wraparound machinery not exist (HLD §12; product pillar §4.1). The step is resumable at page granularity; `Progress` reports pages cleaned and entries removed.

---

## 8. TOAST (WP5 D15's engine half)

Out-of-line storage for oversized values, in the relation's **hidden TOAST segment of the same scope** (HLD §15: same scope, same WAL, same key — a write stays single-scope). Reserved now, landed with `cascade-index` (the chunk lookup needs a keyed structure — the TOAST relation is a hidden heap table with a unique index on `(chunk_id, chunk_no)`, landing at migration step L8 rather than growing a special-purpose page kind):

- **The pointer** (reserved format, frozen when TOAST lands): payload bytes where the value would be are `[magic u32][chunk_id u64][total_len u32]` — the `HAS_TOAST` flag in the row's `next` word (F2) says the payload contains pointers. The magic word and the flag are the only format commitments made before landing; they cost nothing while unused and are the D6-style reservation this design makes now.
- **Threshold**: v1 payload ceiling per column value (descriptor's `ToastPolicy`); no compression in v1 (deferred with the chunk-format decision).
- **Materialization**: `RowRef::value` returns `Err(ToastNeeded)` for a pointed column; `toast_value` resolves the chunks through the TOAST index and inlines them into the `RowRef`'s owned buffer. Chunks of one value are written in the insert's statement, in one mini-txn group per chunk batch; deletion of the parent row delete-marks the chunk rows in the same statement's work (their cleanup is ordinary vacuum food).

---

## 9. Redo records (heap family)

Type bytes extend WP1 F3's table — extend, never renumber. **Proposed allocation (frozen at landing): heap 10–15, FSM 40** (the ranges the btree design §8 fixes for its family and the undo family are untouched). Payload encodings live in `cascade-wal-format`; the apply functions are already designed — the page's `redo` module (D4) — and `cascade-redo`'s heap family is the thin dispatch over them (layer design A2).

| Byte (proposed) | Record | Payload | Replay primitive |
| :--- | :--- | :--- | :--- |
| 5 | `HEAP_INSERT` (landed) | `[segment u32][cell verbatim]` | `apply_heap_insert` — retained forever for replay of existing logs; **emission stops** at the MVCC milestone |
| 10 | `HEAP_INSERT_V2` | `[segment u32][page_no u32][slot u16][cell verbatim]` | `apply_heap_insert` + placement — **physiological**: names the slot, replay reproduces live layout exactly (the page design §7 forward obligation, resolved the moment index entries carry Tids; this design lands it at the MVCC milestone so the format switches once) |
| 11 | `HEAP_DELETE_MARK` | `[segment][page_no][slot][xmax word]` | `apply_delete_mark` |
| 12 | `HEAP_RELINK` | `[segment][page_no][slot][next word]` | `apply_relink` |
| 13 | `HEAP_PRUNE` | `[segment][page_no][PrunePlan verdicts]` | `apply_prune` — verdicts, never watermarks (D2) |
| 14 | `HEAP_SLOT_FREE` | `[segment][page_no][slot]` | `apply_slot_free` |
| 40 | `FSM_UPDATE` | `[segment][fsm_page][entry]` | verbatim entry rewrite |

Idempotence is the replay loop's LSN check (S17); every record is self-contained per page and deterministic (the D2/D4 arguments). A HOT update is one group of records 11+12+10; a non-HOT update adds the index family's records to the same group — the group is the atomicity unit, nothing below it exists at a replay boundary.

**The producer gate, and a defect it exposes (found 2026-11-26, reproduced against the T6.1 code).** `encode_cell`'s ceiling is the u16 length field — 65 509 payload bytes — far above what a page can hold, so the engine gates at the page before logging anything: the largest cell a fresh page holds is **8158 bytes whole, 8132 of payload**. A row past that is a TOAST candidate (§8) and, until TOAST lands, is refused with PostgreSQL's `54000` at the executor boundary. But the landed fit rule does not enforce that ceiling correctly. `PageMut::append_cell` accepts a cell when `free_offset − len ≥ 32 + 2·slot_count`, and then writes the new slot's own 2-byte entry at exactly `32 + 2·slot_count`. For the two lengths where `free_offset − len` equals that address or is one above it — a 8159- or 8160-byte cell on a fresh page; two lengths per page state in general — the slot entry overwrites the cell's length prefix and the header contradicts itself: `append_cell` returns `Ok`, and `PageView::parse` then refuses the image as malformed. The rule must reserve the entry it is about to write: `free_offset − len ≥ 32 + 2·(slot_count + 1)`. Until that lands in `cascade-page`, this engine applies the corrected check itself and treats a miss as `Full`. The in-flight `cascade-redo` mirrors the uncorrected arithmetic (`fits`, `SegmentFill`, `MAX_CELL_LEN` = 8160) and reports `Applied` for the same two lengths, so it needs the same correction; both are listed in the layer design §12.

---

## 10. Testing

The page design §6 pins the format battery (marker goldens, parse refusal matrix, transition table, prune roundtrip, plan-refusal atomicity, redo read-back). The engine adds:

- **Visibility property tests**: randomized txn interleavings (begin/write/commit/abort at seeded points) through the fake mini-txn scope and a scripted `VisibilitySource`; every snapshot's scan result equals the reference model's; the RC re-check recipe's outputs match its definition.
- **Chain invariants**: HOT chains never cross pages (assert `next.page_no == own page` whenever `HEAP_ONLY`); a non-HOT update's chain is exactly one cross-page hop from root to newest; no cycles.
- **Prune/vacuum end-to-end**: the page design's worked example driven through the engine — including the vacuum ordering invariant (slot freed only after index cleanup, proven with a counting fake index that fails the test if a slot frees first).
- **Replay equivalence**: live pages vs replayed-log pages byte-identical for random DML + prune + vacuum workloads (the layer design §11 differential harness).
- **Tid stability**: slots never renumber across prune/compact/vacuum/reuse (the D1/D3 goldens cover the bytes; the engine test covers the sequences).
- **Conflict matrix**: every §4.4 row exercised under every `WaitPolicy`, including bounded-wait expiry and the poisoned-txn paths (D9).
- **Wire parity**: the runtime's query/commit/txn suites pass unchanged across the L1→L2 boundary until MVCC behavior is deliberately pinned (WP5 §8's rule).
- **`AnyTable` conformance**: the full §4.11 suite against the heap arm.

---

## 11. Landing plan and the interim slice

| Step | What lands | Notes |
| :--- | :--- | :--- |
| **L1 / T6.4 interim** (M6 plan) | `HeapView`-shaped insert/scan through the BPM as M6 defines it: xmin stamped from `next_xid()`, xmax/next reserved zero, no MVCC, `RowVisibility::Live` only, `HEAP_INSERT` type 5 emission, `AnyTable::Heap` arm with `insert`/`fetch`/`seq_scan::next`; speculative + rescan/mark/restore as reserved signatures. Placement by the sequential-fill cursor, advanced in LSN order so that live layout equals replayed layout (H9, decided 2026-11-26 — it replaces the plan's earlier acceptance that the two could differ); see §2. Behavior-identical wire suites (FR-47). | The interim slice is this document's *only* concession to the pre-MVCC world; every structure it creates (descriptor, env, scan) is the L2 shape minus visibility |
| **L2 / MVCC milestone** | Slot states (page D1) land first (the amendment window rule); visibility + `VisibilitySource`; the §4 DML paths with real outcomes; prune (D2); `HEAP_INSERT_V2` emission switch — which retires the fill cursor and its mutex, and is the earliest point at which FSM placement (H5) and slot reuse may be enabled; poisoning; statement boundary | The wire suites pass unchanged until MVCC behavior is deliberately pinned |
| **L3** | Hygiene: drop the dead `cascade-mvcc`/`cascade-undo` deps, fix the crate description, CONTEXT sync | Layer design §3.2 |
| **L4** | The mini-txn seam swap (engine code unchanged — it already holds the seam contract) | Layer design §6.1 |
| **L5 / M8** (upstream) | Writeback dies; the engine's mutation paths don't change — they hold the seam; the BPM miss path goes to `PageSource`; `page_lsn` becomes real on-page truth (it is already format-complete) | Storage-layer-design §9 |
| **L8** | `cascade-index` lands: vacuum's index-cleanup step activates (it is code-complete-but-dormant from L2, asserting "no index registered" = no-op), TOAST lands behind it | §7.3, §8 |

Each step is one commit with the workspace gate green and the commit-message discipline of the standing plan (`Task:` trailer, review attestations).

---

## 12. Open questions

| Question | Owner / needed before |
| :--- | :--- |
| Version-word stamping and status-table retention horizon (entries for xid X must outlive every page word still carrying `uncommitted(X)` — i.e., purge-coupled, not read-watermark-coupled; commit-time vs lazy stamping left as WP2 D2 left it) | The L4 transaction pass; constraint recorded, engine-neutral by construction (A8) |
| FSM kind 6 assignment and layout freeze | Landing review of §7.2 — a format one-way door |
| TOAST chunk format, compression, `HAS_TOAST` pointer freeze | L8 landing review (§8's reservation made now, format decided then) |
| Unlogged/temp heap tables (the `UNLOGGED_MARKER` path) | With `cascade-temptable` (layer design §9.3); the marker type stays reserved |
| Prune's background cadence and the dead-food list's memory bound | Tuned when `cascade-maintenance` lands (§9.4 of the layer design); correctness never depends on it |
| ~~Row migration on update~~ | **Resolved by the pivot**: append-only versions have no migration problem — a non-fitting update writes a new version wherever the FSM says (§4.2); the old CONTEXT question was the in-place world's |
| ~~HOT-equivalent for v1~~ | **Resolved**: it *is* the design (§4.2) |

---

## 13. Decisions

| # | Decision | Status |
| :--- | :--- | :--- |
| H1 | One main segment per relation (+ hidden TOAST segment later); segment-contiguous scan order; no page chain | Proposed — DRAFT |
| H2 | Locators name chain roots; the two chain laws (never cross a page when HOT; Dead resolves NotVisible) are engine-enforced on top of the format's | Proposed — DRAFT |
| H3 | The conflict protocol per §4.4; speculative inserts are ordinary inserts with index-level arbitration; `complete`/`abort` change no heap bytes | Proposed — DRAFT |
| H4 | Prune eligibility = committed/aborted **and** older than the injected watermark; uncommitted never eligible (page D2 restated as engine law) | Proposed — DRAFT |
| H5 | FSM = lossy WAL-logged pages, proposed kind 6, 32-byte granularity, verdict-shaped updates | Proposed — DRAFT |
| H6 | Vacuum ordering: index cleanup durably precedes `apply_slot_free` — structural, crash-safe | Proposed — DRAFT |
| H7 | TOAST: hidden same-scope heap relation + unique index; pointer format reserved `[magic][chunk_id][total_len]`; lands at L8 | Proposed — DRAFT |
| H8 | `HEAP_INSERT_V2` (type 10) carries the slot; emission switches at the MVCC milestone; type 5 replay-retained forever | Proposed — DRAFT |
| H9 | While type 5 is the record, placement follows `cascade-redo`'s sequential-fill rule and the fill cursor advances in LSN order; FSM placement and slot reuse begin only with type 10 | Decided (Kenneth, 2026-11-26) — replaces the M6 plan's earlier T6.4 acceptance; FR-46 updated to match |
