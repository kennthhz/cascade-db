# bench

The M1 performance harness: an architecture A/B that measures what the
runtime shape itself costs. Two servers speak the same PostgreSQL simple
protocol, run the same debug engine, and write the same WAL bytes with the
same fdatasync-before-ack discipline — the only variable is the concurrency
architecture:

| | cascade | pgmodel |
|---|---|---|
| process model | one process, one reactor per core | postmaster + `fork()` per backend |
| I/O | glommio / io_uring, non-blocking | blocking syscalls per backend |
| connection handoff | std accept → fd over a shared channel → worker ring | accept + protocol handled wholly in the forked child |
| WAL | shared `WalWriter`, per-worker handles, group-commit flush (leader CAS) | one O_APPEND file per backend, `write` + `fdatasync` per txn (`commit_delay=0`) |
| engine | `cascade_runtime::sql` parser + in-memory `Vec` table (identical) | same engine, re-hosted behind the blocking loop |

`bench-pgmodel` is the stripped-down PostgreSQL runtime twin: a
single-threaded postmaster blocking in `accept(2)`, forking a backend per
connection (SIGCHLD→SIG_IGN so children are reaped by init), each backend
serving one connection with blocking socket I/O and committing by appending
the same `cascade_wal::format::encode` records and calling `fdatasync`
before the ack.

## Fairness contract

Both sides share, byte for byte:

- the wire protocol implementation the client sees (simple query loop:
  startup → AuthOk → parameter statuses → backend key → ReadyForQuery →
  Q/R/S/C/D/E/Z), including error codes and empty-query handling — pinned
  by `tests/parity.rs`, which replays 19 statement batches against both
  architectures and compares every wire message *and* every WAL record
  shape;
- the transaction discipline: an autocommit INSERT and a
  `BEGIN; INSERT; COMMIT` batch are one round trip each, with WAL bytes
  written and `fdatasync`'d before the CommandComplete leaves the server;
- the client (one `bench::Client` per closed-loop thread, `TCP_NODELAY`,
  duration-based warmup, p50/p90/p99/max + throughput reporting).

They deliberately differ in exactly one dimension: how the process
structures concurrency.

## Known, documented asymmetries

- **Group commit.** Cascade flushes through one shared writer with a
  leader-CAS barrier, so concurrent sessions on a worker can share one
  `fdatasync`. pgmodel's backends each fsync their own — the PostgreSQL
  `commit_delay=0` posture. At the txn workloads' concurrency this mostly
  measures the storage stack, not the runtime; read the txn/insert numbers
  accordingly.
- **Fork cost** sits in pgmodel's per-connection setup (one `fork()` per
  connection, amortized to zero across a connection's lifetime by
  closed-loop clients). This *understates* what per-connection process
  creation costs a real Postgres under connection churn.

## Running

Everything builds through the workspace. Servers bind 127.0.0.1; keep
server and client cores disjoint for honest numbers (this box: servers on
0-2, clients on 4-7).

```sh
cargo build --release -p bench -p cascade-server

# cascade — spawned by the harness; stdout is the control channel and the
# observables JSON comes back on SIGTERM
target/release/bench-harness \
  --server-bin target/release/cascade-server \
  --server-args "--port 5544 --hk-cores 0 --worker-cores 1,2 --wal-dir /tmp/wal" \
  --workload txn --concurrency 1 --seconds 5 --warmup-seconds 2 \
  --client-cores 4,5,6,7

# pgmodel twin
target/release/bench-harness \
  --server-bin target/release/bench-pgmodel \
  --server-args "--port 5545 --cores 0,1,2 --wal-dir /tmp/wal" \
  --workload txn --concurrency 1 --seconds 5 --warmup-seconds 2 \
  --client-cores 4,5,6,7

# or attach to an already-running server
target/release/bench-harness --addr 127.0.0.1:5544 --workload read \
  --concurrency 4 --seconds 5 --client-cores 4,5,6,7
```

Workloads: `read` (point SELECT off a tiny preloaded table — engine ≈ 0,
pure platform), `insert` (autocommit INSERT, one fsync'd txn per op),
`txn` (`BEGIN; INSERT; COMMIT` in one round trip — the group-commit
target). `tests/parity.rs` must stay green: it is the fairness contract.

## Results

WSL2 (5.15.167.4), Ryzen 7 9800X3D (8 physical cores, no SMT), servers on
cores 0-2, clients pinned to 4-7, release build, 2026-10-24. **Treat all
numbers as directional** — WSL2's virtual disk and io_uring shim are not a
native-Linux platform; the point is the A/B under identical conditions.

Throughput (ops/s) and p50 latency:

| workload | c | cascade | pgmodel |
|---|---|---|---|
| read   | 1 | **11 966 / 53.5 µs** | 9 456 / 72.4 µs |
| read   | 4 | 30 312 / 93.0 µs | **34 972 / 74.3 µs** |
| insert | 1 | 161 / 4.03 ms | 157 / 3.99 ms |
| txn    | 1 | **161 / 4.00 ms** | 134 / 5.18 ms |
| insert | 4 | **640 / 5.7 ms** (380–710) | 345 / 7.18 ms |
| txn    | 4 | **580 / 4.6 ms** (460–706) | 437 / 6.23 ms |

What this says so far:

- **read c1** is the cleanest runtime-only datapoint: 54 µs vs 72 µs per
  round trip — the thread-per-core + io_uring path beats fork + blocking
  I/O by ~1.3× with an empty engine.
- **insert/txn c1** sit on the fdatasync floor (~4 ms/op on this disk);
  both architectures measure the storage stack, cascade modestly ahead.
- **read c4** scales ~2.5× on both; pgmodel holds a small edge at p50 —
  nothing in the current single-table debug engine stresses per-core
  independence yet, so this is a tie within noise, not a verdict.
- **txn/insert c4, post-fix:** cascade now *beats* the pgmodel twin
  (group commit: ~1 `fdatasync` per wave of commits vs one per backend).
  The M2 opener bug this table used to report is fixed — see the next
  section. Cascade's c4 numbers move 450–710 ops/s run-to-run on this box;
  the pgmodel cells are single runs, so read the comparison as a shape,
  not a leaderboard.

## Findings already folded back into the runtime

- **Server-side `TCP_NODELAY` is part of the contract.** Without it,
  every multi-segment reply pays Nagle × delayed-ACK — measured as a flat
  50-60 ms per op at c1 (quantized in 10 ms steps) before the fix
  (`router.rs`, `accept.rs`; real PostgreSQL sets nodelay too). The bench
  exists to catch exactly this class of thing.

## Fixed in the M2 opener: the txn/insert concurrency collapse

At any concurrency ≥ 2 (and outright hangs at ≥ workers), txn/insert
throughput sat pinned at the c1 level while reactors stayed healthy
(stall counters 0, syncs advancing). Root cause was two defects, found by
tracing every ring op (`WALDBG` submit/complete instrumentation, since
removed in favor of the watchdog below):

1. **The crawl — the flush leader capped its drain at its own commit**
   (`target = filled.min(up_to)`): even when other sessions' records were
   already filled and waiting, each leader synced only its own bytes, so
   every `fdatasync` carried exactly one commit — group commit defeated
   by construction. The leader now drains the *whole* contiguous filled
   prefix (always a record boundary, FR-25), plus a 300 µs frontier window
   (the `commit_delay` analog) so sessions landing just after the leader
   join its barrier. Cost: ~+9% p50 at c1 (4.0 → 4.4 ms) — the knob to
   turn if the bare-metal gate disagrees with it.
2. **The hang — a lost CQE.** WSL2 (5.15) + glommio 0.9 occasionally
   never delivers the completion for a buffered write/fdatasync (~1 per
   few thousand multi-session ops; observed directly as a `write
   submit` with no matching `done`, the reactor alive on timers but that
   session frozen forever). glommio has no timeout of its own. Every WAL
   ring op is now watchdog-guarded (250 ms ≈ 50× the p99 sync): on a
   firing, the op is re-issued — safe because both are idempotent (pwrite
   at an explicit offset with the same bytes; a repeated `fdatasync`) —
   and after 3 firings the session gets an honest 58000 instead of an
   unbounded stall. Zero firings across the post-fix runs so far; a
   watch-item for the bare-metal gate.

Regression tests: `cascade-wal/tests/writer.rs::{leader_sweeps_the_whole_filled_prefix, frontier_window_lets_a_latecomer_join_the_barrier}`.

## Known issues (M2 queue)

- `max_us` occasionally shows 8-28 ms outliers on WSL2 (scheduling
  noise); p99 is the honest tail number here.
- The txn c1 latency cost of the frontier window (above) is untuned; revisit
  against bare metal.

## Layout

- `src/bin/bench-harness.rs` — load generator / server supervisor.
  Spawns or attaches, drives closed-loop client threads, prints one JSON
  report line, SIGTERMs a spawned cascade and relays its observables JSON.
  A `ServerGuard` kills the spawned server on any exit path.
- `src/bin/bench-pgmodel.rs` — the PostgreSQL-runtime twin (postmaster +
  forked blocking backends).
- `src/lib.rs` — the shared client, workloads, pinning, aggregation.
- `tests/parity.rs` — the fairness contract: identical wire vectors and
  identical WAL record shapes across both architectures.
