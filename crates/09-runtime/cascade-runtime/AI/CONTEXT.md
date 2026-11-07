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
| `default_db_numa_affinity` + per-DB `WITH numa_affinity = …` | **Per-database** | One DB can be `single` (NUMA-bound), another `cross` (machine-spanning), in the same cluster. (PG's CREATE DATABASE has no parenthesized WITH list — the raw grammar refuses it; T5.5 rides PG's real option form.) |

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
(`until_next`), and EVERY loop pass fires `poll_due` at its top (at most ONE
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

**Graceful shutdown (T4.4, FR-8) — §10.2 verbatim, in order, on the
serving housekeeper.** `src/shutdown.rs`: the whole sequence runs where
the background work lived (D9) — the accept loop breaks on a shutdown
flag (`Runtime::shutdown` sets it and wakes a blocked `accept(2)`
instantly via `shutdown(2)` on a **dup of the listening fd**; Linux
fails the accepter with EINVAL, and the flag check in the dead-listener
arm distinguishes that from a real listener death; ECONNABORTED is
transient and continues, never mistaken for death — that would skip
the drain). A connection that raced the flag is refused `57P01` (never
routed); sessions carry a `SessionEnv { draining, inflight }` — once
draining is set, the NEXT statement is fatal `57P01` and the connection
closes (FR-15 cleanup retested through this path), and the refusal is
re-checked **after** the in-flight increment, closing the race where a
statement slips in while the drain wait is concluding. The drain wait
counts **in-flight statements, not connections** — an idle client
blocks nothing, exactly like PG's txn-free idle backend — bounded by
`drain_deadline_s` (default 30): `wait_inflight` returns at zero or at
the deadline (the force-abort half is finished by step 5's reactor
drop), and the gauge is balanced by an RAII `GaugeGuard` — a manual
decrement would miss an unwinding panic and stick the gauge forever,
silently converting every later shutdown into the deadline path.
Final flushes ride the **third dedicated channel family**
(`DrainMsg::FinalFlush` to each designated reactor; SPSC — the control
channel stays pure T4.2-flush, the recycle channel pure T4.3-recycle):
the WAL-home reactor flushes the per-DB WAL tail first (step 3), then
every designated reactor runs one final pass (step 4) — passes run
**unconditionally**, so the trace proves flushes RAN rather than were
skipped as "nothing dirty" (the live frontier pins at 0 and the pass
still acks). The home site is identified by a **flag carried in the
sender tuple, never by ack slot position**: a persisted WAL home on a
pooled NUMA with no workers yields no flagged sender, and the drain
then refuses the marker (`shutdown_no_wal_home_reactor`) instead of
mistaking a non-home Ok for the tail flush. Acks are message-carried
(`mpsc::Sender<Result<..>>` inside the message — the staged form of
T5.1's `submit_to` return path) and share ONE deadline budget (a
wedged set delays shutdown once, not once per site); the clean marker
(T3.3) is stamped **only on the home ack** — a wedged other site
delays shutdown once but cannot unsync the marker. Every phase
boundary appends to a `ShutdownTrace`; `tests/shutdown.rs` asserts the
whole **sequence** (accept-stopped → refuse-new → sessions-drained →
final-wal-flush → final-bpm-flush → clean-marker → reactors-dropped),
plus idle-client non-blocking, listenerless quiet
(`["reactors-dropped"]`), the identity-keyed home ordering (unit), and
the deterministic 57P01 refusal driven at the session layer. Step 5 is
`Runtime::shutdown`'s joins: housekeepers first (a worker must live to
ack), then workers — a joined worker drops its ring, killing detached
session tasks and closing fds/WAL handles. Parked (non-serving)
housekeepers poll the flag on each `park()` wake and are explicitly
unparked — without that their join would hang forever (the flag exists
on every boot for exactly this). Report:
`ShutdownReport { phases, housekeepers_stopped, workers_stopped }`.
Known follow-ups (review-acknowledged, not T4.4): a byte-trickling
client can hold the serving thread inside `handshake` per-read bounded
but unbounded in total — a total handshake deadline belongs to the
pgwire slice.

**Idle-connection reaper (T4.5, FR-34) — the idle definition is the
design.** `src/reaper.rs`: the fourth dedicated channel family (SPSC —
control stays T4.2-flush, recycle T4.3-recycle, drain T4.4-final-flush).
The serving housekeeper's `IdleReaper` (a `TickJob`) `try_send`s an
empty `ReapTick` to **every** worker — idle connections live on all
rings, not just designated ones; the signal carries no timeout
deliberately, because the decision is made worker-side, on the ring
that owns the entries (the housekeeper only rings the bell). Each
worker's `reap_loop` (sibling task, sharing the sessions'
`Rc<ReapRegistry>` — Tier A, same ring) walks the reactor-local
registry and condemns entries that are **parked in their message read
AND whose last completed statement is older than `idle_reap_ms`**. Both
halves are load-bearing: idle runs from *last statement completion*
(`touch` fires exactly when each statement's RFB goes out — never from
connection age, and never across a statement's own runtime), and
`waiting` is set only inside the session's read wrapper (`reap_message`
in `session.rs`: a `poll_fn` that checks `is_dropped` first — returning
EOF, so the reap exits through the ordinary FR-15 cleanup, mid-txn
abort included — stores the poll's waker, then delegates to the pinned
underlying read). A session blocked in a write or a WAL sync is
`waiting == false` and structurally un-reapable; the reap snapshot is
exact (same-ring serialization), the waker is re-stored every poll, and
the entry survives condemnation for the session to observe (the
unregister guard removes it — a zombie entry would otherwise sit in the
registry forever). Condemned connections close without a wire message
(the client sees EOF); the reap-vs-arriving-query race is deliberate
and documented — a query already in the kernel buffer when the pass
snapshot condemned the connection loses, exactly like PG's
`idle_session_timeout` at tick granularity. `idle_reap_ms` defaults to
`0` = disabled (a default that kills idle client connections must be
opted into). **The stall rule (`stall_reap_ms`, default 30 000, `0`
= off) is the opposite trade and rides the same plane**: a statement
in flight longer than the bound condemns its session EVEN THOUGH it
is not parked in its read. The containment: this environment (WSL2
kernel 5.15 + glommio 0.9) occasionally never delivers an io_uring
completion — cascade-wal's `OP_WATCHDOG` documents it for buffered
file ops (idempotent, re-issued); a socket recv/send CQE can be lost
too, and those are neither, so the session would hang forever. Found
as a ~1-in-30 full-suite hang (`ticks_continue_under_client_load`):
2/4 client threads wedged, per-worker connection gauges `[1, 1]`,
rings alive (other sessions on the same rings completed), zero WAL
watchdog firings — one lost CQE per worker ring, mid-statement or
mid-response. The fix kills the backend at the bound (PG's own
termination of a stuck backend): `ReapEntry.in_flight_since` is set
at the statement's in-flight increment and cleared at its completion
`touch`; `reap_overdue(idle_timeout, stall)` walks BOTH rules; the
session side wraps the ENTIRE post-read window — framing refusals,
draining refusal, execute, WAL, every response write — in
`rescoped` (`session.rs`), the read wrapper generalized: it stores
the poll's waker and checks `is_dropped` first, so the reaper's wake
aborts the statement wherever it is parked, closing through the
ordinary FR-15 cleanup with no further wire output (the wedged op may
itself have been the response write). Clients reconnect — the load
test in `tests/tick.rs` retries a dead connection with fresh ids and
asserts the honest durability invariants (acked ids ⊆ WAL rows;
WAL rows − acked ≤ kills). Revisit both bounds on bare metal.
**Connect-order rule (found as a live three-way
boot deadlock: all executors spinning at 100%)**: a `connect()` blocks
until the peer endpoint connects on its own executor, so the serving
side must connect the signal families in the same order the workers do
(handoff → control → recycle → drain → reap) — connecting the reaper
before the drain deadlocked: the serving thread waited inside a
reap-sender connect for a worker still waiting inside its drain connect
for this thread's drain senders, sequenced after the reapers. The rule
now lives at the connect site in `router.rs`. Observables:
`Runtime::reap_stats()` (signals attempted / refused-full / connections
condemned across workers; `None` when disabled or listenerless).
Review follow-ups acknowledged, not T4.5: the session's connection
counter decrement (`session.rs`) is a manual line rather than an RAII
guard — the same unwind discipline ReapGuard/GaugeGuard follow would
apply to it. T4.6's review extends the same class: the FR-15 tail's
`rollbacks` increment (`session.rs`) is a second manual line beside it
— a panicking mid-txn session undercounts rollbacks as well as leaking
the gauge; one RAII tail guard would cover both.

**Statistics collector (T4.6, FR-33) — per-core counters, summed on
read.** `src/stats.rs`: each worker owns a `WorkerCounters` set (§4
Tier B — `Arc`-shared atomics), created in boot's per-core loop and
`Arc`-cloned only into that reactor's router route, session tasks, and
the read-side sum; attribution is by construction (a connection is
pinned for life to one ring, invariant 2) and nothing on a session's
hot path ever touches another core's counters. Semantics track
pg_stat_database's per-database tallies: `connections` is the live
gauge the router's least-loaded pick reads (FR-11) — the same set
doubles as the routing counter, one source of truth;
`statements` counts at `execute()` entry (attempted — parse errors
never reach the engine; empty statements are protocol no-ops and are
NOT counted, PG parity); `commits` counts explicit `COMMIT` of an open
txn plus autocommit writes (each INSERT its own transaction, PG
parity); `rollbacks` counts explicit `ROLLBACK`, the 25P02
`COMMIT`-acts-as-`ROLLBACK` path, a failed autocommit write's aborted
implicit transaction (counted at the failure — the txn never left
Idle, so no later path would count it; a failed explicit COMMIT is the
mirror case: its txn is still InTxn and deterministically ends in
exactly one counted rollback downstream), and the FR-15 mid-txn
disconnect cleanup (the same tail the reaper's sessions exit through —
T4.5's reap path shows up here too). The aggregate exists ONLY in
`StatsSnapshot`, materialized by `Runtime::stats()` summing the
per-core sets in one pass with Relaxed loads — sound without a fence
because every counter is monotonic (a snapshot is a lower bound at its
read instant; none can go backwards). The read is the STAGED stand-in
for §6.1's `submit_to` fan-in (ask each reactor to produce its own
slice, which is what would make these Tier A): T5.1 is not built, so
the sum reads the shared atomics directly from the caller — correct on
the §4 Tier B substrate, documented as the deviation, same discipline
as the checkpointer's staged control channel. Tests answer the plan's
review question with real traffic, not synthetic sums: one real txn
per worker with the split OBSERVED (least-loaded pick, per-worker
gauges assert exactly two reactors at load 1) and exact totals — any
double count in the plumbing (boot double registration, worker vs
router sharing, summing one set twice) breaks an equality; plus a
150-statement benchmark-sized workload with exact totals and the
abandoned-mid-txn rollback through the FR-15 tail.

**`submit_to` primitive (T5.1, FR-35, §6.1) — the closure crosses, the
future never does.** `src/submit.rs`: the signature is R14's verbatim
shape — `f: FnOnce() -> Fut + Send + 'static` with **no `Send` bound
on `Fut`** and `T: Send + 'static`. The asymmetry is the whole design:
the queued job carries only the boxed closure (a `Send` type, as
glommio's `new_bounded<T: Send>` requires), `submit_loop` invokes the
closure ON the target ring and spawns the returned future with
`spawn_local` right there — so `Rc`s and other ring-local state are
free inside the future, and no `unsafe` `Send` claim exists anywhere
in the module. The result crosses back over a hand-rolled oneshot
(std Mutex, state-then-waker lock order; cancel-on-sender-drop) whose
contract is "the caller always resolves, never hangs":
`SubmitError::Lost` when the job's task died or the queue tore down,
`Saturated` when the bounded queue is full — the codebase's
counted-refusal discipline, never a blocking caller ring.
`SubmitStats{submitted, executed, refused}` is the FR-36 debug
counter — T5.4's zero-probe reads `submitted` over an OLTP workload.
**Two properties consumers must know**: (1) a panicking job resolves
its caller `Lost` (glommio drops the dying task's future, the oneshot
cancels) but the panic then propagates through glommio's executor loop
— the WHOLE target ring dies, every in-flight caller gets `Lost`; for
T5.4 a panicking housekeeper job is a housekeeper-ring outage, and
recovery belongs there, not in the primitive (pinned by
`panicking_job_resolves_lost_and_the_ring_dies`). (2) The channel is
1:1 and ring-bound — glommio's `ConnectedSender` holds `Rc`, so a
`SubmitHandle` is `!Send`: clones submit concurrently but only from
the owning ring; fan-out = one handle per target, fan-IN from N rings
= N channels + N sibling `submit_loop`s on the target (the T5.4
housekeeper shape). **Bring-up lessons**: (a) the oneshot's poll must
restore `State::Empty` on the pending path — persisting the transient
`Taken` marker made a later `send()` see the channel as resolved and
silently DROP the value (caller hang; found by probe, covered by
`pending_then_send_wakes_the_stored_waker`); (b) the T4.5 connect rule
again — sequencing both halves' connects on one thread spins forever;
the halves must rendezvous concurrently on their own executors. The
compile-fail test is the plan's trybuild pattern: a closure capturing
an `Rc` is rejected at compile time with E0277 on the `F: Send` bound
(the `.stderr` is rustc-version-sensitive — regenerate with
`TRYBUILD=overwrite` on toolchain upgrades). Boot wiring is
deliberately absent until T5.3 (the first real consumer) — the same
staging discipline as the checkpointer's channel.

**Tier C snapshot + pointer-load reads (T5.2, FR-28, §6.3/§8.4) —
the pointer is the lock-free read.** `src/tierc.rs`: the T2.4 static
snapshot grew the ArcSwap read path and took FR-28's literal payload
name (`TierCSnapshot` → `ClusterSnapshot`); arc-swap 1.9.3 entered the
workspace for this (§8.4 names it the chosen primitive). The `Catalog`
holder wraps `ArcSwap<ClusterSnapshot>`: reads are `load()` — a Guard,
one pointer copy, no refcount bump, no lock, no compare-exchange — or
`load_full()` (one refcount increment) for the rare reader that must
hold its snapshot across an await; a Guard never spans an `.await`
because a live guard occupies one of the THREAD's debt slots (per-thread,
shared across every ArcSwap instance), which pushes that thread's other
loads onto the slower refcount path. The ONLY write is
`pub(crate) store(Arc<ClusterSnapshot>)`: no swap/rcu/compare_exchange
is re-exposed, so "no second writer path" (I9) is grep-auditable and no
external crate can write at all. (Written at T5.2, when boot's
`Catalog::new` was the only publication and the snapshot stayed
immutable after boot; T5.3's writer took over — see below. `Catalog::new`
survives as the TEST constructor; boot hands every reactor an EMPTY
catalog and publication is the writer's first act.) Consumers: router
`route()` pointer-loads per connection; boot's WAL-home resolve reads
the built-but-unpublished initial once pre-serving. Per-reactor
instances are T5.3's fan-out shape (R11's laggard adoption needs one
ArcSwap per reactor). The three §8.4 guarantees
are pinned by tests the review verified against arc-swap 1.9.3's actual
swap/pay_all/debt mechanics: the race test (barrier-synchronized 3
readers + writer over 5 000 whole snapshots riding the revision in
`bound_numa`; asserts coherent pairing, per-reader MONOTONE revisions —
a real invariant since the storage is one SeqCst-swapped pointer and
every snapshot is a distinct allocation, so no ABA — final-revision
visibility via the done flag's Acquire/Release, and that some reader
actually saw the pointer MOVE: the anti-"run and hope" assert), the
reclamation test (strong_count 2→1 at the store — pay_all drops the old
Arc synchronously — and Weak::upgrade fails once the last reader
finishes), and the guard-pinning test (a Guard's snapshot survives two
concurrent replacements whole and frees on guard drop, on both debt
outcomes). Compile-time pins: `assert_impl_all!(ClusterSnapshot:
Send, Sync)` — what makes T5.3's fan-out job legal — and Catalog
Send+Sync. Surprise: arc-swap's Guard IS `Send` in 1.9.x (it holds
`ManuallyDrop<Arc<T>>` plus a shared debt reference, not the raw
pointer of older versions) — the docs state the honest debt-pressure
reason, not a fake `!Send`. Also in this commit: pre-existing clippy
debt the old exit-code-only gate had let through — an
`async_yields_async` ERROR in tests/submit.rs (denied by clippy 1.95;
the unresolved `first` future now drops inside the block),
walgate's unused import and detached-task naming, and cascade-wal's
doc-divider comment plus a `&PathBuf`→`&Path` in its writer test.

**Tier C writer + bounded fan-out + boot publication order (T5.3,
FR-6/FR-29/FR-30, §10.1 step 7, R11) — one writer, one budget, empty
until published.** `src/tierc.rs` grew the serialized writer:
`ClusterState` (the ONE mutable copy — applies changes in place,
publishes immutable clones whole) is owned by `writer_loop`, the one
task on the serving housekeeper's ring. The fan-in is per-worker write
channels (`TIERC_WRITE_CAP` 16, eager `try_send` — a full channel
refuses up-front instead of hiding pressure in a queue) whose job
futures, running on the housekeeper ring via the T5.1 forwarders,
reach `submit_change` → the `FUNNEL` thread_local (a same-ring
`LocalSender<WriteCmd>`; `None` on any ring without a live writer is
an honest `SubmitError::Lost`, never a hang). The fan-out
(`fan_out`) submits one whole-snapshot store job per reactor with the
ISSUING reactor swapped to the front and awaits under ONE shared
deadline (`Instant::now() + tierc_fanout_wait_ms` taken BEFORE
submission — the bound covers the whole fan-out wall time, the
strictest reading of R11's "bounded wait"; each await races the
remaining budget against the submit via a hand-rolled `Race` future
because glommio's `timer::timeout` only fits ITS `Result` type, and
the submit is polled first so a confirm+deadline same-wake counts as
confirmed). The ack is `Applied` or `Pending(n)`; pending is never
lost — `submit_to` admitted the store job eagerly, so it sits in the
laggard's publication channel (`TIERC_PUBLISH_CAP` 64) and lands when
that ring drains (lazy adoption on its next pointer load); dropping
the awaited future at the deadline abandons only the ack, not the
job. Boot-order (FR-6, §10.1 step 7 → 9): boot BUILDS the initial
snapshot (the WAL-home resolve reads it) but publishes NOTHING —
every reactor starts `Catalog::empty()`; `start_writer` runs inside
`Router::serve` BEFORE the accept loop opens, publishes own-catalog
first then fans out, and the `published` oneshot gates the loop;
workers spawned early gate each handoff through `gate_handoff` — an
unpublished catalog answers wire `57P03` ("the database system is
starting up") and closes the fd exactly once (the same single
adoption `session::run` would make). The router's `None` arm is the
same refusal, defensive (publication precedes its loop). Tests: the
happy path (two workers issue through real write channels; `Applied`
ands the row visible on all THREE catalogs, other names inherit the
default), the stalled reactor (instrumented gate on the publication
loop, NOT a sleep guess: `Pending(1)` with a ≥350 ms lower-bound
wait, issuer's catalog serves at ack time, opening the gate IS the
lazy adoption, post-stall change acks `Applied`), the issuer-first
DISCRIMINATOR (issuer on the unstalled worker, laggard gated:
`Pending(1)` not `Pending(2)` — natural-order awaiting would burn
the budget on the non-issuer), and the 57P03 gate over a real TCP
pair both ways. Review findings: the fixture raced its own fan-out —
it asserted worker publications synchronously after waiting only on
the housekeeper's (a ~1-in-5 full-suite flake, caught by loop
re-runs); the fix polls each ungated catalog. `Catalog` and
`ClusterSnapshot` carry `Send`/`Sync` static asserts (what makes the
fan-out job legal — pinned at T5.2).

**Affinity DDL surface (T5.5, FR-10/FR-12/FR-38, §2.5.1) — created rows
decide routing; ALTER is validated, then honestly refused.**
`CREATE DATABASE … [WITH numa_affinity = 'single'|'cross']` rides the
T5.4 bounce exactly: the parser lowers PG's real option form (NO
parentheses — the raw grammar refuses the parenthesized list; the
DefElem list stays generic, so unmodeled options like `OWNER` are
refused 0A000 upstream rather than silently dropped), `sql.rs`
validates the value (22023 for garbage — the parameter is known —
BEFORE the feature question), and the session bounces
`TierCChange::CreateDatabase` through the write channel; the writer
refuses duplicates 42P04 with zero mutation (validate before first
mutate, like CreateRole). The bare form freezes the cluster default
INTO the row at creation (PG-faithful: inheritance is a creation-time
decision); `Single` binds per A13's least-loaded proxy at apply.
Routing needed NO code: the router already resolves placement from the
live snapshot (T2.4), so the created row decides the next connection's
candidates — single → bound NUMA's workers only, 53300 naming the NUMA
when it is drained, now driven by a CREATED row rather than only boot
config. `ALTER DATABASE … SET numa_affinity` is validated then refused
0A000 — the migration runner is Phase 7+ (§2.5.4), and pretending to
migrate (re-homing the DB's whole Tier B set) would be the lie;
nothing bounces, no snapshot changes. The observable is
`SELECT debug_databases()` — this reactor's snapshot rows (name,
affinity spelling, bound NUMA, `-` when unbound), a pointer load like
debug_roles; the FR-36 probe extends to it: reads submit zero, each
DDL exactly one. Tests: `tests/db_affinity.rs` — created-single routes
only to its bound NUMA and refuses at cap; rows visible on another
reactor; ALTER validated-then-refused leaving rows AND routing
untouched; duplicate + txn-block refusals. The fixture boots CROSS on
purpose so the created ROW is the discriminator, not the cluster
default.

**Load view completion (T5.6, FR-11, R9 phase two, §5.2) — the composite
is lexicographic, and the EWMA lives on the reactor.** Routing's pick
is now ONE ordering key in the spec's listed order: connection count
primary → the reactor's CPU-utilization EWMA (decides only at equal
counts) → NIC proximity (decides only at equal counts AND equal EWMA,
`cross` DBs only — a `single` candidate set is already one NUMA, so
proximity holds at 0 across it; `None` NIC discovery is inert the same
way). Strict `<` keeps first-route-wins on full ties — the T2.4
determinism survives the composite. The CPU half is
`src/cpuload.rs`: each reactor spawns a sibling sampler task (next to
control/recycle/drain/reap) that reads ITS OWN thread's
`getrusage(RUSAGE_THREAD)` every `load_sample_ms` (default 1 000 ms,
A2; `0` disables), computes `ewma ← (ewma + sample) / 2` in integer
milli-units (α = 0.5; the FIRST sample seeds rather than halves in
from 0), and publishes into `WorkerCounters.cpu_ewma_milli` — the
spec's "lightweight per-reactor atomic counter polled by
housekeeper". **The EWMA is computed at the sampler, never on the
housekeeper** — a self-observation is synchronous with its sample, so
the review bullet's stale-samples-in-transit shape has no code to
fail; the router only ever Relaxed-loads the latest milli-value (same
as the connection gauge it already read). NIC proximity anchors on
`nic_numa` (config override; else sysfs
`/sys/class/net/<if>/device/numa_node`, first sorted interface with a
real node ≥ 0, `-1` = unknown → skipped; loopback-only rigs discover
nothing). `pick()` is extracted pure (Candidate view of Route) so the
full ordering matrix is unit-tested EXACT — the NIC-decisive cases
construct literal equality in the leading components; integration
(`tests/loadview.rs`, NIC pinned to NUMA 1 against the 2+1 layout)
drives the real samplers: an all-zero boot's first connection lands
nic-local (the exact tie asserted from the same counters the router
reads), count still outranks proximity, and a hammered worker is
rejected at a count tie (its EWMA strictly-greater-with-margin —
deliberately NOT a tie — while the idle workers publish exact 0).

**Post-M5 review fix 1 — the handshake is bounded in time (H1).** The
holistic adversary review found the accepted handshake had NO time
bound anywhere: the listener's `SO_RCVTIMEO` does not propagate to
accepted sockets (Linux), so one silent client parked `pg::handshake`
— and with it the whole serving housekeeper (accepts, `poll_due`
ticks, the Tier C writer, the shutdown flag check) — indefinitely.
Fix: `handshake_timeout_ms` (default 60 000, PG's
`authentication_timeout` analog; validation caps at one hour so the
`Instant` arithmetic cannot overflow; 0 = documented off switch),
enforced self-containedly INSIDE `handshake` — `SO_RCVTIMEO` on the
accepted socket bounds any single blocked read, a total deadline
(`Deadline` read-wrapper, checked before every read) bounds a
byte-trickling peer. The socket timeout is CLEARED on success: an
idle session is legal (idle policy belongs to the reaper), the
deadline was the handshake's, not the connection's. Expiry maps to
`HandshakeError::TimedOut` via `io_closed` (WouldBlock|TimedOut =
our clock, never the peer's) and nothing is sent — writing to a peer
stalled this hard could itself block. The internal pg fns went
generic over Read/Write so the wrapper threads through unchanged.
Tests (tests/auth.rs): silent client times out bounded; a dribbled
valid startup (9 bytes × 50 ms vs a 300 ms budget) hits the TOTAL
deadline while every per-read gap sits inside `SO_RCVTIMEO`; and the
end-to-end discriminator boots a real runtime (budget 300 ms) where
one stalled handshake cannot stop another client's INSERT, and the
stalled connection is cut, not held.

**Post-M5 review fix 2 — ticks poll on every pass, not on timeout
(M1).** The same review caught the tick poll living ONLY in the
accept-timeout arm while the quantum comment above it claimed "on
every return": under sustained connection churn — a backlog that
never drains — no pass ever times out, and every tick job suspends
precisely while the server is busiest. `orchestrator.poll_due()` now
runs at the top of every serving pass, before `until_next` reads the
park (due jobs fire first, so the park never waits on a passed
deadline); the redundant call in the timeout arm is gone, and the
Interrupted/ConnectionAborted arms — which also skipped the poll —
are covered by the same move. `tests/tick.rs` pins the system
property: with two dozen silent sockets burying the accept loop at a
200 ms handshake budget, ticks still advance (≥ 4 in 1.5 s at a
50 ms interval), the churn is provably engaged (the first stall
socket is cut by the budget, not left hanging), and wakeups stay
handshake-paced, not idle-paced.

## Open Design Questions

See the spec's §11. Highlights:
- Specific Glommio version and any required forks.
- CPU topology discovery: `hwloc-rs` vs hand-rolled `/sys` parser.
- Reactor stall plumbing details (sampler thread vs in-band).
- Whether `submit_to` is public API to higher crates or restricted.
