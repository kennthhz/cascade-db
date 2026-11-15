# WP1 — Formats: version word, row header, WAL record contract, key encoding

Status: ACCEPTED (F1/F2 implemented at T1.1; F3 implemented at T1.2; F3's HEAP_INSERT landed at T1.3; F4 implemented at T1.4) · 2026-11-14
Upstream: docs/storage-subsystem-hld-v2.md §5 (record contract), §6 (version words), §9 (redo contract), §11–§13 (row/index formats). Upstream §18 names WP1 the one-way door: every engine depends on these formats; detailed designs must not break an invariant without changing that doc first.
Consumers: WP5 table access API (docs/storage-wp5-table-access-api.md — D4 version words, D13 key contract), cascade-page, cascade-wal, cascade-heap (T3.x), cascade-btree (WP6/WP8).

## Responsibility

Freeze the byte-level one-way doors before any format-consuming engine code lands: the 64-bit **version word** that every row and index entry carries, the **heap row header** as it lives in a cascade-page cell, the **WAL record contract** (header layout, record families, versioning rules), and the **key encoding contract** (`KeyBytes`/`KeyRange`) with the logical index-entry format.

## Key decisions

### F1 — The version word (upstream §6, "Proposed" → confirmed)

64 bits, little-endian wherever stored:

```text
word = 0                      → EMPTY (absent deleter / reserved — never a valid creator)
top bit (63) = 1              → UNCOMMITTED: bits 0..63 = Xid (63-bit per-scope counter)
top bit (63) = 0, word ≠ 0    → COMMITTED: word = commit_ts (packed HLC)
```

- The repo's `Xid` (cascade-types) **is** the WP5 doc's `TxnId`; the API doc defers to repo naming.
- The packed `Hlc` is `48-bit ms << 16 | 16-bit logical`, counting **ms since a custom epoch (2026-01-01Z)**. The custom epoch is a *cross-WP requirement on WP2*: it keeps bit 63 clear until ≈ year 6488, so committed words can never collide with the uncommitted tag. WP1 enforces it at the type boundary (`VersionWord::committed` refuses `Hlc` ≥ 2^63).
- `EMPTY = 0` is safe precisely because the HLC never issues 0 (physical part > 0 from epoch).

### F2 — Heap row header (cascade-page cell; upstream §12)

The cell keeps `[len u16][xmin u64][xmax u64][next u64][payload]` with `CELL_FIXED = 26` — **no size change, no golden-byte change for reserved-zero cells** (bytes written today are byte-identical to the v1 format). What changes is the semantics of the third word:

| Field | v1 (T6.1) | v2 (this decision) |
| :---- | :---- | :---- |
| `xmin` | raw u64 XID | `VersionWord` — the §12 *creator* word |
| `xmax` | reserved 0 | `VersionWord` — the §12 *deleter* word (0 = live) |
| `undo_ref` | reserved 0 | **`next` word** — packed `[flags u16 \| page_no u32 \| slot u16]` |

`next` bit layout (LE u64): bits 0..15 slot, bits 16..47 page_no (relation-local `PageNo` — a chain never leaves its relation), bits 48..63 flags. Flags: `HAS_NEXT = 1<<0` (the chain continues — without it the rest of the word is ignored, so `next == 0` unambiguously means "chain ends"), `HEAP_ONLY = 1<<1` (HOT chain member), `HAS_TOAST = 1<<2`. "HOT-updated root" is *derived*, not stored: `HAS_NEXT && next.page_no == this page_no` (§12's "a HOT chain never crosses a page" makes this derivation total). PG names (`xmin`/`xmax`) are retained for the creator/deleter words — same concepts as §12's creator/deleter, shorter and already the crate's vocabulary.

Rationale: heap has no undo (undo chains are IOT, §11) — repurposing the reserved `undo_ref` word costs zero bytes and keeps the header inside §12's shape. `FORMAT_VERSION` stays 1: no persisted reader exists anywhere; the format evolution is documented here and in git history, which is exactly what the version field is *for* once there are readers.

Page-level validation stays **structural** (lengths, offsets); version-word semantics are not validated at parse — any u64 decodes, and the visibility layer (D4's `row_visible`) interprets. Mutation accessors `set_xmax` / `set_next` land now (they are the §12 delete/update mechanics at the format level); the engine semantics that drive them are T3.x's.

### F3 — WAL record contract v2 (upstream §5 + §9; lands at T1.2)

Current v1 header: `[payload_len u32][format_version u32][type u8][pad u8×3][xid u64][payload][crc32 u32]` = 20 B. v2:

```text
offset  0: payload_len    u32
offset  4: format_version u32  (2)
offset  8: record_type    u8
offset  9: reserved       u8 ×3
offset 12: xid            u64   (or 0 for no-txn records)
offset 20: lsn            u64   (this record's start offset — explicit, self-describing)
offset 28: epoch          u64   (scope ownership epoch, §4 fencing; 0 until multi-owner)
offset 36: payload        …
tail:     crc32 u32            (over offset 0..payload_end, as v1)
```

`HEADER_LEN` 20 → 36. Rationale per field: explicit **LSN** makes records self-describing (§5 "every record carries LSN") — recovery and the PageStore can position without trusting file offsets; **epoch** reserves §4 fencing in the format now so multi-owner needs no format bump.

Record types (bytes stable forever; new types extend, never renumber):

| Byte | Type | Payload |
| :---- | :---- | :---- |
| 1 | BEGIN | empty (kept; §5 "begin optional") |
| 2 | COMMIT | `commit_ts` HLC packed u64 (§6: the commit record carries txn_id + commit_ts) |
| 3 | ABORT | empty |
| 4 | LEGACY_INSERT | v1 debug row image — **retained for replay of existing fixtures; runtime stops emitting it** |
| 5 | HEAP_INSERT | `[segment u32][cell bytes verbatim as cascade-page lays them down]` — the T6.3 record, now WP1-owned |
| 6 | CLOSED_TS_HEARTBEAT | `closed_ts` u64 — reserved (§7 idle-scope advance) |
| 7 | HLC_HIGH_WATER | `max_issued` u64 — reserved (§6 primary restart) |
| 8 | BULK_LOAD | reserved (§17: reserve the type now) |
| 9 | UNLOGGED_MARKER | reserved (§17 non-logged path) |

Versioning rules (§9 contract, restated as format law): `format_version` is per-record and global — a decoder refuses a version it does not know **loudly** (v1 behavior kept); type bytes are never renumbered; any payload evolution inside a type bumps the format version; redo records must stay self-contained per page and deterministic (§9's one-way-door contract — the format carries `lsn` for §9's idempotency rule).

The contiguous-durable-prefix semantics (§5 "persisting up to N means every byte below N is persisted; holes block") are writer machinery — cascade-wal's existing filled-watermark invariant — referenced here, not redefined.

### F4 — Key encoding + index entry (logical contract; lands at T1.4 with pgtypes)

- `KeyBytes` = **order-preserving** normalized encoding; equality and ordering are defined *on normalized bytes* (WP5 D13). Per-type encodings are `cascade-pgtypes`' deliverable; NULLS LAST is pinned here as the global rule.
- `KeyRange { lower: Bound<KeyBytes>, upper: Bound<KeyBytes> }`, `Bound = Unbounded | Included | Excluded` — bounds compare on normalized bytes; multi-column keys are the concatenation of per-column encodings (prefix scans are ordinary ranges over the concatenation).
- Index entry, **logical** shape (§13): `[key bytes][locator][create word][delete word]` where locator = PK `KeyBytes` (IOT) or `[page_no u32][slot u16]` (heap, relation-local — same packing family as F2's `next` word, without flags). Create/delete words are F1 version words stored raw (8 B each); §13's delta-encoding-against-page-base is an WP8 optimization deferred until entry volume justifies it.
- The B-tree **page** layout (how entries are slotted, split, and SMO'd) is cascade-btree's — WP6/WP8 — deliberately not pinned here; WP1 pins only what redo and uniqueness checks consume: the entry encoding above.

## Invariants

1. Little-endian everywhere; every multi-byte field in F1–F4 is LE.
2. `VersionWord(0)` is EMPTY and never a valid creator; bit 63 is the uncommitted tag; committed words are < 2^63 — guaranteed by the HLC custom epoch, enforced at `VersionWord::committed`.
3. A version word is opaque at page parse; only the visibility layer interprets it (structural vs semantic validation split, cascade-page invariant).
4. `next` words: bit-flow is `slot | page_no<<16 | flags<<48`; `HAS_NEXT` clear ⇒ the rest of the word is ignorable; chains never reference another relation.
5. WAL: CRC over `header ++ payload` on every record; unknown `format_version` refuses loudly; type bytes immutable; records self-describing (`len`, `lsn`).
6. Redo records are per-page self-contained and deterministic (§9) — enforced at every new type's design review, starting with HEAP_INSERT (F3).
7. Key ordering is *only* ever defined on normalized `KeyBytes`; a type whose normalization is not order-preserving may not participate in an index.

## Interfaces

Lands in `cascade-types` (F1): `Hlc`, `VersionWord` (`uncommitted(Xid)`, `committed(Hlc)`, `from_raw`, `is_uncommitted`, `is_empty`, `xid()`, `commit_ts()`), the epoch constant. Lands in `cascade-page` (F2): `NextWord` (`NONE`, `chain(page_no, slot, flags)`, `from_raw`, `has_next`, `page_no`, `slot`, `flags`), `NEXT_HAS_NEXT`/`NEXT_HEAP_ONLY`/`NEXT_HAS_TOAST`, typed `CellRef { xmin: VersionWord, xmax: VersionWord, next: NextWord, payload }`, `PageMut::set_xmax(slot, VersionWord)`, `PageMut::set_next(slot, NextWord)`, `append_cell(xmin: VersionWord, payload)`. WAL v2 (F3) landed at T1.2: `WalRecord` (v2 enum: `CommitTxn { xid, commit_ts }`, `LegacyInsert` + reserved families), `format::encode(rec, lsn, epoch)`, `WalWriter::new_in_epoch`, `WalEntry::epoch`, and the reader's declared-LSN cross-check (`WalError::LsnMismatch`/`UnknownRecordType`). HEAP_INSERT landed at T1.3: `WalRecord::HeapInsert { xid, segment, cell }` (payload `[segment u32][cell bytes verbatim]`); cascade-page gained `encode_cell`/`parse_cell` so every producer stamps cells through one code path — runtime and the pgmodel twin emit it, LEGACY_INSERT is decode-retained for pre-T1.3 logs, nothing emits it. Pgtypes key encodings (F4) landed at T1.4: `KeyBytes` (non-null keys are `[tag u8][payload]` — `0x01` INT, all widths canonicalized to sign-flipped big-endian `i64` so int2/int4/int8 interleave under one tag; `0x02` TEXT, bytewise C collation, UTF-8 + `0x00` terminator; `0x03` BOOL; tags frozen, extend-never-renumber like the WAL's type bytes) and the bare `0xFF` NULL marker that makes NULLS LAST hold bytewise; `KeyRange` bounds compare on normalized bytes with `prefix_bytes`/`text_prefix` constructors whose excluded uppers keep NULL continuations inside a multi-column prefix; `IndexEntry::encode`/`Locator` (`Pk(KeyBytes)` | `Heap { page_no u32, slot u16 }`, tags `0x00`/`0x01`) pin the logical entry shape.

## Open questions

| Question | From | Needed before |
| :---- | :---- | :---- |
| Per-type `KeyBytes` encodings for the types that don't normalize order-preserving yet (numeric, temporal, UUID, JSON) and non-C text collations | F4 | before those types participate in an index (invariant 7 gates them; int/text/bool + C collation landed at T1.4) |
| Schema version placement: row header vs page header | cascade-page CONTEXT (carried) | ALTER TABLE era / §6.3 CDC |
| Undo record format (IOT undo pages, §11) | §11 | WP6 detailed design |
| Bulk-load record payload shape | §17 | Reserved only; sharding/bulk era |

## Provenance

F1/F2 decided and implemented 2026-11-14 (task T1.1) from HLD v2 §6/§12 read against the landed T6.1 cell; F3/F4 decided 2026-11-14, implementing at T1.2/T1.4. T6.3 (HeapInsert type 5) is absorbed by F3 per the phase plan in the session task list; the T6.3 plan text ("type 4 documented legacy-retained, replay maps it, runtime stops emitting it") is restated verbatim as F3's type-4 row.

F3's T1.2 implementation review resolved the one open format question: COMMIT does **not** also carry the durable LSN — the header's explicit LSN already self-describes position (and the reader cross-checks it), and §6's commit record stays exactly txn_id + commit_ts. Also landed at T1.2 beyond the strict header: the reader refuses a record whose declared LSN contradicts its position (`LsnMismatch`) and names unknown type bytes (`UnknownRecordType`) — the loud-refusal half of "extend, never renumber"; the runtime's commit_ts is a logical-only counter stand-in until WP2's HLC (T2.3). T1.3 (HEAP_INSERT) added the row-image payload codec to `session` (`debug_row_image`/`parse_debug_row`, public — the format is a contract) and an oversized-cell refusal mapped to a `54000` error, replacing the legacy path's panic; the pgmodel twin emits the same record through the same cell codec, with wire/process independence intact.

F4's T1.4 implementation fixed the remaining encoding degrees of freedom: NULLS LAST by a bare `0xFF` marker (no non-null payload starts with `0xFF`, so no per-type NULL rule exists); ints sign-flipped (`^ i64::MIN`) big-endian, all widths canonicalized to `i64` (width-uniform ordering); text under C collation — UTF-8 byte order is code-point order and the `0x00` terminator only breaks ties toward the shorter string, so the encoding stays order-preserving even with interior NULs, and prefix-free so `text_prefix` can build its upper as `payload ++ [0xFF]` (terminator-less); the general multi-column prefix upper is `fixed ++ [0xFF, 0xFF]`, which outranks every continuation's tag byte *and* the bare NULL marker — a row whose next column is NULL stays inside its prefix range. Text collation is decided (C only; non-C gated by invariant 7 until a collation-aware normalization lands).
