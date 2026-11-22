# cascade-page design — one page layer, many page formats

Status: DRAFT for review · 2026-11-15 · supersedes the same-day first draft — multi-format support re-rooted as the requirement; the draft-to-draft mapping is the provenance note at the end of the decision log
Upstream: docs/storage-subsystem-hld-v2.md §2 principle 5, §9 (the redo contract), §12 (heap engine); WP5 (docs/storage-wp5-table-access-api.md) D4 (one visibility rule), D8 (owner-core reads).
Depends on: the T6.1 page format (locked), the T6.2 BPM access model (the buffer pool: copy-out guards, exclusive-pin mutation), WP1 F1/F2 typed version/chain words.
Citations: `§9`, `§12`, `§6.3`, `§7`, `§11` refer to the upstream HLD's sections.

**This document in one look:**

1. cascade-page is the **page layer** shared by its two consumers — cascade-heap today, cascade-btree later — and §1–§3 pin what the layer owns vs what each consumer's format owns. The crate exists, first, so the heap format can sit below both the engine and the storage-side consumers of its bytes (WAL producers, replay/PageStore) — see "Why a separate crate at all" in §1.
2. §4 is the heap format's MVCC mutation design: slot states (D1), plan-carried prune (D2), compact-on-prune (D3), redo primitives (D4), chain traversal (D5).
3. D6 lists the reservations to make now, while no page is frozen in production.
4. **Nothing here changes M6** — the current milestone (real heap INSERT/SELECT through the buffer pool, pages persisted, but no MVCC visibility yet) needs none of it; this design lands in the MVCC milestone that follows.

**Terms, in plain words** (the upstream docs own the formal definitions):

| Term | Meaning here |
| :---- | :---- |
| page / page image | one 8 KiB block of bytes — the unit everything stores |
| slot | a 2-byte entry in the page's slot array; a row version's address is `(page, slot)` — the `Tid` |
| cell | the bytes of one row version: `[len][xmin][xmax][next][payload]` |
| xmin / xmax | the version's creator / deleter stamps (typed 64-bit "version words") |
| chain, HOT chain | the versions of one row linked by `next`. "HOT" is decided at update time — the new version fits on the same page and no indexed column changed — so the resulting chain never leaves the page, and index entries keep pointing at its oldest link (the **root**) |
| stub | a dead cell kept (instead of freed) only so a chain or a stale index entry stays resolvable until vacuum cleans it up |
| prune | quick, access-triggered cleanup of dead versions on one page |
| vacuum | slower background cleanup: index entries first, then the slots they pointed at |
| redo | the WAL record that lets a replaying side reproduce one change on one page image |
| cleanup watermark | the oldest timestamp any active reader may still need; things older than it are safe to remove |
| MVCC | multi-version concurrency control — readers see snapshots; writers add new versions instead of overwriting |
| IOT | index-organized table — the PK-clustered B-tree organization (cascade-btree's); the row lives in the leaf |
| golden test | a test that pins the exact expected bytes, so any layout drift fails loudly |

**The page, in one picture** (the locked T6.1 layout this design extends):

```text
offset 0    ┌────────────────────────────────────────────┐
            │ format_version u16 = 1                     │ ┐ layer
offset 2    │ page_kind       u16   (1 = heap)           │ ┘ prologue
offset 4    │ slot_count      u16                        │ ┐
offset 6    │ free_offset     u16                        │ │ heap
offset 8    │ page_lsn        u64   (zero in M6)         │ │ header
offset 16   │ reserved 16 B (zero; @16 = future          │ │ (32 B)
            │   schema epoch, D6)                        │ ┘
offset 32   ├────────────────────────────────────────────┤
            │ slot array: slot_count × u16, growing UP   │  each = cell offset or a D1 marker
free_offset ├────────────────────────────────────────────┤ ← everything between = free space
            │        ...                                 │
            │ cells, growing DOWN from the page tail     │  cell = [len u16][xmin u64][xmax u64][next u64][payload…]
PAGE_SIZE   └────────────────────────────────────────────┘
```

## 1. The root requirement: a page layer, not a page format

cascade-page is not "the heap page format". It is the **page layer of the database**: an internal API that every page-based storage organization builds on. It has two consumers — the parent access layers, heap and btree — both called by the engine through the table access API:

```text
cascade DB engine
  └── table access API            (WP5: AnyTable — engine-neutral row operations)
        ├── cascade-heap          heap organization   (page_kind 1)     ──┐
        │                                                          ├── cascade-page
        └── cascade-btree        IOT organization    (future kinds)  ──┘   (the page layer)
```

Three requirements define the layer:

- **R1 — one place for common page logic.** Anything two page formats would otherwise duplicate — the 8 KiB unit, the version/kind prologue, the parse discipline, the mutation-safety rules, the redo rules — lives here, once.
- **R2 — many formats under one layer.** The heap format and the btree format are different byte layouts hosted by the same contract, each independently versioned. Adding a format must not edit the existing ones.
- **R3 — the layer never reaches up.** cascade-page knows nothing about rows, keys, visibility, or engines. Its consumers call it; it calls nothing above it.

**Why a separate crate at all — the honest answer, beyond common logic.** The common part *is* small (a prologue parser, the kind registry, accessor helpers, and conventions); conventions alone would not justify a crate. The justification is dependency topology: the heap page format has consumers on both sides of the engine boundary, and they cannot all live above it.

- **cascade-heap** (05-access) — the engine; needs everything.
- **WAL record producers** — the HeapInsert record carries cell bytes verbatim as this format lays them down (WP1 F3: one code path, or the log and the page drift).
- **Replay / recovery — today the runtime's boot redo, tomorrow the disaggregated PageStore** — the redo-apply functions (D4) are pure byte transforms with zero engine semantics (§9: engines are opaque to the PageStore except through redo).

If the format lived inside cascade-heap, every storage-side consumer would import an access-method crate to write bytes — a layering inversion (05-access sits above 02-storage), and the PageStore's build would carry engine code the redo contract forbids it from meaning. So cascade-page's real role: **the crate that lets the heap page format sit below both the engine and every consumer of its bytes.** The layer framing (R1–R3) adds the frame contract on top, deliberately nothing more — btree logic never lives here; the btree format is cascade-btree's from day one.

The route from here: §2 draws the boundary the layer must not cross; §3 makes the **layer decisions** (L1–L3); §4 makes the **heap-format decisions** (D1–D6) that obey them.

Precedent check: this is the PostgreSQL `bufpage.h` shape — one shared page API serving heap, btree, and the other access methods, each with its own layout on top. PG's *heap prune mechanics* remain the template for the heap format's mutation semantics (§4), because the upstream HLD chose a PostgreSQL-like heap (its requirement R3). PG is precedent at both levels, for different reasons.

## 2. The boundary: what the layer owns vs what each format owns

**The layer owns (shared once, here):**

| Piece | Content |
| :---- | :---- |
| The unit | 8 KiB page, 4 KiB alignment (`PAGE_SIZE`/`AlignedBuf` live in cascade-types; the layer treats them as given) |
| The prologue | bytes 0..4 — `format_version` u16 @0, `page_kind` u16 @2 — the only bytes whose meaning spans kinds. The switch's rules are decision L1. |
| The discipline | bytes-are-the-spec (no `#[repr(C)]` structs), compile-time format locks, golden byte-literal tests, **total parse at the boundary** (corruption refused once, never mid-scan), validate-before-mutate (the mutation path cannot bypass the read path's rules) |
| The redo binding | every kind's redo obeys §9's contract — self-contained per page, deterministic, versioned, idempotent by LSN — and each format's apply functions live at that format's authority (L2) |
| Helpers | little-endian field accessors and prologue reads shared by every format's parser |

**Each format owns everything else** — its full header, its slot/entry layout, its cell shapes, its parse, its redo apply functions. Today the heap format (kind 1) is the only one implemented, and it lives in this crate. The btree leaf/branch formats (§11) will live in cascade-btree under this contract; columnar keeps its own crate and consumes the same unit.

**The line the layer does not cross — cells are not unified.** The heap cell is `[len][xmin][xmax][next]`: a *forward* chain over heap tuples. The future btree leaf entry points *backward* into undo (`roll_ptr`). Different direction, different target space, different mutation model (append-and-prune vs overwrite-in-place). A union cell would tax every heap tuple 8 bytes for a field the heap can never use — and this format's history already rejected the idea: v1's original `undo_ref` reservation *was* the unified-cell accommodation, and WP1 F2 deliberately repurposed it into `NextWord` because "the heap has no undo; undo chains are IOT". That rejection is reaffirmed here as a boundary, not a footnote.

The unification that matters happens above the bytes: `VersionWord`'s encoding (cascade-types, WP1 F1) is identical in heap cells, btree leaf rows, undo records, and index entries, and one read rule (`Snapshot::visible` / `row_visible`, WP5 D4) interprets all of them. The layer shares *semantics*; formats own *bytes*.

## 3. Layer decisions

### L1 — The prologue is the switch (kind registry, per-kind version namespaces)

Bytes 0..4 are the layer's: `format_version` u16 @0, `page_kind` u16 @2. The heap layout's slot_count/free_offset (offsets 4/6) begin immediately after the prologue and belong to the kind — moving them into a "shared header" would buy nothing and churn the locked format; shared means semantics, not adjacency.

**The payload is pluggable; the header is the contract.** Everything after byte 4 belongs to whichever kind the prologue names — its own header, layout, parse, and redo appliers. Pluggability lives at the parser seam: a new page kind = a new number in the registry plus a new authority crate; no existing kind changes, and there is no runtime indirection inside the bytes.

**Version = major.minor under each kind.** The u16 `format_version` is interpreted as *major.minor* — high byte major, low byte minor (so on the wire, LE: byte @0 = minor, byte @1 = major) — and the locked byte layout does not move. The numbers mean:

- **major** — layout-breaking evolution. A parser that does not know the major refuses (`WrongVersion`).
- **minor** — backward-compatible evolution, under one hard discipline: a minor bump may only *define previously reserved* bytes or regions. Older same-major parsers must keep parsing correctly — validating what they know, ignoring what they don't; the outer CRC remains the integrity net for space an older parser cannot check. Reinterpreting existing bits — exactly what the D1 slot markers do — is a **major** bump, however small it feels.
- **major 0** — pre-deployment: no compatibility promises of any kind. This formalizes the amend-in-place window the format has used since T6.1: D1's amendment ships while the heap format is major 0; the first persisted deployment is 1.0, and from there the rules above bind.
- §9's "the PageStore must apply every version it has ever accepted" reads as: it accepts `(page_kind, major, minor)` tuples.

Kind registry: 1 = heap (this crate). B+Tree leaf, btree branch, undo pages, and free-space-map (FSM) bookkeeping pages become future kinds, each assigned by its landing authority; this crate owns only kind 1. An unknown kind is not corruption — it is "not this format", and the right refusal (`WrongKind`) distinguishes it from a torn image.

### L2 — The rules are shared; the apply functions are per-format

The redo binding (§9's four rules) is layer property: every kind's WAL redo must be self-contained, deterministic, versioned, and idempotent by LSN. But the apply functions themselves are *byte writers*, so they live with the format authority that owns those bytes — heap apply functions here (D4), btree apply functions in cascade-btree. The layer defines the shape and the rules once so no format invents its own.

The same split applies to mutation safety: the layer owns the *rule* (a mutation validates through the read path and refuses atomically — an invalid transform leaves the image untouched); each format implements it for its own layout. Idempotence-by-LSN needs a per-kind stamp position: each format's header defines where its `page_lsn` lives (heap: offset 8), and the replayer checks it before applying; the BPM frame stamp remains the compute-side authority while nothing replays from page images (T6.1's M6 stance).

### L3 — Crate consequence: split the module before cascade-btree lands

Today `src/lib.rs` is heap through and through — `parse` hardcodes kind 1, `init` stamps `PAGE_KIND_HEAP`, `CELL_FIXED` is heap cell law. Becoming the layer (R1/R2) needs a small, cheap refactor, in two moves:

- **frame module** — prologue reads, the kind registry, `is_initialized`, shared accessors, the discipline's shared rules;
- **heap module** — everything T6.1 landed, moved under kind 1 unchanged; goldens move with it byte-for-byte.

This must land *before* cascade-btree starts, so the btree consumes the contract instead of forking its own prologue logic. No format change, no version bump — pure code organization.

## 4. Heap format decisions (kind 1 — the MVCC milestone's format work)

*Each decision below is legal because it obeys the layer: heap-only concerns stay in the heap format's namespace, and the shared rules it relies on are layer property.*

### D1 — Slot state machine (a v1 amendment — must land before any marker-bearing page is persisted)

A slot's u16 value currently has one meaning: a cell offset. Legal offsets are `< PAGE_SIZE = 0x2000`, so bits 13–15 are always zero in every legal value today — the marker space is free without moving a byte:

```text
Live      = 0x0000..0x1FFF        cell offset (today's only state)
Redirect  = 0x8000 | target_slot  target < slot_count, target must be Live
Dead      = 0xC000                no storage; index cleanup pending
Unused    = 0xE000                free for slot reuse
```

Decode order: bit 15 clear → Live; else bit 14 clear → Redirect; else bit 13 clear → Dead; else Unused. `MAX_SLOTS = 4080` fits the u15 redirect target with room to spare.

(Contrast with PG: its line pointer is 4 bytes — offset, length, and flags together. Here the slot is a bare 2-byte offset; the cell's own `len` prefix carries the length, so markers had to come out of the offset's unused high bits.)

Why each state exists (the reachability argument, PG-modeled because the heap organization is PG-like):

- **Redirect** — a pruned HOT chain root keeps index entries (stale or still-live) resolvable to the newest same-page version at 2 bytes instead of a retained stub cell. Redirect-to-redirect is malformed; parse validates the target is a live cell.
- **Dead** — fully dead, storage gone, index entries pointing here still pending cleanup. A fetch on Dead must resolve to NotVisible deterministically — never to whatever bytes a future append leaves in the region. Distinguishing Dead from Unused is exactly what makes that unambiguous.
- **Unused** — released by vacuum after its index cleanup; reusable by the next insert.

Slot_count never shrinks and slot indices never renumber — they are the Tid's half of its identity. Freed slots remain in the array as markers (2 bytes each) until reuse.

**Legal transitions (who performs each):**

| From → To | Operation | Performer |
| :---- | :---- | :---- |
| Live → Live | `set_xmax` / `set_next` stamps, chain relink | engine DML; prune relink |
| Live → Redirect | prune (`PrunePlan::redirects`) | prune |
| Redirect → Redirect | re-prune retargets the marker to a newer surviving version | prune |
| Live → Dead | prune (`PrunePlan::free`) | prune |
| Dead → Unused | `apply_slot_free` | vacuum, after its index cleanup |
| Redirect → Unused | `apply_slot_free` | vacuum, after its index cleanup |
| Unused → Live | `append_into` | insert (slot reuse) |

`apply_slot_free` refuses Live slots — freeing a live tuple's slot always means the §12 ordering invariant is broken, and an explicit refusal beats silently aliasing a later append onto a reachable row.

One deliberate conservatism in the table: prune always parks freed slots at **Dead — never straight to Unused** — even for HOT middle versions that are provably unreferenced (index entries only ever point at chain roots, and the plan relinked the chain around them). Vacuum, which owns index knowledge, is the single authority for the Dead → Unused step; that keeps invariant 5's proof to one rule. (PG lets its prune mark safe items reusable directly; revisit if slot-reuse latency ever shows up in practice.)

`parse` extends structurally: marker slots are valid states (redirect targets checked live and in range); `cell(slot)` refuses markers with distinct errors so a scan can never misread; `iter()` stays live-cells-only (scan semantics); a new `slots()` iterator exposes `(SlotId, SlotState)` for maintenance.

**Marker-aware slot accessors are mandatory.** Today `cell_offset` treats the slot value as a raw offset; once markers exist in the encoding, every slot-taking path — `cell`, `slot_state`, `set_xmax`, `set_next`, and the redo primitives — must branch on the state first and refuse markers explicitly. A marker misread as an offset points past the page (≥ `0x8000` > 8192); Rust's bounds checks would turn that into a bare panic today, but the contract is a diagnosable refusal, not an accident of bounds checking.

**Version call:** amend v1 in place — same precedent as T1.1, which repurposed the `undo_ref` reservation into `NextWord` without a bump on the grounds that no persisted readers exist. The rule this amendment must pin (into `AI/CONTEXT.md` and `docs/storage-wp1-formats.md` at landing): *the amendment must land before the MVCC milestone persists any marker-bearing page — in L1's terms, while the heap format is still major 0; from the first persisted deployment (1.0) on, slot-encoding changes require a major bump.* M6-era pages carry no markers and remain valid under the extended parse — no data migration. The alternative (ship markers as 1.0 from the start) buys the same safety with golden churn; it is kept only if review prefers the stricter discipline.

### D2 — Prune is a plan-carried transform; deadness is computed by the engine, never replayed

The redo binding (L2, from §9) is what forces the shape: redo applies with "only the page image and the record", and "no reads of engine state". Deadness — aborted creator? deleter older than the cleanup watermark? — depends on the status table, which the replaying side does not have. Therefore the HEAP_PRUNE record **cannot carry a watermark for replay to re-derive verdicts**; it carries the verdicts themselves:

```rust
pub struct PrunePlan {
    pub free: Vec<u16>,                    // slots whose cells are removed (→ Dead)
    pub redirects: Vec<(u16, u16)>,        // (root slot, newest surviving slot), same page
    pub relink: Vec<(u16, NextWord)>,      // chain rewrites around removed middle versions
}
```

The engine computes the plan at prune time, reading the status table (its `txn_id` → committed / aborted / in-progress map) and the cleanup watermark. The eligibility rule it must obey: **uncommitted stamps are never prune-eligible.** An in-flight deleter may still roll back, so its stub must stay — a Read Committed (RC) re-check may need it. An in-flight creator must keep its cell for the rest of its transaction.

Replay is pure application: deterministic, idempotent by LSN, self-contained per page. That same rule is also why prune can never touch indexes — entry cleanup belongs to vacuum, and its records are index pages'.

`PageMut::apply_prune(plan)` validates the whole plan against the current image first (slot in range, redirect targets live, relinked slots live, and the plan **closed**: no redirect may point at a slot the same plan frees) and refuses with the page untouched on any failure — the layer's validate-before-mutate rule (L2). Valid plans apply as one atomic pass: markers, relinks, then compaction (D3). Readers never observe a mid-transform image: compute-side mutation is exclusive-pinned under the BPM access model; replay-side application is to the replayer's own image, and a torn image is refused by the outer page CRC — the checksum cascade-page deliberately does not carry and storage owns (the CRC division in cascade-page's CONTEXT).

### D3 — Compact on prune, always, in the same pass

`apply_prune` ends by rebuilding the tail region. Live cells are copied **verbatim** into a fresh contiguous run growing down from `PAGE_SIZE`; slot values are rewritten to the new offsets; markers, slot order, and slot indices are preserved. `free_offset` is reset exactly — it may move *up* when the topmost cells were freed (legal; it is a header field inside the transform) — and the gap is zeroed.

Why eager, not lazy: `append_cell` can only consume the contiguous region below `free_offset`, so holes are dead weight until compacted; PG compacts during prune for the same reason; 8 KiB makes the rebuild cheap; and `free_bytes()` stays exact for the free space map (FSM) with no hole bookkeeping anywhere. The heap engine triggers prune on low-space access, so the O(page) memcpy lands where it is cheapest — when the page is nearly full.

Slot reuse is the one append that does not grow the page: `PageMut::append_into(slot, xmin, payload)` fills an Unused slot (engine picks lowest-first, for replay-friendly determinism) with cell bytes still carved from the tail region. `append_cell` keeps its exact semantics — including the 291-cell/12-free-bytes full-boundary golden, which must survive unchanged.

### Worked example — one prune, one vacuum, one reuse

Row 7 is updated twice on one page; no indexed column changed, so the chain is HOT. Row 9 was deleted long ago. The cleanup watermark has passed every deletion shown.

Before — slots grow up, cells grow down:

```text
slot 0 → v1 of row 7   deleted, next → slot 1   ← chain ROOT; index entries point here
slot 1 → v2 of row 7   deleted, next → slot 2   ← middle version
slot 2 → v3 of row 7   live                     ← newest; the only VERSION a current read sees
slot 3 → row 9         deleted, chain ends      ← plain dead tuple
```

(All four slots are still state-Live here; "deleted" means MVCC-deleted — `xmax` stamped. A scan reads every cell and filters by visibility; the prune is what turns MVCC-dead into slot-Dead.)

The engine computes `PrunePlan { free: [1, 3], redirects: [(0, 2)], relink: [] }`:

- slot 0 (the root) has a same-page successor → **Redirect**, and its 26+-byte cell is freed;
- slot 1 (middle) is removed outright;
- slot 3 is a standalone dead tuple → **Dead** (its slot stays named because index entries may still point at it and must resolve to "not visible", never to a future stranger);
- `relink` is empty here — it exists for the case this example doesn't hit: a *surviving* cell whose successor was removed (typically a kept cross-page stub root) must be re-pointed around the gap.

After `apply_prune`:

```text
slot 0 → Redirect(2)   root cell bytes gone; entries resolve through 2 bytes now
slot 1 → Dead          cell bytes gone
slot 2 → Live          same cell bytes, moved down — slot index unchanged
slot 3 → Dead          cell bytes gone

free_offset moved UP (top cells were freed); slot_count still 4 — never shrinks;
every surviving cell is byte-verbatim.
```

Later, vacuum does the cross-structure work prune cannot: it cleans row 9's index entries (they point at slot 3), then calls `apply_slot_free` — slots 1 and 3 become **Unused**, and the next insert may `append_into(1, …)`.

Slot 0 does **not** become Unused: row 7 is still live, and its index entry still points at slot 0 — so slot 0 stays Redirect for the life of row 7, its 2-byte marker being the standing price of the collapsed chain. Only when row 7 is itself deleted, and vacuum has cleaned the entries that pointed at it, does slot 0 too go Unused.

If row 7 is updated yet again (v4 lands on this page, HOT) and the page is pruned again, the same mechanics **retarget** the root: Redirect(2) → Redirect(4), and slot 2 — whose cell is now freed — goes Dead. That is the table's Redirect → Redirect row; without it, an update-heavy page could only ever be pruned once.

### D4 — Redo-apply primitives for heap pages live in this crate

The heap format authority is the one legal writer of heap page bytes — and §9 says the replaying side knows page formats "only as far as the redo apply functions need". Those functions *are* this crate's surface (the L2 principle, instantiated for kind 1). A `redo` module over raw images:

```rust
pub fn apply_heap_insert(img: &mut [u8; PAGE_SIZE], cell: &[u8]) -> Result<u16, PageError>;
pub fn apply_delete_mark(img: &mut [u8; PAGE_SIZE], slot: u16, xmax: VersionWord) -> Result<(), PageError>;
pub fn apply_relink(img: &mut [u8; PAGE_SIZE], slot: u16, next: NextWord) -> Result<(), PageError>;
pub fn apply_prune(img: &mut [u8; PAGE_SIZE], plan: &PrunePlan) -> Result<(), PageError>;
pub fn apply_slot_free(img: &mut [u8; PAGE_SIZE], slot: u16) -> Result<(), PageError>;
```

`apply_heap_insert` wraps `append_cell` with the cell validated through `parse_cell` — the WP1 F3 "cell bytes verbatim as cascade-page lays them down" contract extends to replay by construction. Record *decode* stays with cascade-wal/the engine (the redo module consumes decoded arguments; the format authority gains no dependency on the WAL crate). Idempotence-by-LSN stays the caller's check (`page_lsn`) — this crate stays LSN-agnostic except the `set_page_lsn` field op (T6.1 discipline).

### D5 — Fetch-by-Tid traversal vocabulary

The format contributes the walk's vocabulary; the engine owns every visibility decision inside it (parse stays structural — WP1 F2 discipline):

- `slot_state(slot)` → the engine branches: Unused = engine bug (assert — §12 invariant 2 was supposed to prevent this), Dead = NotVisible, Redirect = jump to target, Live = evaluate.
- On an invisible Live cell: `next.has_next()` → hop (same page via slot; cross-page via the engine, which resolves the relation-local `PageNo`).
- The "HOT-updated root" derivation (`HAS_NEXT` && same page) becomes one call (`NextWord::is_hot_link(page_no)`-shaped convenience) so engines never re-derive the predicate by hand.

### D6 — Heap-format reservations pinned now

1. **Schema epoch: heap header offset 16..18, u16, zero = "epoch 0 / unspecified."** The problem it answers: when a table's row encoding changes (online `ALTER TABLE`), old pages still hold rows written in the older encoding, and a decoder must know which one it is looking at — this is the *placement* half of the open question in `AI/CONTEXT.md`. The decision, in bullets:
   - *One u16 per page, not one per row.* Row-level would grow `CELL_FIXED` past 26 — format churn plus a per-tuple tax — for an ALTER TABLE era that has not shipped.
   - *What makes page-level sufficient:* **a page's cells are homogeneous in schema epoch.** Online ALTER re-encodes a page's rows eagerly and raises the page's epoch at the rewrite. The re-encode record carries the new cells verbatim, so replay stays deterministic without needing the old codec on the replaying side.
   - *Who reads it:* CDC (change data capture, §6.3) decoders, to interpret historical payload encodings. *Adoption* stays deferred with the ALTER era; reserving the byte now costs nothing and forecloses nothing else.
   - *Parse stance:* the epoch is never validated at parse — its meaning is the catalog's (structural parse discipline). This is a *heap header* reservation; each format defines its own equivalent when it needs one.
2. **The cleanup watermark is never a page field.** It rides in prune plans (as computed verdicts, D2) and lives in engine/cluster state (§7/§12 open question on cross-scope learning stays theirs).

### Concurrency, in one paragraph

Everything a scope owns — its heap pages, its indexes, its prune and vacuum — runs serialized on the owning core (WP5 D8's message-to-owner reads), so two mutators never touch one page and prune never races an index cleanup. Page-level exclusion is the BPM pin protocol: mutators hold the frame's exclusive pin (a replayer instead writes its own private image), and readers take copy-out snapshots — a coherent 8 KiB copy taken under the frame mutex. A snapshot taken before a prune keeps the versions it captured; a pin taken after it sees the pruned page with the same visible rows, because prune only removes versions older than the cleanup watermark, and the watermark always trails the oldest active snapshot. No page pin is ever held across an await (the F3 copy-out discipline).

## 5. Invariants

**Layer invariants:**

1. **The dependency direction never inverts:** cascade-page depends on no workspace crate besides cascade-types; its consumers (heap, btree) depend on the layer; the layer names no engine, no row, no key.
2. **A `(page_kind, format_version)` pair an authority does not know is refused at that authority's parse** — never guessed at, never partially read.
3. **Cells are never unified across kinds** (§2 boundary). Shared page logic lives in the layer; shared *bytes* do not exist beyond the prologue.
4. **The redo binding applies to every kind** — self-contained per page, deterministic, versioned, idempotent by LSN — and each format's apply functions live at that format's authority.

**Heap-format invariants (kind 1):**

5. **Slot indices are stable for the life of the page's data.** Never renumbered, never shrunk; reuse only via Unused, only after the engine's index-cleanup ordering (§12 invariant 2), lowest-first.
6. **Prune/compact transforms preserve slot indices, slot order, and cell bytes verbatim.** The only legal byte mutations are through `PageMut`/`redo` methods that validate through the read path; an invalid plan leaves the image untouched.
7. **Prune records carry verdicts, not watermarks.** Replay never consults engine state; the same image plus the same plan yields the same bytes.
8. **Reachability is preserved by construction of the plan:** a cell is freed only if chain and index reachability survive — same-page successor via Redirect, cross-page successor via a retained stub cell. A fetch on Dead resolves to NotVisible, never to a stranger's bytes.
9. **Parse is total at the boundary including markers** — corrupt marker values (redirect out of range, redirect-to-redirect, redirect-to-marker) are refused once, at parse, never mid-scan.
10. **A heap page's cells are homogeneous in schema epoch** (once adopted); the header epoch is the page's truth, changed only by a full-page re-encode.

## 6. Interfaces

(Additions to the existing surface; everything from T6.1/T1.1 stands. Layer-level surface first, then heap.)

```rust
// --- layer (frame module, L1/L3) ---
/// Prologue reads shared by every format's parser.
pub fn prologue(img: &[u8; PAGE_SIZE]) -> (u16 /*format_version*/, u16 /*page_kind*/);
// is_initialized already exists and stays kind-agnostic.

// --- heap format (kind 1) ---
/// The four legal slot states (D1's encoding; u16 slot values).
pub enum SlotState {
    Live,          // slot value is a cell offset — read the cell via cell(slot)
    Redirect(u16), // target slot (same page)
    Dead,
    Unused,
}

impl PageView<'_> {
    /// Marker-aware slot inspection; `cell(slot)` keeps refusing non-live slots.
    pub fn slot_state(&self, slot: usize) -> Result<SlotState, PageError>;
    /// Maintenance iteration: every slot with its state (scan uses `iter()` — live only).
    pub fn slots(&self) -> impl Iterator<Item = (u16, SlotState)> + '_;
}

/// D2's verdict carrier — built by the heap engine, applied here.
pub struct PrunePlan {
    pub free: Vec<u16>,
    pub redirects: Vec<(u16, u16)>,
    pub relink: Vec<(u16, NextWord)>,
}

impl PageMut<'_> {
    /// D2+D3: validate plan against this image, then markers + relinks + compact,
    /// atomically. Refusal leaves the image untouched.
    pub fn apply_prune(&mut self, plan: &PrunePlan) -> Result<(), PageError>;
    /// D3: fill an Unused slot (lowest-first is the engine's pick). slot_count unchanged.
    pub fn append_into(&mut self, slot: u16, xmin: VersionWord, payload: &[u8])
        -> Result<(), PageError>;
}

/// D4 — replay-side primitives over raw images (record decode lives elsewhere).
pub mod redo {
    pub fn apply_heap_insert(img: &mut [u8; PAGE_SIZE], cell: &[u8]) -> Result<u16, PageError>;
    pub fn apply_delete_mark(img: &mut [u8; PAGE_SIZE], slot: u16, xmax: VersionWord) -> Result<(), PageError>;
    pub fn apply_relink(img: &mut [u8; PAGE_SIZE], slot: u16, next: NextWord) -> Result<(), PageError>;
    pub fn apply_prune(img: &mut [u8; PAGE_SIZE], plan: &PrunePlan) -> Result<(), PageError>;
    pub fn apply_slot_free(img: &mut [u8; PAGE_SIZE], slot: u16) -> Result<(), PageError>;
}
```

New `PageError` variants for the marker paths (e.g. `SlotNotLive`, `RedirectTarget`) so `cell()`'s refusals stay diagnosable; `Malformed` wording extended for the parse-time marker checks.

**Tests to pin when this lands:**

- Marker encoding golden: slot-array bytes `0x8000|t` / `0xC000` / `0xE000`.
- Parse refusal matrix extended: redirect-to-redirect, redirect out of range, redirect-to-marker, `cell()` on each marker.
- Every slot-taking mutator and redo primitive refusing each marker state (the marker-as-offset hazard, D1).
- The transition table end-to-end: prune → re-prune retargeting a Redirect → vacuum slot-free → reuse — including `apply_slot_free` refusing a Live slot.
- Prune roundtrip golden: slot indices preserved, cells byte-verbatim, `free_offset` exact, gap zero, `free_offset` moving *up*.
- Plan-refusal atomicity: a bad plan leaves the image untouched.
- `append_into` reuse with slot_count unchanged; the 291-cell / 12-free-byte full boundary untouched.
- Redo-module read-back and refusal cases.
- L3's frame/heap split proven by goldens moving byte-for-byte (same literals, new module paths).

## 7. Landing plan

- **Milestone:** the heap mutation work (D1–D6) lands in the MVCC milestone (post-M6; task numbers assigned when its plan is authored). Nothing in M6's ladder changes.
- **The L3 module split is the one piece that should land early** — before cascade-btree starts consuming pages — and D1's `SlotState` + parse extension + goldens can ride with it (no callers yet; it is the piece whose pre-deployment timing matters most).
- **Doc sync at landing (T6.7 discipline):** amend `crates/05-access/cascade-page/AI/CONTEXT.md` — its Role line today ("the 8 KB byte layout that every access method shares") is ambiguous against its own out-of-scope list; this doc resolves the ambiguity toward the layer model (R1–R3): the layer hosts many formats, the heap format is kind 1, btree formats live in cascade-btree. Also grep the other crates' contexts for drifted mirrors of the format description, and append the evolution entry to `docs/storage-wp1-formats.md`.
- **Forward obligation to reserve now (WP8):** the moment index entries carry Tids, replay must reproduce live layout exactly — HeapInsert's redo record must then carry its slot (a physiological record: it names the slot, and replay reproduces the placement), and prune plans already do. M6's accepted "live layout may differ from replay layout" ends at the first index; the record-shape decision belongs to cascade-wal's contract review, flagged here so it is not rediscovered under pressure.

## 8. Open questions

| Question | Owner / needed before |
| :---- | :---- |
| Amend while major 0 vs. ship markers as 1.0 for D1 (recommended: amend — same substance as the old amend-v1-vs-bump-to-2 question, restated in L1's major.minor terms) | This doc's review |
| `page_kind` number assignments beyond kind 1 (btree leaf/branch, undo, FSM) | Each landing authority assigns its own; this crate owns only kind 1 |
| Whether cascade-btree needs more from the frame module than the prologue + discipline (e.g. shared slot-array mechanics for ordered entries) | cascade-btree's format design (§11) — answer there, not by pre-growing this layer |
| Schema epoch adoption (byte reserved at heap header @16; eager-rewrite re-encode record shape sketched in D6) | ALTER TABLE era / before logical decoding ships (§6.3) |
| Cleanup watermark learning across scopes and replicas | Engine/vacuum design (§12, §17) — this format only ever sees verdicts |
| FSM page kind and layout | Heap-engine maintenance design (§12 — ordinary WAL-logged pages; layout under the L1 prologue rule) |
| IOT leaf/undo cell formats | cascade-btree (§11), under the same prologue + golden discipline |
| Bulk-load page handoff (pre-built pages to the PageStore) | §9 open question; page format joins when the record type does |

## 9. Decision log

| Decision | Status |
| :---- | :---- |
| R1–R3 root requirement: cascade-page = the page layer serving cascade-heap and cascade-btree under one contract | Proposed — re-rooted from first draft's trailing D6.3/D7 |
| Cascade-page stays a separate crate: it hosts the heap format *below* the engine so storage-side consumers (WAL producers, replay/PageStore) encode and apply it without engine semantics; common logic alone would not justify the crate — dependency topology does | Proposed — added in review (challenged: "if formats aren't shared, why the layer?") |
| Version = major.minor per kind (major = breaking, refuse; minor = previously-reserved additions only, older same-major parsers keep working; major 0 = pre-deployment, no promises) | Proposed — from review, folded into L1 |
| L1 prologue switch: kind registry + per-kind version namespaces | Proposed |
| L2 rules shared at the layer, apply functions per format authority | Proposed |
| L3 frame/heap module split before cascade-btree lands | Proposed (code organization only — no format change) |
| D1 heap slot state machine (Live/Redirect/Dead/Unused in the u16 marker space) | Proposed — DRAFT |
| D2 prune = plan-carried transform; verdicts computed engine-side, never replayed | Proposed — forced by §9's contract |
| D3 compact-on-prune in the same pass; lowest-first slot reuse via `append_into` | Proposed |
| D4 heap redo-apply primitives owned by this crate; decode stays with WAL/engine | Proposed |
| D5 traversal vocabulary on the format; visibility stays engine-side | Proposed |
| D6 heap reservations: schema epoch @16 (heap header), no watermark field | Proposed |
| Cells stay per-organization (heap `next` forward / btree `roll_ptr` backward); WP1 F2's undo_ref→NextWord repurpose reaffirmed | Proposed — first draft's D7, demoted to the §2 boundary |
| HeapInsert redo carries its slot from WP8 onward (Tid stability across replay) | Flagged for cascade-wal's contract review |

**Provenance from the first draft (same day):** old D1–D5 → D1–D5 unchanged in substance · old D6 item 3 (prologue) → L1 · old D6 items 1–2 → D6 · old D7 (two organizations) → R1–R3 + §2 boundary + L2.
