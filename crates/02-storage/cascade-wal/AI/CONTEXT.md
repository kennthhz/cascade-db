# cascade-wal — AI Design Context

## Role

Per-database WAL. The single most consequential crate for §6.3 (logical CDC) because the on-disk record format will be expensive to change later.

## Pillar Reservations

- **§3.4 (Per-DB WAL):** the *defining* pillar. Every WAL is scoped to a `DatabaseId`.
- **§6.3 (Logical CDC):** record format must be sufficient to reconstruct logical events. Get this right *before* the second consumer arrives.
- **§6.4 (Disaggregated):** `WalStore` trait separates I/O backend from log logic. Future S3 / page-service WAL plugs in here.
- **Runtime spec §4 Tier B scoping / §8.2 (NUMA placement):** WAL writers live on a specific NUMA per the owning DB's `numa_affinity`. For `single`-affinity DBs the writer lives on the DB's bound NUMA (all appends NUMA-local). For `cross`-affinity DBs the writer lives on one designated NUMA (chosen at DB creation; cross-NUMA reactors append via `Arc<WalWriter>` and pay cross-NUMA atomic cost on the commit path — documented trade).

## Hard Invariants

1. **WAL-before-data.** No dirty page may be written until the WAL containing its modifications is durable. The flush LSN protocol is the single most load-bearing crash-safety invariant.
2. **One WAL per `DatabaseId`.** No global LSN. No cross-database log records.
3. **Record format carries enough for logical decoding** — full row images where decoders need them, DDL recorded as WAL records.
4. **CRC on every record.** Detect torn writes.
5. **Append-only on the hot path.** Any operation that would seek backward must go through truncate.

## Out of Scope

- Recovery orchestration above redo-loop level (lives in `cascade-runtime` / Phase 3).
- Logical *decoding* (lives in `cascade-cdc` — this crate only exposes the trait and ensures the records are decodable).
- Physical / streaming replication transport (Phase 7).

## Open Design Questions

- Record format: protobuf vs. hand-rolled binary. Leaning hand-rolled for hot-path speed; protobuf is tempting for the CDC side. Compromise: hand-rolled physical format, with a CDC-event protobuf produced by the decoder downstream.
- Group-commit strategy under thread-per-core (§3.2).
- Where DDL records sit in the sequence — interleaved or sidechannel?
- Whether *un-replicated* operations (e.g., temp tables) get a logged-but-undecoded marker, or skip the WAL entirely.
