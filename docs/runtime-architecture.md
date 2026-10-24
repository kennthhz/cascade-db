# Cascade DB — Runtime Architecture

**Status:** Accepted (v1 spec). Supersedes any informal runtime discussion in the workspace AI/ notes.
**Audience:** Anyone touching the workspace. The invariants below cross-cut every crate from `cascade-storage` up to `cascade-pgwire`.
**See also:** [README §3.2](../README.md), [README §3.5](../README.md), [`AI/CONTEXT.md`](../AI/CONTEXT.md), [`AI/GLOSSARY.md`](../AI/GLOSSARY.md).

---

## 0. Decisions captured

| # | Decision |
|---|---|
| D1 | Thread-per-core execution. One pinned worker thread per CPU (mode-dependent — see §2.2). |
| D2 | Runtime: **Glommio**. Each worker runs a `LocalExecutor` with a private `io_uring` ring. |
| D3 | **NUMA-aware allocation.** Per-reactor working memory is always NUMA-local. Per-database Tier B (BPM, WAL, MVCC, undo, access methods) is NUMA-local for `single`-affinity DBs and may span NUMAs for `cross`-affinity DBs (see §2.5, §4 Tier B scoping). |
| D4 | SMT policy is **configurable**: `physical` (P99-optimized, default) or `logical` (throughput-optimized). |
| D5 | **Hybrid state model**: connection-local state is `!Send` per core (Tier A); shared substrate (BPM, WAL, MVCC, catalog) uses lock-free primitives accessible from any core (Tier B); cluster metadata is replicated per-core (Tier C). |
| D6 | Connection routing: **least-loaded core at accept**, no migration. A connection lives on one core for its lifetime. |
| D7 | Cooperative yielding contract: yield at batch boundary **and** when CPU deadline exceeded (default 500 µs). |
| D8 | Reactor stall budget: structured log at INFO for stalls ≥ 1 ms, at WARN for stalls ≥ 10 ms; kill-task off by default. |
| D9 | Background work centralized on the housekeeper for v1. |
| D10 | **Per-database NUMA affinity is configurable**: `single` (default; one NUMA node; latency-optimized) or `cross` (spans all NUMA nodes; throughput-optimized). Cluster default settable; per-DB override via DDL. |
| D11 | **Asymmetry**: SMT topology (`physical`/`logical`) is cluster-wide hardware policy; NUMA affinity is per-database placement. |

---

## 1. Goals & non-goals

### Goals (v1)

- **Predictable P99 latency** under OLTP load — no CFS interference, no surprise context switches, flat tail. (Achieved in full only by the latency stack — §2.5.)
- **A single database scales across all worker cores within its NUMA affinity** — one NUMA in `single` mode, the whole machine in `cross` mode. The customer chooses per database.
- **10 000+ concurrent connections** without process or kernel-thread bloat.
- **`io_uring`-native async I/O** with per-core rings; no global I/O mutex.
- **Cooperative fairness** between OLTP (sub-millisecond) and OLAP (multi-second) queries sharing a core.
- **NUMA-aware** memory allocation and connection routing.
- **Tenant-context propagated** end to end (README §6.1 reservation).

### Non-goals (v1)

- Cross-machine distribution / sharding (sketched for Phase 8+).
- Connection migration between cores.
- Cross-database transactions (matches PG semantics, see README §3.4).
- Dynamic core resize at runtime — topology is fixed at boot; changes require restart.
- Strict shared-nothing for *all* state. We accept Tier B and Tier C in exchange for single-DB scalability.

---

## 2. Hardware partitioning — the Fortress model

The Linux kernel is asked to stop scheduling on most of the machine. Concurrency control moves entirely into user space.

### 2.1. Core roles

| Role | Cores | Responsibilities |
|---|---|---|
| **Housekeeper** | `0..H` (default `H = 1`) | OS daemons, SSH, logging, NIC IRQs, listening socket, connection accept + dispatch, background work (checkpointer, stats, Tier C publication). Subject to normal CFS scheduling. |
| **Worker (Fortress)** | `H..N` | Database execution. Pinned, isolated from CFS, NUMA-aware, one `glommio` reactor each. |

`H` is configurable. Single-core dev boxes still work (you give up isolation). The `H..N` notation above is the idealized contiguous case; the actual worker set is computed from the discovered topology (§2.2) and may be non-contiguous in `physical` mode (§2.4).

**Accepted risk (v1):** with the default `H = 1`, a single non-isolated core handles accept/dispatch, *all* background work (§9), structured logging, and hardware IRQs. Runtime telemetry watches housekeeper utilization; the first mitigation is raising `H`.

### 2.2. SMT policy — configurable

The deployment chooses, at startup, between two topology modes:

```toml
[runtime.topology]
mode = "physical"   # default: one worker per physical core. HT siblings idle.
# mode = "logical"  # one worker per vCPU (logical core). HT siblings used.
```

| Mode | Workers spawned | Best for |
|---|---|---|
| `physical` (default) | One per physical core | P99-latency-sensitive workloads. No L1/L2 thrash from HT siblings — idle siblings are isolated but worker-less (§2.4). SaaS multi-tenant with SLAs. |
| `logical` | One per vCPU including HT siblings | Throughput-heavy workloads. More parallel execution units; tolerates more cache contention. Bulk OLAP, batch jobs. |

In both modes:
- NUMA-local allocation is mandatory.
- Workers are pinned via `LocalExecutorBuilder::pin_to_cpu(...)`.
- Each worker still owns exactly one `io_uring` ring.

CPU topology discovery uses `hwloc` (preferred) or `/sys/devices/system/cpu/` as a fallback.

### 2.3. NUMA — awareness mandatory; Tier B placement is per-DB

A worker pinned to socket 0 with its working memory allocated on socket 1 pays a 2–3× latency penalty on every cache miss. We forbid this absolutely for **per-reactor working memory**. For **per-database Tier B state** (BPM, WAL, MVCC, undo, access methods), placement follows the database's NUMA affinity (§2.5).

| Invariant | Enforcement |
|---|---|
| Each worker allocates per-reactor working memory from a NUMA-local arena. | Per-worker allocator (e.g., `mimalloc` with thread-local heaps) bound to the worker's NUMA node. |
| For **`single`-affinity DBs**: BPM, WAL, MVCC, undo all live on the DB's bound NUMA. Workers serving the DB also live on that NUMA. Hot-path cross-NUMA reads are bugs. | See §2.5, §4 Tier B scoping, §5.2 routing, §8.1 BPM. |
| For **`cross`-affinity DBs**: pages distributed across all NUMA pools; WAL/XID/MVCC pinned to one designated NUMA; workers on any NUMA may serve. Cross-NUMA hot-path reads are the documented trade. | See §2.5, §4 Tier B scoping. |
| The housekeeper's listening socket dispatches connections per §5.2: NUMA-constrained candidate set for `single`-affinity DBs; least-loaded machine-wide with NIC proximity as tie-breaker for `cross`-affinity DBs. | Routing policy in §5.2. |
| Background tasks run on the housekeeper — a small, non-isolated core set (one core by default, §2.1). Accept this asymmetry for v1. | See §9. |

### 2.4. Kernel deployment requirements

For predictable performance, the host kernel cmdline must include:

```text
isolcpus=<workers>          # remove worker cores from CFS general scheduling
nohz_full=<workers>         # suppress scheduler tick on isolated cores
rcu_nocbs=<workers>         # offload RCU callbacks
irqaffinity=<housekeepers>  # force all hardware IRQs to housekeeper cores
```

`<workers>` / `<housekeepers>` are kernel CPU lists (comma-separated ranges, e.g. `isolcpus=2-9` — the kernel does not accept parenthesized notation). The lists are **generated at provisioning time** from the machine's topology (§2.2) — they must exist before the kernel boots, so Cascade cannot derive them at its own startup — and are not copied by hand: in `physical` mode the worker set need not be contiguous, since HT-sibling enumeration order varies by platform. These are deployment-time requirements, documented in the deployment guide. Cascade DB verifies the actual affinity at startup and logs a loud warning if the kernel still ticks on a worker core.

**Idle HT siblings in `physical` mode:** the `isolcpus` set is *all* non-housekeeper cores — worker cores **and** their idle siblings (isolated, but no worker is spawned on a sibling). A sibling left in the general CFS domain shares L1/L2 and execution ports with its worker and reintroduces the exact thrash `physical` mode exists to prevent. Provisioning additionally pins system services to the housekeeper cores (e.g., systemd `CPUAffinity`); the deployment guide carries this checklist item.

### 2.5. NUMA scaling boundary & the latency/throughput stacks

#### 2.5.1. Per-database NUMA affinity

NUMA-local allocation (§2.3) preserves cache-line locality only if **the workers serving a database and the memory backing its pages live on the same NUMA node.** On multi-NUMA hosts there are two ways to honor this — and they trade off latency vs. throughput. Cascade DB exposes the choice as **per-database configuration**:

```sql
-- Latency-optimized: DB bound to one NUMA node.
CREATE DATABASE oltp_prod WITH (numa_affinity = 'single');

-- Throughput-optimized: DB spans all NUMA nodes.
CREATE DATABASE bulk_warehouse WITH (numa_affinity = 'cross');
```

Cluster-level default (inherited by new DBs that don't specify):

```toml
[runtime]
default_db_numa_affinity = "single"   # or "cross"
```

| Mode | Vertical scaling unit | Page residency | Worker pool | Hot-path memory access | Best for |
|---|---|---|---|---|---|
| **`single`** (default) | **One NUMA node** | All pages on the DB's bound NUMA | Workers on the same NUMA | Always local | OLTP, multi-tenant SaaS, latency SLAs |
| **`cross`** | **Full machine** | Pages distributed across NUMAs | Any worker on any NUMA | Mix of local + cross-NUMA | Single-DB analytics, batch, throughput-first |

A `single`-affinity DB is bound to a specific NUMA at first activation. The binding is stored in the cluster catalog: `pg_database.datnumaffinity` records the mode (`single` / `cross`); `pg_database.datnumanode` records the bound NUMA for `single`-affinity DBs. Changing it later requires the migration operation described below.

A `cross`-affinity DB has no NUMA binding — pages allocate across all NUMA pools via `hash(PageId) mod num_numas` (deterministic, so a page's home pool is computable from its `PageId`; see §8.1). Workers on any NUMA serve its connections.

#### 2.5.2. The cluster-wide / per-DB asymmetry

Two knobs control the latency/throughput trade-off, at *different scopes*:

| Knob | Scope | Why |
|---|---|---|
| **Topology mode** (`physical` / `logical`) — §2.2 | **Cluster-wide** | SMT is a hardware-utilization choice for the whole host. All databases on the host share it. Cannot be enabled per-DB. |
| **NUMA affinity** (`single` / `cross`) — §2.5.1 | **Per-database** | Page residency and worker assignment are per-DB. One DB latency-bound, another throughput-spread, both on the same host. |

A deployment that hosts *any* latency-sensitive DB should set `topology.mode = "physical"` cluster-wide — even if other DBs on the same host choose `numa_affinity = 'cross'`. The throughput-mode DBs give up some SMT-derived throughput; the latency-mode DBs keep their flat P99. Worthwhile trade for mixed workloads.

#### 2.5.3. The latency stack vs. throughput stack

Customers configure four coordinated knobs to land on one of two coherent product postures:

**Latency stack** (default; OLTP, SaaS, P99 SLAs):

```text
kernel cmdline:     isolcpus / nohz_full / rcu_nocbs / irqaffinity   (§2.4)
cascade.toml:       [runtime.topology] mode = "physical"             (§2.2)
                    [runtime] default_db_numa_affinity = "single"    (§2.5.1)
per-worker memory:  NUMA-local arena (always)                        (§2.3)
```

Every memory reference on the OLTP hot path is NUMA-local: per-reactor working memory (Tier A) hits the worker's private L1/L2, and Tier B state is shared only among the cores of *one* NUMA node — it stays within that node's shared L3 and never crosses sockets. The Tier B atomics and shared-L3 traffic of §4 are part of this cost model. Flat P99.

**Throughput stack** (opt-in; single-DB analytics, batch, throughput-first):

```text
kernel cmdline:     isolcpus / nohz_full / rcu_nocbs / irqaffinity   (still mandatory)
cascade.toml:       [runtime.topology] mode = "logical"              (SMT on)
                    [runtime] default_db_numa_affinity = "cross"     (or per-DB)
per-worker memory:  NUMA-local arena (always)
```

Trades flat P99 for higher aggregate CPU utilization. Use when the workload doesn't have per-query SLAs.

A deployment opting out of any single piece of the latency stack is consciously trading latency for something else. The four knobs are independent; we document them as a stack to make the coherent product posture obvious.

#### 2.5.4. Mode switching (migration)

`ALTER DATABASE … SET numa_affinity = …` is supported but **not a hot operation**:

- `single` → `cross`: re-home the DB's full Tier B set — re-place pages into their `hash(PageId)` pools (§8.1), choose and persist the WAL/XID/MVCC home NUMA, and open per-reactor handles on the newly eligible reactors (§8.2).
- `cross` → `single`: choose the bound NUMA and consolidate the whole Tier B set onto it — pages onto its pool, WAL/XID/MVCC/undo home moved, `datnumanode` updated.

For v1 this is an admin operation: drain in-flight queries on the DB, perform the migration, resume. Brief per-database unavailability. The runtime exposes the primitive; the admin tooling that orchestrates it is a Phase 7+ deliverable. The DDL surface (`ALTER DATABASE`) is in v1; the implementation may initially return `unsupported_feature` until the migration runner lands.

---

## 3. The Glommio reactor model

### 3.1. One reactor per worker

Each worker core runs exactly one `glommio::LocalExecutor`:

```text
+---------------------- Worker core 1 ---------------------+
| glommio LocalExecutor (pinned to physical/logical core)  |
|   |-- io_uring SQ/CQ (private)                           |
|   |-- NUMA-local memory arena                            |
|   |-- Run queue (FIFO + deadline yields)                 |
|   |-- Tier A connections (Rc, RefCell, !Send)            |
+----------------------------------------------------------+
```

The reactor never accepts work-stealing. Tasks scheduled on Core 1 only run on Core 1.

### 3.2. Why Glommio over alternatives

| Runtime | Outcome |
|---|---|
| **Glommio** ✓ | Designed for thread-per-core + `io_uring` + per-reactor `!Send` task model. Used in Datadog Husky / Vector. Native fit (we layer `Arc<…>` Tier B substrate on top, where atomics are acceptable). |
| `monoio` | Similar shape, newer, less battle-tested. Reconsider if Glommio stalls. |
| Hand-rolled over `io_uring` | Maximum control. Seastar (C++) took this path. ~6+ months of additional work; revisit if Glommio limitations bite. |
| `tokio::task::LocalSet` over multi-threaded tokio | Worst-of-both: pays multi-threaded tokio overhead for single-threaded execution. **Rejected.** |
| `tokio-uring` (single-threaded) | Lacks Glommio's cross-core `submit_to` primitive and topology controls. Rejected. |

A separate ADR will record the Glommio decision once we've vendored a specific version. Until then this section is the canonical reference.

### 3.3. What's on each reactor

- **Owned (Tier A, `!Send`):** connection state machines, query operators, batch buffers, prepared statements, cursors, in-flight session settings.
- **Borrowed (Tier B / Tier C, via `Arc<…>`):** buffer pool, WAL writers, XID allocators, catalog snapshots. Accessed but not owned.
- **Private I/O:** `io_uring` ring. No other reactor sees it.

---

## 4. State tier model

The most important architectural decision in this document. Three tiers, with different concurrency rules.

### Tier A — Connection-local (strict `!Send`)

| What lives here | Pattern |
|---|---|
| Connection state machine (the long-running async task) | `!Send` task in the owning reactor |
| Query operators, vectorized batches, intermediate row sets | `Rc<…>`, `RefCell<…>`, no atomics |
| Prepared statements, portals, cursor positions | Owned by the connection |
| Per-session GUCs / settings | Owned by the connection |
| In-flight transaction's working memory (lock-acquisition list, undo writes pending, dirty page pins) | Owned; pins released on yield-aware paths |

**Properties:**
- Pinned to one core for the connection's lifetime — accept binds it, disconnect drops it.
- Zero atomic operations on hot path.
- Compiler-enforced via `!Send` types.

This is where Rust's borrow checker and lifetime system earn their license fee.

### Tier B — Shared substrate (lock-free / sharded concurrency)

| What lives here | Concurrency mechanism |
|---|---|
| Buffer Pool frames + `PageId → Frame` table | **One BPM per NUMA node**; concurrent hash map (`flurry` / `scc`) + per-page RW-latch with OLC for index reads |
| Per-DB WAL writer | `Arc<WalWriter>` per DB, group commit; one fsync per batch |
| Per-DB XID allocator | `AtomicU64` per DB |
| Per-DB MVCC visibility (active-XID set, commit log) | Lock-free; readers snapshot without blocking writers |
| Undo segments | Append-only per DB; lock-free segment pointer |
| Heap, B+Tree, columnar projections (in-memory state above `PageStore`) | OLC / hand-over-hand latching; no global locks |
| Workspace metrics counters (`pg_stat_statements`-style) | Per-core counters, summed on read |

**Properties:**
- Lives behind `Arc<…>`. Any worker *with the right NUMA proximity* may access it (see scoping below).
- Atomic operations and per-page latches on the hot path. We accept this cost.
- **No coarse locks.** Every mutex-equivalent must justify itself.

**This is the trade.** Strict shared-nothing would give us zero atomics on the BPM hot path, at the cost of single-DB throughput capping at one core. We chose scalability.

#### Tier B scoping by NUMA affinity (§2.5)

Tier B is *not* a single flat namespace — it is **scoped by the owning database's NUMA affinity**:

| Tier B structure | For a `single`-affinity DB | For a `cross`-affinity DB |
|---|---|---|
| **BPM (page table + frames)** | All pages allocated from the DB's bound NUMA pool. Only workers on that NUMA may touch them. | Pages distributed across all NUMA pools; workers on any NUMA may touch any page (cross-NUMA traffic accepted by customer choice). |
| **WAL writer** | Lives on the DB's bound NUMA. Workers on that NUMA append directly. | Lives on one designated NUMA (chosen when the DB enters `cross` affinity — creation or migration, §2.5.4); cross-NUMA workers append via the `Arc<WalWriter>` — cross-NUMA cost on commit path. |
| **XID allocator** | Bound NUMA. | Single home NUMA (low contention; cost negligible). |
| **MVCC active-XID / commit log** | Bound NUMA. | Lives with WAL writer's NUMA. |
| **Undo segments** | Bound NUMA. | Distributed like data pages, or pinned with WAL — TBD (§11). |
| **Heap / B+Tree / columnar in-memory state** | Same NUMA as the pages they index — i.e., bound NUMA. | Distributed alongside pages. |

The invariant: **for `single`-affinity DBs, every Tier B touch on the hot path is NUMA-local.** For `cross`-affinity DBs, some Tier B touches will be cross-NUMA — that is the trade the customer made.

### Tier C — Replicated cluster metadata (per-core snapshots)

| What lives here | Why it can't be Tier A or B |
|---|---|
| `pg_database`, `pg_authid`, `pg_roles`, `pg_auth_members`, `pg_tablespace` (PG's "shared catalogs") | Describe relationships *between* databases; cannot belong to any single DB. |
| Resource group definitions (README §6.1) | Cluster-wide. |
| Cluster configuration / GUCs | Cluster-wide. |
| DB → NUMA affinity / bound-NUMA mapping (§2.5) | Cluster-wide; consulted by routing (§5.2) and by the BPM (§8.1). |

**Mechanism — Seastar's "global service" pattern:**

```text
                    +---- Housekeeper (writer / owner) ----+
                    |  Builds new immutable Snapshot.      |
                    |  Calls submit_to(every reactor, swap)|
                    +---------------+----------------------+
                                    |
                +-------------------+-------------------+
                |                   |                   |
                v                   v                   v
        +---------------+   +---------------+   +---------------+
        | Reactor 1     |   | Reactor 2     |   | Reactor N     |
        | ArcSwap<Snap> |   | ArcSwap<Snap> |   | ArcSwap<Snap> |
        +-------+-------+   +---------------+   +---------------+
                |
                | (read = one pointer load,
                |  zero synchronization)
                v
          connection task reads snapshot.foo
```

- Every reactor holds an `ArcSwap<ClusterSnapshot>`.
- Reads on the hot path: `snapshot.load()` — a single pointer load. No lock, no atomic compare-exchange.
- Writes are rare and serialized through the housekeeper: it builds a new immutable `ClusterSnapshot`, then `submit_to(every_reactor)` to swap pointers.
- Old snapshots drop naturally when reader refcounts hit zero.

**Net effect:** shared in concept, shared-nothing in mechanism. Reading the cluster catalog on the OLTP hot path costs the same as reading a constant.

---

## 5. Connection routing & lifecycle

### 5.1. Accept on housekeeper

The listening socket is bound on a housekeeper core. The housekeeper:

1. Accepts the TCP connection.
2. Performs the PG startup handshake (auth, `DatabaseId` resolution against the Tier C catalog snapshot).
3. Decides which worker reactor the connection will live on (§5.2).
4. Hands off the connection to the chosen reactor. Glommio's cross-executor channel requires `T: Send + Copy`, so what crosses is the **raw descriptor** (`RawFd` is `Copy`); the chosen reactor constructs the connection object from it on its own ring. The vendored Glommio version must support adopting an existing fd — an explicit version-pinning requirement (§11).

We do **not** use `SO_REUSEPORT` for accept distribution — the housekeeper makes informed routing decisions that the kernel can't.

### 5.2. Assignment policy

**v1: NUMA-affinity-aware, least-loaded reactor.**

The housekeeper consults the Tier C catalog snapshot to resolve the connection's `DatabaseId → numa_affinity`:

- **`single`-affinity DB:** restrict the candidate worker set to **reactors on that DB's bound NUMA node**. Within that set, pick least-loaded.
- **`cross`-affinity DB:** any worker on any NUMA is a candidate. Pick least-loaded across the whole machine (with NUMA proximity to the NIC as a tie-breaker).

"Load" is a low-cost composite (evaluated against whichever candidate set the affinity rule above produces):

- Active connection count on that reactor.
- Recent CPU utilization (sampled, exponentially-weighted).
- NUMA proximity to the connection's NIC/socket — **tie-breaker only, and meaningful only for `cross`-affinity DBs**. For `single`-affinity DBs the candidate set is already constrained to one NUMA.

The housekeeper maintains this load view from periodic samples published by each reactor (via Tier C-style snapshot, or a lightweight per-reactor atomic counter polled by housekeeper).

If a `single`-affinity DB's bound NUMA has no available workers (drained / failed), connection is refused with a clear error — we do **not** silently fall back to another NUMA, since that would silently violate the latency contract the customer chose.

**Considered and rejected for v1:**
- Round-robin: simpler, ignores load asymmetry from sticky connections.
- Hash-by-`DatabaseId`: caps a single hot DB at one core. (Earlier proposal; superseded.)
- Least-loaded with migration: complex, breaks `!Send` pinning invariants.
- NUMA-blind least-loaded: would silently violate `single`-affinity guarantees.

### 5.3. Pinned for life

Once assigned, **a connection lives on its reactor until close**. It never migrates. This is what lets every Tier A type stay `!Send`.

### 5.4. Disconnect / cleanup

On client disconnect, txn abort, or graceful shutdown:

1. Roll back any in-flight transaction (release locks, abort WAL record, drop undo refs).
2. Release page pins to the BPM (no leaks — `PageGuard` is RAII).
3. Drop session-owned prepared statements.
4. Free Tier A allocations from the NUMA-local arena.

Reactor accounting decrements `active_connections` so the housekeeper sees the load drop.

---

## 6. Cross-core API

### 6.1. `submit_to(core, async fn)` — coordinator-pattern primitive

For Tier C writes, cross-core observability queries, and any rare cross-reactor coordination:

```rust
// Conceptual — the actual signature uses Glommio's channel primitives.
pub async fn submit_to<F, Fut, T>(target: CoreId, f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{ ... }
```

- The closure runs on the target reactor; its result is delivered back to the caller's reactor.
- Inputs and outputs must be `Send` (since they cross reactors). Intermediate state inside the closure can be `!Send` — it never leaves the target reactor. This is why the API takes an `FnOnce` closure rather than a pre-built future: the future is *constructed on the target reactor*, so its internal state never needs to be `Send`. (The `async move { … }` blocks in the examples — §6.4, §8.4 — stand in for the closure form for readability.)
- Use sparingly. The hot path of an OLTP query must not call `submit_to`.

### 6.2. Tier B access — no special API, but NUMA-affinity is policy

Tier B structures live behind `Arc<…>` and are accessed by direct method calls from any reactor. The atomics / lock-free primitives are inside the structures.

**However:** the *Rust API* lets any reactor call these methods; the *invariant* (I4b, see §12) forbids a reactor on NUMA *j* from touching a `single`-affinity DB's Tier B state when the DB is bound to NUMA *k ≠ j*. Routing (§5.2) prevents this by construction — a `single`-affinity DB's connections never land on a non-bound-NUMA reactor. Background workers (housekeeper-side) that need to operate on a single-affinity DB's Tier B state must `submit_to` a reactor on the bound NUMA.

### 6.3. Tier C reads — direct, lock-free

```rust
// On any reactor, on any code path, including hot path:
let snapshot = cluster_snapshot.load(); // ArcSwap::load — pointer copy.
let role = snapshot.find_role(role_oid); // pure memory read.
```

### 6.4. Tier C writes — through the housekeeper

```rust
// From a worker reactor (e.g., handling CREATE ROLE):
submit_to(housekeeper_core(), async move {
    cluster_state.apply(change);              // serialized; only housekeeper writes.
    let new_snapshot = cluster_state.snapshot();
    broadcast_snapshot(new_snapshot).await;   // submit_to every reactor + ArcSwap::store
}).await
```

---

## 7. Cooperative yielding

The kernel is no longer pre-empting us. A misbehaving query could hijack its reactor for seconds and starve every other connection on that core. We solve this cooperatively.

### 7.1. The yield contract

Every long-running operator **must** yield at:

1. **Every vectorized batch boundary.** (1 024-row default; configurable per operator.) Cheap, always safe.
2. **When CPU time since last yield exceeds a soft deadline** (default 500 µs). Inside a batch if the batch is taking too long.
3. **Never across a blocking syscall.** Glommio's `io_uring` integration parks the *task*, not the reactor, on I/O — a runtime property, not a yield point operators insert.

**Implementation note — Glommio does not implement (2).** `glommio::executor::yield_if_needed().await` yields only when the local run queue is non-empty; it tracks no CPU time and knows nothing of deadlines. A task relying on it alone would never yield on an otherwise idle reactor — and would trip the stall detector (§7.2). The runtime therefore owns a deadline-aware helper:

```rust
// cascade-runtime. A reactor-local cell records when the calling task last
// yielded; every call refreshes it. Delegates to Glommio's yield primitives.
pub async fn yield_point();
```

- **Batch boundaries** call `yield_point()` unconditionally. Below the deadline it is a no-op unless the run queue is non-empty (it then delegates to `glommio::executor::yield_if_needed()`).
- **Internal operator yield points** (§7.3) call `yield_point()` — the deadline check is what protects a core from a single oversized batch.

### 7.2. Reactor stall detector

The runtime samples each reactor periodically and tracks the longest gap between yields:

| Threshold | Default | Action |
|---|---|---|
| Soft | **1 ms** | Emit `reactor_stall_soft_total` counter; structured log at INFO. |
| Hard | **10 ms** | Emit `reactor_stall_hard_total`; structured log at WARN; capture a lightweight stack trace if cheap. |
| Kill | configurable, **off by default** | Cancel the offending task. Admin opt-in only. |

All thresholds are configurable; the defaults above are the v1 product policy.

The stall detector publishes telemetry via [`cascade-telemetry`](../crates/01-foundation/cascade-telemetry/) — exactly the README §6.2 reservation hook this was built for.

### 7.3. Operators that must yield internally

A batch boundary isn't always natural. These operators yield mid-build:

| Operator | Yield points |
|---|---|
| **Sequential scan** over large segments | Every batch (already at batch boundary). |
| **Hash join build phase** | Between hash-table extension rounds. |
| **Sort** (in-memory or external) | Between runs; between merge passes. |
| **In-memory aggregation** | Between batch flushes to the hash table. |
| **Columnar vectorized scan** | Between segment chunks. |
| **`PL/pgSQL` interpreter** | Between SQL statements within a function; between loop iterations on `LOOP` constructs. |

Operator implementations call the runtime's `yield_point()` (§7.1) at these points. The call is a no-op unless the soft deadline has been exceeded or the run queue is non-empty.

### 7.4. What yielding does *not* solve

- **A single C extension that runs unchecked** (e.g., a regex on a giant string). Future mitigation: per-tenant CPU accounting (README §6.1) caps cumulative time, but the individual call still hogs its slot. Acceptable for v1; document.
- **Tail latency under a specific adversarial query.** The kill-task switch exists for this; admin opts in.

---

## 8. Shared-substrate concurrency primitives

How Tier B stays lock-free / fine-grained on the hot path.

### 8.1. BPM concurrent access

**Architecture: one BPM per NUMA node** (per §2.5 / §4 Tier B scoping). The BPMs are independent — different page tables, different physical memory, no shared state between them. Within a NUMA node, the BPM is shared by all cores on that node (Tier B).

**Page placement policy:**

| DB affinity | Page placement | Page lookup |
|---|---|---|
| `single` (bound to NUMA *k*) | All pages allocated from NUMA *k*'s BPM. | `BPMs[k].lookup(page_id)` — single-pool lookup, NUMA-local. |
| `cross` | Pages distributed across all NUMAs via `hash(PageId) mod num_numas` — deterministic, so placement is derivable from `PageId`. Round-robin-at-allocation was rejected for v1: not derivable, would force miss-probing. | `BPMs[hash(PageId) mod num_numas].lookup(page_id)` — single direct lookup, no miss-probing. Cross-NUMA access when the home pool is remote; accepted. |

**Page table (`PageId → Frame`) within each NUMA pool:** concurrent hash map with versioned reads. Candidates: `flurry` (Rust port of Java's ConcurrentHashMap), `scc::HashMap`, or a hand-rolled sharded array. **Decision deferred to BPM crate ADR.**

**Per-page latch:** RW-latch with an **optimistic-read** path. Readers grab a version stamp, do their work, validate the stamp on exit. If the stamp moved, retry. This is the Optimistic Lock Coupling pattern Cascade B+Tree already plans to use.

**Per-page accounting:** atomic refcount (pin count) and atomic dirty bit. `Arc<Frame>` is too coarse for pages we pin millions of times per second; we use raw atomics with `PageGuard` (RAII) on top.

**Eviction:** within each NUMA pool, sharded by `hash(PageId) mod cores_on_this_numa`. Each core handles eviction decisions for "its" page-table shard *of its NUMA's pool*. Reads from any core on the same NUMA; writes (eviction, dirty-flush scheduling) handled by the shard's home core.

**Ring ownership:** shared BPM state (frames, page table, atomics) is plain memory — only I/O is reactor-bound. Each reactor opens its own `PageStore` file handles (handles never escape `cascade-storage`, I2) and submits page-in reads on its private ring (§3.3). Writeback I/O is submitted by the shard-home reactor and by the checkpointer via `submit_to` (§9). No BPM object carries a file handle across reactors.

**Memory sizing:** each NUMA's BPM is configured independently. For asymmetric NUMA nodes (uncommon but possible), the runtime accepts per-NUMA sizing rather than splitting one total uniformly.

### 8.2. WAL group commit

One `WalWriter` per database, behind `Arc<WalWriter>`:

```text
Reactor A   Reactor B   Reactor C
   |           |           |
   v           v           v
+------------------------------------+
| WalWriter (per-DB, Arc, mutex-free |
| in-memory ring buffer + leader     |
| election via atomic CAS)           |
+------------------------------------+
                |
                v
        io_uring fdatasync (on the leader's own ring)
```

- `append_wal(payload) -> Lsn`: atomically reserves a byte range in the ring buffer, writes the record, returns the LSN.
- `flush_wal(up_to_lsn)`: if no flush is in flight, this caller becomes the leader, kicks off `io_uring fdatasync`, and notifies waiters when done. If a flush is already in flight covering this LSN, the caller waits on the notification.
- **Reserve vs. fill (invariant):** `append_wal` hands out LSNs at reservation time; the caller writes the record afterwards. Flush eligibility may advance only to the **filled watermark** — the highest LSN whose record is completely written — never the reserved watermark; otherwise the leader could fsync a region containing a torn record. How the two watermarks are represented in the ring buffer is the WAL crate's choice (ADR).
- Net effect: many concurrent commits, one fsync per batch. PG does this; we do it without LWLock.
- **Ring ownership (why this is sound with private rings, §3.3):** the shared state inside `Arc<WalWriter>` is the in-memory ring buffer and its atomics — *not* a file handle. Each reactor opens its own handle to the DB's WAL segment files (handles never escape `cascade-wal`, I2). `append_wal` touches only shared memory (atomic reservation + memcpy), so it is safe from any reactor. The flush leader submits `fdatasync` on **its own** reactor's ring, through its own handle; `fdatasync` is per-inode, so it durably covers records appended through every reactor's handles.
- **NUMA placement (per §4 Tier B scoping):**
  - For a `single`-affinity DB, the WAL writer lives on the DB's bound NUMA. All appending reactors are also on that NUMA — every append is NUMA-local.
  - For a `cross`-affinity DB, the WAL writer lives on **one designated NUMA** (chosen when the DB enters `cross` affinity — at creation or via migration, §2.5.4 — and persisted in the cluster catalog alongside the DB's affinity columns; exact column TBD, see §11). Reactors on other NUMAs append via the same `Arc<WalWriter>`; the ring-buffer reservation incurs cross-NUMA atomic cost. This is the documented trade for `cross` affinity.

### 8.3. XID allocation

Per-DB `AtomicU64`. `fetch_add(1, Ordering::Relaxed)` on transaction start. Contention is irrelevant at OLTP rates; uncontended atomic add on modern hardware is ~5 ns.

### 8.4. Snapshot publication (Tier C)

```rust
// On the housekeeper:
let new_snapshot = Arc::new(cluster_state.build_snapshot());
for reactor in &all_reactors {
    let s = new_snapshot.clone();
    submit_to(reactor.id, async move {
        CLUSTER_SNAPSHOT.store(s);  // ArcSwap::store
    }).await;
}
// Old snapshots drop when last reader is done.
```

`ArcSwap` is the chosen primitive. It guarantees:
- Reads are wait-free.
- Writers don't block readers.
- Memory reclamation is automatic via `Arc`'s refcount.

---

## 9. Background work

v1: **all background work runs on the housekeeper.** This is the simplest viable model; we revisit if it bottlenecks.

| Background task | Owner | What it does |
|---|---|---|
| Checkpointer | Housekeeper | Orchestrates only: for each NUMA's BPM pool, `submit_to` a reactor on that pool's NUMA to iterate its dirty pages and schedule flushes **in-pool**, fanning flush I/O out to the shard-home cores (§8.1) — the housekeeper never touches another NUMA's Tier B state directly (§6.2); advances per-DB WAL-truncatable LSN. |
| WAL segment recycler | Housekeeper | Orchestrates only: `submit_to` the reactor owning the DB's WAL home to reclaim segments below the per-DB truncatable LSN — the bound NUMA for `single`-affinity DBs, the designated home NUMA for `cross` (§6.2, §8.2). |
| Statistics collector | Housekeeper | Aggregates per-core counters via `submit_to(*)` for views like `pg_stat_database`. |
| Catalog publisher | Housekeeper | Serializes Tier C writes; broadcasts snapshots. |
| Idle-connection reaper | Housekeeper | Times out connections; signals the owning reactor to drop them. |
| Reactor stall sampler | Housekeeper | Polls per-reactor stall counters; emits aggregated telemetry. (Exact plumbing — housekeeper-side polling vs in-band self-reporting — is the §11 open question; this row assumes polling.) |
| (Future) Logical CDC decoder | Housekeeper for v1 | Reads each DB's WAL, emits change events. May move to a dedicated reactor per DB in Phase 7. |

**Cross-core coordination:** any background task that needs work done on a specific reactor uses `submit_to(reactor, …)`. Worker reactors never queue work onto each other directly — coordination flows through the housekeeper. For per-NUMA Tier B work (e.g., flushing a `single`-affinity DB's dirty pages), the housekeeper targets a reactor on that DB's bound NUMA.

---

## 10. Cold start & shutdown

### 10.1. Boot sequence

```text
1. Parse config; validate topology mode (physical / logical) and cluster default_db_numa_affinity.
2. Query CPU topology via hwloc / /sys. Compute housekeeper + worker core sets, grouped by NUMA node.
3. Verify kernel cmdline (isolcpus, nohz_full, rcu_nocbs, irqaffinity) matches the generated lists (§2.4); warn loudly if not.
4. Spawn housekeeper reactor on core 0 (or H cores if H > 1).
5. For each NUMA node:
   a. Allocate that NUMA's BPM pool (sized per config; backed by NUMA-local memory).
6. For each worker core:
   a. Spawn glommio LocalExecutor, pin_to_cpu().
   b. Allocate NUMA-local per-reactor working arena.
   c. Bind the reactor to the BPM pool registry (`Arc<[NumaBpm]>`, indexed by NUMA): its own NUMA's pool is the `single`-affinity home; other pools stay reachable for `cross`-affinity lookups (§8.1).
   d. Open private io_uring ring.
7. Load Tier C from on-disk shared catalog (pg_database including datnumaffinity / datnumanode);
   publish initial snapshot to all reactors.
8. For each database (per cluster catalog):
   a. Resolve numa_affinity from Tier C; for single-affinity DBs, identify bound NUMA.
   b. Open data files via PageStore — per-reactor handles on every reactor
      that may touch this DB (§8.1 ring ownership).
   c. Open WAL files via WalStore — per-reactor handles on every reactor
      that may append to this DB: bound-NUMA reactors for `single`-affinity
      DBs, all reactors for `cross`-affinity DBs (§8.2 ring ownership).
   d. Run per-DB WAL replay until clean LSN (parallel across DBs).
9. Bind listening socket on housekeeper; begin accepting.
```

Boot is not fully parallel — Tier C must be published before workers start serving traffic. WAL replay can be parallel across databases.

### 10.2. Graceful shutdown

```text
1. Housekeeper: stop accepting new connections.
2. For each worker reactor (in parallel):
   a. Mark "draining."
   b. Wait for in-flight queries: each connection finishes its current statement,
      then refuses new ones with a clear error code.
   c. Wait for in-flight transactions to commit or roll back, up to drain_deadline.
   d. Any txn still active at deadline is force-aborted.
   e. Release locks, pins, undo refs.
3. Final per-DB WAL flush (each per-DB WalWriter flushes its tail on its home NUMA).
4. Final BPM flush across all NUMA pools: write dirty pages, advance per-DB checkpoint LSN.
5. Drop reactors; close io_uring rings; close files.
```

`drain_deadline` is configurable (default 30 s). Beyond it, in-progress txns are aborted to keep shutdown bounded.

### 10.3. Crash recovery

Boot detects unclean shutdown (WAL not at clean LSN, missing shutdown marker). Per-database WAL replay runs as part of boot step 8d above (parallel across DBs). This is a storage-layer concern (README §3.4) — the runtime only orchestrates.

---

## 11. Open questions

These are deliberately deferred. Each is small enough to resolve when the relevant crate lands; none affects the overall architecture above.

| Question | Where it lives |
|---|---|
| Specific Glommio version + any required forks; must support adopting an existing fd into a ring (connection handoff, §5.1). | Future ADR alongside the runtime crate's first real code. |
| CPU topology discovery: `hwloc-rs` vs hand-rolled `/sys` parser. | Runtime crate. |
| Concurrent hash map for BPM page table: `flurry` vs `scc` vs hand-rolled. | BPM crate ADR. |
| Reactor stall detection plumbing — sampler thread on housekeeper vs in-band per-reactor self-reporting. | Runtime crate. |
| Whether `submit_to` is public API to higher crates or restricted (most callers shouldn't need it). | Runtime crate API ADR. |
| **`cross`-affinity WAL/XID home NUMA selection**: chosen when the DB enters `cross` affinity (creation or migration, §2.5.4); whether to expose as DDL or pick automatically; its catalog column (alongside `datnumaffinity` / `datnumanode`). | Catalog + runtime. |
| **`single`-affinity DB placement at first activation**: how the housekeeper picks the bound NUMA (least-loaded NUMA, round-robin, admin-pinned). | Catalog + runtime. |
| **NUMA-affinity migration protocol** (`ALTER DATABASE … SET numa_affinity = …`): drain + full Tier B re-homing choreography (pages, WAL/XID/MVCC home, per-reactor handles — §2.5.4). Phase 7+ implementation; DDL surface in v1. | Future ADR. |
| **Fallback behavior** when a `single`-affinity DB's bound NUMA has no live workers (drained / failed). v1: refuse connection. Better: admin-driven failover. | Future ADR. |
| NUMA node selection when the machine has more nodes than worker cores. | Runtime crate. |
| Behavior on hot-attach / hot-detach of CPUs in cloud environments (vCPU resize). | Documentation: not supported; requires restart. |

---

## 12. Invariants summary (for code review and AI sessions)

The invariants below are the *enforcement contract* for every crate in the workspace. Reviewers and AI sessions should treat violations as architectural bugs, not stylistic issues.

| # | Invariant | Tier |
|---|---|---|
| I1 | Connection-local state is `!Send` and pinned to one reactor for the connection's lifetime. | A |
| I2 | No `RawFd` or `File` handle escapes `cascade-storage` / `cascade-wal`. | B |
| I3 | `PageId` is `(DatabaseId, SegmentId, PageNo)` — never `(file_path, offset)`. | All |
| I4a | NUMA-local allocation for all per-reactor (Tier A) working memory. Always. | A |
| I4b | For `single`-affinity DBs: all pages, WAL, MVCC state, undo, and workers serving that DB live on the DB's bound NUMA node. Hot-path cross-NUMA reads of this DB's Tier B state are bugs. | B |
| I4c | For `cross`-affinity DBs: cross-NUMA hot-path reads of Tier B state are expected and accepted as the documented trade. | B |
| I5 | All hot-path operations carry a `TenantContext` (README §6.1 reservation). | All |
| I6 | All hot-path operations accept `&Span` (README §6.2 reservation). | All |
| I7 | WAL-before-data: no dirty page is flushed until the WAL covering its mutations is durable. | B |
| I8 | WAL records carry full row images sufficient for logical decoding (README §6.3). | B |
| I9 | Tier C state is read via `ArcSwap::load()` only; writes go through the housekeeper. | C |
| I10 | Long-running operators yield at every batch boundary and call the runtime's deadline-aware `yield_point()` at internal sub-boundaries. | A |
| I11 | No coarse mutex on the hot path. Justify any `Mutex<…>` in code review. | B |
| I12 | `submit_to` is not used on the OLTP query hot path. | All |
| I13 | Connection routing honors DB NUMA affinity: `single`-affinity DB connections are only assigned to workers on the bound NUMA. | A |

---

## 13. References

- [README](../README.md) — product spec and pillars.
- [`AI/CONTEXT.md`](../AI/CONTEXT.md) — workspace operating manual.
- [`AI/GLOSSARY.md`](../AI/GLOSSARY.md) — shared types.
- [`crates/09-runtime/cascade-runtime/`](../crates/09-runtime/cascade-runtime/) — the crate this design governs.
- [`crates/09-runtime/cascade-tenant/`](../crates/09-runtime/cascade-tenant/) — Tier A `TenantContext`.
- Glommio: https://github.com/DataDog/glommio
- ScyllaDB / Seastar reactor model: https://seastar.io/shared-nothing/
- ArcSwap: https://docs.rs/arc-swap
