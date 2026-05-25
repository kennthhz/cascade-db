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
6. **Cooperative yield contract:** every long-running operator yields at batch boundaries and calls `yield_if_needed()` against the CPU deadline.

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

## Open Design Questions

See the spec's §11. Highlights:
- Specific Glommio version and any required forks.
- CPU topology discovery: `hwloc-rs` vs hand-rolled `/sys` parser.
- Reactor stall plumbing details (sampler thread vs in-band).
- Whether `submit_to` is public API to higher crates or restricted.
