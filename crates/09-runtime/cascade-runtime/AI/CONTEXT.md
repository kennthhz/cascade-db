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
`0` = disabled: no channels, no job, no loops, and sessions take the
plain read (`reap == None`); a default that kills client connections
must be opted into. **Connect-order rule (found as a live three-way
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
external crate can write at all. Until T5.3's housekeeper writer lands,
boot's `Catalog::new` is the only publication — the snapshot stays
immutable after boot, the T2.4 posture. Consumers: router `route()`
pointer-loads per connection; boot's WAL-home resolve reads once
pre-serving. Per-reactor instances are T5.3's fan-out shape (R11's
laggard adoption needs one ArcSwap per reactor); today ONE instance,
held by the router plus boot's one resolve. The three §8.4 guarantees
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

## Open Design Questions

See the spec's §11. Highlights:
- Specific Glommio version and any required forks.
- CPU topology discovery: `hwloc-rs` vs hand-rolled `/sys` parser.
- Reactor stall plumbing details (sampler thread vs in-band).
- Whether `submit_to` is public API to higher crates or restricted.
