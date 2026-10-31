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
loop. The orchestrator is serving-thread-local by construction —
`TickJob` has no `Send` bound (jobs may hold executor-bound state);
boot ships an all-`Send` `OrchestratorInit` bundle and the serving
thread assembles via `Orchestrator::from_counters` + `register`.

**Checkpointer (T4.2, FR-31) — orchestrates only.** `src/checkpointer.rs`:
the housekeeper-side `CheckpointFanout` (a `TickJob`) `try_send`s
`ControlMsg::FlushPass` over each NUMA pool's *designated* reactor's
dedicated control channel (`new_bounded::<ControlMsg>(64)`, separate
from the connection handoff channel; the formal `submit_to` is T5.1 —
this is the staged stand-in with the same topology). A full channel
refuses (counted in `CheckpointStats::refused`); the next tick
re-signals — the housekeeper never blocks on a worker. The worker side
runs `control_loop` as a sibling task on its own ring; `flush_pass`
iterates the reactor-local BPM's `dirty_pages()` (filter: this DB),
writes each back through the T3.2 gate, and advances the per-DB
`TruncatableLsn` (monotonic `fetch_max`). The frontier rule is
PostgreSQL-conservative: advance to the **minimum stamp over all pages
dirty at pass start** (refused pages' stamps still constrain — their
state lives only in the log), only when something stamped was dirty;
nothing-dirty ⇒ no advance. Live engine truth: workers host no BPM yet,
so live passes scan nothing and the frontier stays `Lsn(0)` — the whole
WAL stays reclaimable-by-nothing, which the T4.3 recycler must respect.
Observables on `Runtime`: `checkpoint_stats()` (requested/refused/
passes; `None` without a listener) and `truncatable_lsn()` (`Some`
whenever `wal_dir` is configured — the recycler reads it even on a
compute boot). The no-housekeeper-I/O property is tested at the
*syscall* layer: kernel per-task accounting
(`/proc/self/task/<tid>/io`; network never appears there), threads
located by comm name via `tid_named`.

**WAL recycler (T4.3, FR-32) — orchestrates only, own channel.**
`src/recycler.rs`: the housekeeper-side `WalRecycler` (a `TickJob`)
`try_send`s a `RecycleSignal { db, below }` — the frontier snapshot —
down the DB's **dedicated recycle channel** to the designated reactor
of the DB's WAL-home NUMA. The channel is the design: SPSC means the
recycler cannot share a sender with the checkpoint fanout, so the
fanout's set stays exactly the T4.2 whole one while a recycle signal is
*structurally* unable to reach any reactor but its DB's home one
(cross-DB isolation is topology, not a filter a refactor could drop).
The receiving end lands on the first worker of the home NUMA (boot
assigns it via `recycle_receivers`, mirroring the per-NUMA designated
rule); that worker runs `recycle_loop` as a sibling task next to
`control_loop` — no dispatch, the channel carries nothing else. The
home NUMA is resolved at boot **before the housekeepers spawn**
(`walhome::resolve_wal_home`, extracted so the recycler target and the
writer construction consume one decision): tick-time resolution would
read the persisted-homes file on the housekeeper and break the no-I/O
property. Registration order in the serving orchestrator: StallWatch →
CheckpointFanout → WalRecycler — consumer after producer, so a tick's
recycle observes the same tick's frontier advance. The worker-side
`RecycleSite` computes `cascade_wal::recycle_decision` (strictly-below
the snapshot, quantized to whole 16 MB windows, saturating at seg=0)
and never touches the frontier — only the checkpointer writes it.
Refusals (full channel) are counted; recycle is idempotent per
snapshot, so a drop costs one tick of staleness. Live engine truth:
frontier pinned at 0 (no worker BPM), so a live recycle reclaims zero
bytes and `db_<db>.log` stays **byte-identical** across a full second
of ticks while recovery still replays every committed row — the
boundary math itself (±1 at exactly the truncatable LSN) is pinned at
the cascade-wal level. The physical rotate-and-retire lands with the
segmented WAL (§8.2). Observable: `Runtime::recycle_stats()`
(signals/refused/passes/reclaimed_bytes; `None` without a listener or
a WAL).

## Open Design Questions

See the spec's §11. Highlights:
- Specific Glommio version and any required forks.
- CPU topology discovery: `hwloc-rs` vs hand-rolled `/sys` parser.
- Reactor stall plumbing details (sampler thread vs in-band).
- Whether `submit_to` is public API to higher crates or restricted.
