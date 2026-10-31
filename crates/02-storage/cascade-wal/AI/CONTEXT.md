# cascade-wal — AI Design Context

## Role

Per-database WAL. The single most consequential crate for §6.3 (logical CDC) because the on-disk record format will be expensive to change later.

## Pillar Reservations

- **§3.4 (Per-DB WAL):** the *defining* pillar. Every WAL is scoped to a `DatabaseId`.
- **§6.3 (Logical CDC):** record format must be sufficient to reconstruct logical events. Get this right *before* the second consumer arrives.
- **§6.4 (Disaggregated):** `WalStore` trait separates I/O backend from log logic. Future S3 / page-service WAL plugs in here.
- **Runtime spec §4 Tier B scoping / §8.2 (NUMA placement):** WAL writers live on a specific NUMA per the owning DB's `numa_affinity`. For `single`-affinity DBs the writer lives on the DB's bound NUMA (all appends NUMA-local). For `cross`-affinity DBs the writer lives on one designated NUMA (chosen when the DB enters `cross` affinity — creation or migration; cross-NUMA reactors append via `Arc<WalWriter>` and pay cross-NUMA atomic cost on the commit path — documented trade).

## Hard Invariants

1. **WAL-before-data.** No dirty page may be written until the WAL containing its modifications is durable. The flush LSN protocol is the single most load-bearing crash-safety invariant.
2. **One WAL per `DatabaseId`.** No global LSN. No cross-database log records.
3. **Record format carries enough for logical decoding** — full row images where decoders need them, DDL recorded as WAL records.
4. **CRC on every record.** Detect torn writes.
5. **Filled watermark before flush.** `append_wal` reserves an LSN before the record exists; flush eligibility advances only to the highest *fully written* record, never the reserved watermark. The write-side sibling of #1/#4 — prevents fsyncing a torn record (runtime spec §8.2). Representation (watermark layout, atomics) is this crate's ADR.
6. **Append-only on the hot path.** Any operation that would seek backward must go through truncate.

## Out of Scope

- Recovery orchestration above redo-loop level (lives in `cascade-runtime` / Phase 3).
- Logical *decoding* (lives in `cascade-cdc` — this crate only exposes the trait and ensures the records are decodable).
- Physical / streaming replication transport (Phase 7).

## Open Design Questions

- Record format: protobuf vs. hand-rolled binary. Leaning hand-rolled for hot-path speed; protobuf is tempting for the CDC side. Compromise: hand-rolled physical format, with a CDC-event protobuf produced by the decoder downstream.
- Where DDL records sit in the sequence — interleaved or sidechannel?
- Whether *un-replicated* operations (e.g., temp tables) get a logged-but-undecoded marker, or skip the WAL entirely.

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

The read side of invariant 4 now distinguishes the two failure shapes:
`WalError::TornTail` — a record whose declared length runs past EOF, or a
truncated header — is the normal crash artifact (an unfinished write, never
fsynced); recovery treats it as end-of-log and truncates (`truncate_log`).
`WalError::Corruption` stays reserved for a complete-length record that
fails its CRC — bit rot under durable history, which recovery must refuse
over, never serve. `WalWriter::new_at(start)` is the resume constructor: a
writer seeded at the replayed log end so a restarted boot's first flush
appends instead of overwriting. The clean marker (`db_<db>.clean`, 8-byte
LE LSN, file + best-effort directory fsync) is written only on graceful
shutdown; `read_clean_marker`/`log_len` let boot classify the shutdown.
All three fs helpers are synchronous `std::fs` and must run before any
worker opens a handle. Recovery *orchestration* (the walk, the plan) stays
out of this crate — it lives in cascade-runtime (`recovery::prepare`).
