# cascade-wal — AI Design Context

## Role

Per-database WAL. The single most consequential crate for §6.3 (logical CDC) because the on-disk record format will be expensive to change later.

## Pillar Reservations

- **§3.4 (Per-DB WAL):** the *defining* pillar. Every WAL is scoped to a `DatabaseId`.
- **§6.3 (Logical CDC):** record format must be sufficient to reconstruct logical events. Get this right *before* the second consumer arrives.
- **§6.4 (Disaggregated):** `WalStore` trait separates I/O backend from log logic. Future S3 / page-service WAL plugs in here.
- **Runtime spec §4 Tier B scoping / §8.2 (NUMA placement):** WAL writers live on a specific NUMA per the owning DB's `numa_affinity`. For `single`-affinity DBs the writer lives on the DB's bound NUMA (all appends NUMA-local). For `cross`-affinity DBs the writer lives on one designated NUMA (chosen when the DB enters `cross` affinity — creation or migration; cross-NUMA reactors append via `Arc<WalWriter>` and pay cross-NUMA atomic cost on the commit path — documented trade).

## Record Format v2 (WP1 F3, T1.2 — the one-way door)

Hand-rolled binary (the protobuf question is answered for the physical
format; a CDC-event protobuf may still be produced downstream by the
decoder). Every record:
`[u32 LE payload_len][u32 LE format_version = 2][u8 type][3× u8 reserved]
[u64 LE xid][u64 LE lsn][u64 LE epoch][payload][u32 LE CRC32 of all
preceding bytes]` — `HEADER_LEN` 36.

- **Type bytes are immutable forever** (F3's table in `format.rs`): 1
  BEGIN, 2 COMMIT (payload = packed `commit_ts` `Hlc`; header xid = the
  txn), 3 ABORT, 4 LEGACY_INSERT (the v1 debug row image — decode
  retained so logs written before T1.3 still replay; nothing emits it
  anymore), 5 HEAP_INSERT (the heap redo record since T1.3 — payload
  `[segment u32][cell bytes verbatim as cascade-page lays them down]`,
  produced by the runtime and the pgmodel twin; the cell is opaque here
  and parses only through cascade-page's `parse_cell`), 6
  CLOSED_TS_HEARTBEAT
  (`closed_ts` u64), 7 HLC_HIGH_WATER (`max_issued` u64), 8 BULK_LOAD,
  9 UNLOGGED_MARKER (6–9 reserved, no producer yet; no-txn records stamp
  header xid 0). New types extend, never renumber — a decoder that meets a
  byte outside its set refuses loudly (`UnknownRecordType`).
- **Records are self-describing** (§5): the header carries the record's
  own LSN, and the reader cross-checks it against the position it read
  from — a mismatch is `LsnMismatch` (a transplant), refused even though
  the CRC may hold. The `epoch` field reserves §4 scope fencing in the
  format (`WalWriter::new_in_epoch`; 0 until multi-owner; surfaced on
  `WalEntry::epoch`).
- **Version law** (§9): `format_version` is per-record and global; an
  unknown version refuses loudly (`UnknownRecordVersion`); any payload
  evolution inside a type bumps the version. CRC covers `header ++
  payload` — the LSN/epoch fields are protected like the payload.
- **Commit ordering is `commit_ts`'s business** (§6), never LSN adjacency;
  the runtime's counter is a logical-only WP2 stand-in (see
  cascade-runtime `session.rs`) replaced by the real HLC at T2.3.
- Decode validates payload shapes strictly (a CRC-valid record whose
  payload disagrees with its type is corruption — only a buggy encoder
  makes one). The writer reserves by `encoded_len` (pure arithmetic) and
  encodes after the LSN exists; the length never depends on the LSN.

## Hard Invariants

1. **WAL-before-data.** No dirty page may be written until the WAL containing its modifications is durable. The flush LSN protocol is the single most load-bearing crash-safety invariant.
2. **One WAL per `DatabaseId`.** No global LSN. No cross-database log records.
3. **Record format carries enough for logical decoding** — full row images where decoders need them, DDL recorded as WAL records.
4. **CRC on every record.** Detect torn writes.
5. **Filled watermark before flush.** `append_wal` reserves an LSN before the record exists; flush eligibility advances only to the highest *fully written* record, never the reserved watermark. The write-side sibling of #1/#4 — prevents fsyncing a torn record (runtime spec §8.2). Representation (watermark layout, atomics) is this crate's ADR.
6. **Append-only on the hot path.** Any operation that would seek backward must go through truncate.
7. **Loud refusal at the format boundary** (WP1 F3): unknown `format_version`, unknown type bytes, and header-declared LSNs that contradict the read position are all named errors, never silent misreads. Old decoders meeting new producers must fail recognizably — that is the price of "extend, never renumber".

## Out of Scope

- Recovery orchestration above redo-loop level (lives in `cascade-runtime` / Phase 3).
- Logical *decoding* (lives in `cascade-cdc` — this crate only exposes the trait and ensures the records are decodable).
- Physical / streaming replication transport (Phase 7).

## Open Design Questions

- Where DDL records sit in the sequence — interleaved or sidechannel?
- Whether *un-replicated* operations (e.g., temp tables) get a logged-but-undecoded marker, or skip the WAL entirely (§17's UNLOGGED_MARKER type byte is reserved for this decision's outcome).
- Whether the mutex-free staging ring (perf ADR) changes the reserve/encode split (currently: reserve by `encoded_len`, encode after — the encode is pure CPU on the hot path).

## Group Commit (T3.1, answered)

Leader CAS elects one flusher per barrier; the leader drains the *whole*
filled prefix (never just its own commit) after a short `COMMIT_DELAY`
window when its own commit is the frontier — 16 commits, one `fdatasync`.
Leadership and the staged batch are both drop-safe: an abandoned leader
future re-stages its unpublished batch (`StagedBatch`), because re-writing
identical bytes at identical offsets is idempotent — the "leader dying
mid-sync" case re-leads instead of wedging the log. Staging keeps encoded
bytes until durable (a `BTreeMap`, the debug-slice ADR; the mutex-free ring
is the perf ADR's business).

## Boot Recovery Primitives (T3.3, answered)

The read side of invariant 4 distinguishes the failure shapes:
`WalError::TornTail` — a record whose declared length runs past EOF, or a
truncated header — is the normal crash artifact (an unfinished write, never
fsynced); recovery treats it as end-of-log and truncates (`truncate_log`).
`WalError::Corruption` stays reserved for a complete-length record that
fails its CRC — bit rot under durable history, which recovery must refuse
over, never serve. `WalError::LsnMismatch` (header LSN ≠ read position: a
transplant) and `WalError::UnknownRecordType` / `UnknownRecordVersion`
(a newer producer's bytes) refuse boot the same way — the loud-refusal
family of invariant 7. `WalWriter::new_at(start)` is the resume
constructor: a writer seeded at the replayed log end so a restarted boot's
first flush appends instead of overwriting (`new_in_epoch` adds the §4
owner epoch). The clean marker (`db_<db>.clean`, 8-byte LE LSN, file +
best-effort directory fsync) is written only on graceful shutdown;
`read_clean_marker`/`log_len` let boot classify the shutdown. All three fs
helpers are synchronous `std::fs` and must run before any worker opens a
handle. Recovery *orchestration* (the walk, the plan) stays out of this
crate — it lives in cascade-runtime (`recovery::prepare`).
