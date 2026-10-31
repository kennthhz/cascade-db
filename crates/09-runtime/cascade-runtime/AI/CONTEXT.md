# cascade-runtime — AI Design Context

## Authoritative design

**The full runtime architecture lives in [`docs/runtime-architecture.md`](../../../../docs/runtime-architecture.md).** That document supersedes any informal description in this file. The summary below is for fast-load context only.

## Role

Thread-per-core executor over Glommio. Glues the kernel-bypass I/O model (§3.1) to user-level scheduling. Owns the cross-reactor coordination primitive (`submit_to`) and the cooperative-yielding contract.

## Pillar Reservations

- **§3.2:** the pillar this crate *is*.
- **§3.1:** each worker owns one `io_uring` ring. No cross-core ring sharing.
- **§6.1 (Tenant):** every task runs in a `TenantContext` (from `cascade-tenant`) so future quotas can throttle.
- **§6.2 (Observability):** reactor-stall detector publishes via `cascade-telemetry`.

## Hard Invariants (from design spec §12)

1. **One worker, one core, one `io_uring` ring.** No work-stealing across cores.
2. **Connections are pinned for life.** Assigned at accept; never migrated. Lets connection state stay `!Send`.
3. **CPU pinning is real** (`sched_setaffinity` on Linux).
4. **NUMA-local allocation** for every per-reactor structure. Cross-socket hot-path reads are bugs.
5. **`submit_to` is not used on the OLTP query hot path.** Cross-reactor RPCs are for Tier C writes and background coordination only.
6. **Cooperative yield contract:** every long-running operator yields at batch boundaries and calls the runtime's `yield_point()` against the CPU deadline. (Glommio's `yield_if_needed()` only yields when the run queue is non-empty — the deadline tracking is ours; spec §7.1.)

## State Tier Model (design spec §4)

Three tiers govern what concurrency primitives apply where:

- **Tier A — Connection-local (`!Send`).** Query state, operators, batches. Owned by exactly one reactor. Zero atomics.
- **Tier B — Shared substrate.** BPM, WAL writers, XID allocators, undo, access methods. `Arc<…>` with lock-free or sharded internals. Atomics on the hot path.
- **Tier C — Replicated metadata.** Cluster catalog, resource groups, config. Per-reactor `ArcSwap<Snapshot>`. Reads are pointer loads.

Cascade DB is **not** strict shared-nothing. It accepts Tier B atomics in exchange for single-DB scalability. This trade is explicit and load-bearing.

## Configurable Topology

Two coordinated knobs at different scopes:

```toml
# Cluster-wide hardware policy
[runtime.topology]
mode = "physical"   # default — P99-optimized; HT siblings idle
# mode = "logical"  # throughput-optimized; HT siblings used

# Cluster default for per-DB NUMA affinity (DBs can override)
[runtime]
default_db_numa_affinity = "single"  # default — DB bound to one NUMA, latency-optimized
# default_db_numa_affinity = "cross" # throughput-optimized; spans all NUMAs
```

| Knob | Scope | Behavior |
|---|---|---|
| `topology.mode` | **Cluster-wide** | All databases on the host share the SMT choice. |
| `default_db_numa_affinity` + per-DB `WITH (numa_affinity = …)` | **Per-database** | One DB can be `single` (NUMA-bound), another `cross` (machine-spanning), in the same cluster. |

Both modes mandate NUMA-local allocation per worker.

## Connection routing — NUMA-affinity-aware

On accept, the housekeeper looks up the connection's `DatabaseId → numa_affinity` in the Tier C catalog:

- `single`-affinity DB → choose least-loaded worker **within the bound NUMA**. Refuse if no workers available there (do not silently violate the affinity contract).
- `cross`-affinity DB → choose least-loaded worker across the whole machine.

See runtime spec §5.2 for the full routing policy.

## Out of Scope

- Connection migration across reactors (breaks `!Send` pinning).
- Multi-machine clustering / distributed query.
- Dynamic core resize at runtime (requires restart).

## Existing Code (M1 debug slice, T1.x–T3.3)

The single-DB debug engine is real end-to-end: pgwire accept → auth (FR-9)
→ least-loaded routing (FR-10) → fd handoff (FR-13) → per-reactor session
loop (FR-16) over a per-worker `Rc<Table>`. Commits take the FR-27 order
(WAL append → fdatasync → apply → ack) through the shared `WalWriter`;
`boot_routed` wires the writer homed per affinity (A14) and opens one
handle per reactor (I2).

**Boot recovery (T3.3, FR-7).** `recovery::prepare(wal_dir, db)` runs on
the boot thread *before* the socket binds — the server (`bin/cascade-server`)
orders it prepare → bind → boot. Every boot walks the log and replays
committed transactions (the WAL is the slice's only durable state home;
even a clean stop replays), producing a `RecoveryPlan` that boot consumes:
the writer resumes at `WalWriter::new_at(replay_end)`, and every worker's
table is seeded with the replayed rows before its first session. Verdicts:
torn tail (declared length past EOF) truncates and boots; uncommitted
txns drop; a complete-length CRC failure refuses the boot outright. A
clean marker (`db_<db>.clean`) stamped at SIGTERM makes the *next* boot
clean — bytes past it still count as a crash. Test-only crash points
(`recovery::crash_barrier`, armed via `CASCADE_TEST_BARRIER_DIR`) park a
session at reserved/midfill/synced so the child-process harness
(`bin/cascade-server/tests/crash.rs`) can `kill -9` at exact protocol
points and assert the replay verdict on the first post-restart connection.

**Orchestrator tick loop (T4.1, D9, FR-5, FR-31's frame).** Background
work is centralized on the *serving* housekeeper: `Orchestrator`
(`src/orchestrator.rs`) owns a fixed-grid tick schedule
(`deadline + n·interval`), and the router loop interleaves it with accept
via `SO_RCVTIMEO` — `accept` sleeps up to the next tick deadline
(`until_next`), and every timeout return fires `poll_due` (at most ONE
tick per call; a buried grid re-arms one interval from *now*, skip-late —
no burst catch-up, no compounding drift, and no re-entrancy: jobs run
sequentially on the one thread). Jobs implement `TickJob` and are
registered at boot; `StallWatch` wraps the §7.2 `coop::poll_stall` pass
(what `Runtime::poll_stalls` does on demand). Observables on `Runtime`:
`orchestrator()` → ticks fired, worst lateness, housekeeper wakeups
(each accept return = 1; idle wakeups ≈ ticks — a busy-wait shows ~10⁶).
`tick_ms` config knob (default 1000). A listenerless boot never ticks
(`orchestrator() == None`); the plan's T4.4 shutdown drain builds on this
loop.

## Open Design Questions

See the spec's §11. Highlights:
- Specific Glommio version and any required forks.
- CPU topology discovery: `hwloc-rs` vs hand-rolled `/sys` parser.
- Reactor stall plumbing details (sampler thread vs in-band).
- Whether `submit_to` is public API to higher crates or restricted.
