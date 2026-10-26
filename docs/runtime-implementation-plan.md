# Cascade DB Runtime — Implementation Design & Task Plan

Status: Accepted v2 · 2026-10-24 (v2 = adversarial review pass: closed spec-mismatch
findings FR-catalog gaps; split per-task reviews into code + test review)
Source of truth: [runtime-architecture.md](runtime-architecture.md) (the spec).
This doc turns the spec into build order. It adds nothing to the design; where the
spec is silent or ambiguous, the gap is recorded in §8 and ruled by a decision.

**Spec-match statement:** every normative requirement in spec §2–§10 and every
invariant I1–I13 maps to at least one FR below; every FR maps to at least one task
and one test. Requirements the slice deliberately does not build are listed in §9
(Deferred — not gaps). An earlier draft (v1) missed multiple spec requirements —
including TenantContext (I5), `&Span` (I6), kernel-cmdline verification, boot-time
WAL replay, the idle-connection reaper, Tier C boot publication, the composite load
view, and the affinity DDL surface — and misstated two (the `submit_to` signature
and the stall kill switch); all fixed in v2.

---

## 1. Decisions (R1–R14; R9/R12 amended by the v2 review to match the spec)

| # | Decision |
|---|---|
| R1 | **Vertical slice, milestone-ordered** — runnable end-to-end each milestone: M1 single-core slice → M2 topology/NUMA → M3 durability hardening → M4 background+shutdown → M5 Tier C + multi-reactor + bare-metal gate. |
| R2 | **Wire: simple-query only.** No extended protocol, no pipelining. Auth: SCRAM-SHA-256; trust mode allowed in dev config. |
| R3 | **Glommio 0.9.x, pinned, no forks.** Cross-executor `RawFd` adoption is the version-pinning requirement (spec §5.1, §11). |
| R4 | **Topology discovery: hand-rolled `/sys` parser** behind a `TopologyProvider` trait (spec offers hwloc-preferred//sys-fallback; §11 leaves it open — this resolves it). No C deps. |
| R5 | **Housekeeper = plain `std::thread` + `std::net`** — CFS-scheduled, interrupt-driven by construction (spec §2.1). |
| R6 | **Core placement is explicit config:** `housekeeper_cores` (default 1, §2.1) and `worker_cores` per NUMA. Empty/unpinnable lists refuse boot. |
| R7 | **Topology testing:** all topology behavior behind `TopologyProvider`; CI tests synthetic topologies; `#[ignore]` bare-metal suite for real pinning/NUMA (run manually, due at the M5 gate). |
| R8 | **Adversary review before every commit — two passes:** code review (§5 checklist) *and* test review (§5.1), each recorded in the commit body. |
| R9 | **Load metric is the spec's §5.2 composite** — active connection count + sampled CPU utilization (EWMA) + NIC proximity as tie-breaker (cross-affinity only) — **delivered in two phases:** connection count alone routes from M2; CPU sampling + tie-breaker land in M5 (T5.6). Until then the plan deviates from the spec *by recorded phasing*, not by omission. *(v2 amendment: v1 ruled conn-count only, which contradicts §5.2.)* |
| R10 | **Affinity refusal** = PG error `53300` (too_many_connections), message names the bound NUMA. Never silently route cross-NUMA (spec §5.2 "clear error, no silent fallback"; §11 rules v1 = refuse). |
| R11 | **Tier C fan-out: bounded wait (1 s)** for all reactors to store; ack reports *applied everywhere* or *pending N*; laggards adopt lazily. The issuing reactor stores first, so the requesting session always sees its own change. (Spec §8.4 shows full fan-out; this adds the bounded-wait safety valve — flagged as A4.) |
| R12 | **Stall actions exactly per §7.2:** soft (1 ms) / hard (10 ms) counters + INFO/WARN logs, published via cascade-telemetry; **kill switch exists, configurable, default off** — no task is killed under default v1 configuration. *(v2 amendment: v1 said "no killing in v1" full stop, contradicting the spec's configurable kill row.)* |
| R13 | **Faster spec-first backfill:** every plan decision that touches spec text is written into the spec in T0.1 before the code that depends on it. |
| R14 | **`submit_to` semantics are the spec's §6.1 signature, verbatim:** the *closure* is `FnOnce() -> Fut + Send` (captured state crosses reactors, so it must be `Send`); the *future is constructed on the target*, so state created inside it may be `!Send`; inputs/outputs are `Send`. *(v2 fix: v1's FR said "captured state may be !Send" — wrong, and would have shipped an unusable API.)* |

## 2. How tasks are written and committed

Every task has: **Goal** · **Do** · **FRs** (from §3, always explicit) · **Tests** ·
**Code review** (adversarial focus for this diff, plus the standing checklist §5) ·
**Test review** (adversarial review *of the tests themselves*, per §5.1).

Commit protocol (per task):
1. Implement → tests green.
2. Run the §5 code-review checklist against the full diff; fix findings (they block).
3. Run the §5.1 test review on the new/changed tests; fix gaps (they block).
4. Commit message body ends with:
   `Task: <id> · FRs: <ids> · Code review: clean | <findings> · Test review: clean | <findings>`
5. Direct commit to `main` (repo convention). One commit per task; split only if genuinely two changes.

## 3. Functional requirements (FR-1 … FR-38)

One sentence each, plain language, spec anchor attached. "In" = invariant I1–I13.

### Boot & topology
- **FR-1** — The process boots exactly as configured: `housekeeper_cores` housekeeper threads (default 1, §2.1) and the `worker_cores` workers per NUMA; an empty, invalid, or unpinnable list refuses boot. (§2.1, §10.1, R6)
- **FR-2** — Every worker and housekeeper thread is pinned with `sched_setaffinity` and pinning is verified after it happens. (§2.2, §3.1)
- **FR-3** — The worker set follows the SMT mode: `physical` (default) = one worker per *physical* core with HT siblings excluded from every role (a config assigning both siblings refuses boot); `logical` = one worker per vCPU. (§2.2, §2.4)
- **FR-4** — At startup the runtime verifies the actual kernel CPU isolation (isolcpus / nohz_full / rcu_nocbs / irqaffinity) against the configured sets and logs a **loud warning** on mismatch — warning, not refusal (§2.4 says the lists are provisioning-time; the runtime can only verify). (§2.4, §10.1 step 3)
- **FR-5** — Housekeeper loops (accept, background ticks) sleep between events — no busy-wait — and the listening socket is a single socket on the housekeeper; `SO_REUSEPORT` accept distribution is not used. (§2.1, §5.1)
- **FR-6** — Tier C is loaded from the on-disk catalog and published to every reactor **before any worker serves traffic**. (§10.1 step 7 + boot-ordering note)
- **FR-7** — Boot detects an unclean shutdown (missing clean marker / WAL past clean LSN) and runs per-DB WAL replay to the clean LSN **before** the listening socket binds; the runtime orchestrates, storage implements. (§10.3, §10.1 step 8d)
- **FR-8** — Graceful shutdown: stop accepting → drain in-flight transactions up to `drain_deadline` (default 30 s), refusing new statements with a clear error → force-abort the rest → release locks/pins/undo refs → final per-DB WAL flush → final BPM flush across pools advancing per-DB checkpoint LSN → drop reactors, close rings and files. (§10.2)

### Connection path
- **FR-9** — Authentication completes before routing: SCRAM-SHA-256, with trust mode allowed in dev config. (§5.1, R2)
- **FR-10** — Routing reads `DatabaseId → numa_affinity` from the Tier C snapshot — `single` DBs consider only their bound NUMA's workers, `cross` DBs all workers — inheriting the cluster default (`default_db_numa_affinity`) for DBs that don't specify. (§5.2, §2.5.1)
- **FR-11** — "Load" is the spec's composite: active connection count + recent CPU utilization (sampled, exponentially weighted) + NUMA proximity to the NIC as tie-breaker (cross-affinity only), computed from load samples each reactor publishes for the housekeeper. Delivered per R9's two phases. (§5.2)
- **FR-12** — A `single`-affinity DB with no available worker on its bound NUMA is refused with PG `53300` naming the NUMA — never silently crossed. (§5.2, R10)
- **FR-13** — The connection crosses to the worker as a bare `RawFd` (Copy value); the worker adopts it onto its own ring; no file handle or ring is ever shared across reactors, and no handle escapes `cascade-storage`/`cascade-wal`. (§5.1, §8.1, §8.2, I2)
- **FR-14** — Connection state is Tier A: `!Send`, owned by exactly one reactor, pinned for the life of the connection. (§5.3, I1)
- **FR-15** — Disconnect rolls back the in-flight transaction, releases page pins, drops session statements, frees the Tier A arena memory, and decrements the reactor's `active_connections` so the housekeeper's load view drops. (§5.4)

### Execution & observability
- **FR-16** — The slice serves the simple-query protocol path: startup → query → vectorized result batches (default 1 024 rows, configurable per operator) → ready. (§5.1, §7.1, R2)
- **FR-17** — `yield_point()` runs at every batch boundary and yields when CPU time since the last yield exceeds the soft deadline (default 500 µs) or the local run queue is non-empty; below the deadline with an empty queue it is a no-op. (§7.1, D7)
- **FR-18** — Long-running operators call `yield_point()` at internal sub-boundaries (the runtime exposes the contract; a test operator exercises it). (§7.3, I10)
- **FR-19** — The stall detector tracks the longest gap between yields per reactor: soft ≥ 1 ms increments `reactor_stall_soft_total` + INFO log, hard ≥ 10 ms increments `reactor_stall_hard_total` + WARN log, published via `cascade-telemetry`; the kill switch is configurable and default-off. (§7.2, D8, R12)
- **FR-20** — Every hot-path operation carries a `TenantContext` (from `cascade-tenant`) end to end. (I5, README §6.1)
- **FR-21** — Every hot-path operation accepts a `&Span` (observability reservation) end to end. (I6, README §6.2)

### Data path (slice scope)
- **FR-22** — One BPM pool per NUMA, sized per NUMA by config: a `single` DB uses its bound pool; a `cross` DB maps each page via `hash(PageId) mod num_numas` (deterministic, single direct lookup, no miss-probing). (§8.1, §2.5.1)
- **FR-23** — Page access: hit → pin the frame (atomic pin count + dirty bit, `PageGuard` RAII — a pin survives no scope, including panics); miss → read through *this* reactor's own ring on its own file handle, insert, pin. (§8.1)
- **FR-24** — Eviction happens in-pool, sharded `hash(PageId) mod cores_on_this_numa`, write decisions on the shard-home core; dirty eviction is gated on WAL durability — no dirty page is written before its WAL is durable. (§8.1, I7)

### Durability & recovery
- **FR-25** — WAL append reserves an LSN before the record exists; flush eligibility advances only to the highest *fully written* record (filled watermark), never the reserved watermark. (§8.2)
- **FR-26** — `flush_wal` elects one leader via atomic CAS; the leader `fdatasync`s on its own ring through its own handle — per-inode durability covers records appended through every reactor's handles — and waiters are notified exactly when the durable LSN covers theirs. (§8.2)
- **FR-27** — One WAL writer per database, homed per affinity (bound NUMA for `single`; one designated NUMA, persisted, for `cross`); a commit acks only after durability reaches the filled watermark. (§8.2)

### Tier C
- **FR-28** — Every reactor serves metadata from its local `ArcSwap<ClusterSnapshot>`; a read is `load()` — one pointer copy, no lock, no compare-exchange. (§6.3, I9)
- **FR-29** — Metadata mutation has one path: worker `submit_to`s the housekeeper → the single serialized writer applies it → builds an immutable new snapshot → `submit_to` every reactor for `ArcSwap::store`. (§6.4, §8.4)
- **FR-30** — The writer waits at most `tierc_fanout_wait_ms` (default 1 s) for stores; the ack says *applied everywhere* or *pending N*; laggards adopt lazily; the issuing reactor stores first. (R11)

### Background
- **FR-31** — The checkpointer orchestrates only: per NUMA pool it `submit_to`s a reactor on that pool's NUMA to iterate dirty pages and fan flush I/O to shard-home cores, advancing per-DB WAL-truncatable LSN — never touching another NUMA's Tier B state directly. (§9)
- **FR-32** — The WAL recycler `submit_to`s the reactor owning the DB's WAL home to reclaim segments below the per-DB truncatable LSN. (§9)
- **FR-33** — The statistics collector aggregates per-core counters (summed on read, §4) for `pg_stat_database`-style views. (§9)
- **FR-34** — The idle-connection reaper times out idle connections and signals the owning reactor to drop them. (§9)

### Cross-reactor primitive & DDL surface
- **FR-35** — `submit_to(target, f)` takes `f: FnOnce() -> Fut + Send + 'static` with `Fut::Output: Send`; the closure runs on the target reactor with the future constructed *there* (so state created inside the future may be `!Send`), and the result is delivered to the caller. (§6.1, R14)
- **FR-36** — `submit_to` is used only for rare coordination — Tier C writes, background work, cross-core observability queries — never on the OLTP query hot path. (§6.1, I12)
- **FR-37** — Per-reactor working memory is allocated from a NUMA-local arena (allocator mechanism is the runtime ADR, spec names mimalloc-style thread-local heaps as the example); verified on bare metal. (§2.3, I4a)
- **FR-38** — The affinity DDL surface exists and is honest: `CREATE DATABASE … WITH (numa_affinity = …)` records `datnumaffinity` (binding chosen at first activation for `single`); `ALTER DATABASE … SET numa_affinity` parses and validates but returns `unsupported_feature` until the Phase 7+ migration runner. (§2.5.1, §2.5.4, §11)

## 4. Milestones & tasks

### M0 — Foundations

**T0.1 Backfill the rulings into the spec.**
- Goal: spec and plan say the same thing before code depends on either.
- Do: write into the spec — R9's load phasing + A2 sampling defaults (§5.2 note), R11's bounded fan-out (§6.4/§8.4 note), A6 ring-queue-depth/readahead defaults (§3.3), A7 per-worker connection cap (§5.2), A13 bound-NUMA selection default (§11), A14 cross-WAL-home column decision path (§11), A15 stall-plumbing choice (§9/§11), A18 more-NUMA-nodes-than-workers validation (§11); close the §11 rows these resolve.
- FRs: FR-11, FR-12, FR-30, FR-38 (the rulings these depend on).
- Tests: spec-consistency sweep — grep `crates/**/AI/CONTEXT.md` and crate READMEs for drifted mirrors (standing rule).
- Code review: any spec sentence still contradicting a decision?
- Test review: n/a (docs-only commit — recorded as such).

**T0.2 Runtime crate skeleton + full config surface.**
- Goal: `cascade-runtime` builds with a validated `RuntimeConfig` covering every knob the FRs need.
- Do: TOML keys — `topology.mode` (physical default), `housekeeper_cores` (1), `worker_cores` per NUMA, `default_db_numa_affinity` (single), `bpm_size` per NUMA, `yield_deadline_us` (500), `batch_rows` (1024), `drain_deadline_s` (30), `max_conns_per_worker` (200), `tierc_fanout_wait_ms` (1000), stall thresholds (1000/10000 µs) + `stall_kill` (off), listen addr/port. Validation: empty/unpinnable lists are config errors (boot refusal lands in T1.1).
- FRs: FR-1, FR-3, FR-22 (sizing), FR-19 (thresholds) — config surface for each.
- Tests: parse valid/invalid configs; every default asserted; unknown-key rejection.
- Code review: silently-accepted nonsense (0 workers, negative deadlines, overlapping NUMA lists, stall_kill on without telemetry).
- Test review: does the invalid-config list cover *every* validation rule added, one negative test each?

### M1 — Single-core vertical slice (one worker, one NUMA)

**T1.0 Spike: pin glommio; verify the parser builds.**
- Goal: de-risk the two external dependencies before anything depends on them.
- Do: pin glommio 0.9.x (R3), confirm fd-adoption API exists (the §11 version-pinning requirement); try building `cascade-pg-parser` — fallback hand-rolled micro-parser recorded in A1 if it fails.
- FRs: FR-13, FR-16 (tooling prerequisites).
- Tests: hello-executor — spawn one reactor, run one task.
- Code review: glommio API surprises vs spec assumptions (yield semantics, fd adoption, pin API).
- Test review: does the spike test pin the *pinned* reactor or just run a task?

**T1.1 Boot one pinned worker + one housekeeper thread.**
- Goal: the process starts in the spec's shape, trivially small.
- Do: housekeeper `std::thread` (R5); one glommio worker pinned via `TopologyProvider` (synthetic in CI); pinning verified with `sched_getaffinity` after `sched_setaffinity`; boot fails on pin failure or empty core list.
- FRs: FR-1, FR-2.
- Tests: synthetic-topology boot; verification passes on the right core; refusal on bad config; `#[ignore]` bare-metal pin test.
- Code review: unpinned fallback paths; verification that reports success without reading back.
- Test review: would the tests catch a worker silently unpinned by a bad `TopologyProvider`? (mutation check: flip the core id).

**T1.2 `/sys` topology parser.**
- Goal: real sibling/NUMA discovery without C deps.
- Do: parse `/sys/devices/system/cpu/*/topology` (thread_siblings lists, package/NUMA maps) behind `TopologyProvider`; fixture trees for CI.
- FRs: FR-3 (data source for worker-set computation).
- Tests: fixture layouts — single core, SMT pairs, 2-NUMA, non-contiguous physical-mode sets; malformed/missing entries; offline CPUs.
- Code review: kernel-version format variation; sibling lists that are asymmetric or comma-formatted.
- Test review: are fixtures drawn from *real* `/sys` dumps (at least two machines), not idealized ones?

**T1.3 Housekeeper accept loop.**
- Goal: a client can TCP-connect.
- Do: `std::net` bind/listen/accept on a housekeeper thread; accepted fd goes to the router; single socket, no `SO_REUSEPORT`; sleeps between events.
- FRs: FR-5.
- Tests: connect/disconnect from a test client; fd observed by router; idle-wakeup assertion (wakeup count flat with no clients); socket-options assertion that `SO_REUSEPORT` is *not* set.
- Code review: fd leaks on early client drop; backlog-overflow behavior; accidental `SO_REUSEPORT`.
- Test review: is the idle-wakeup assertion robust to scheduler noise, or flaky in CI?

**T1.4 Auth: trust + SCRAM-SHA-256.**
- Goal: PG handshake completes with real auth.
- Do: startup packet parse; trust (dev) and SCRAM-SHA-256; verifier source per A11 (config-file users in the slice); PG error codes on failure.
- FRs: FR-9.
- Tests: integration against the `postgres` Rust client — trust and SCRAM; wrong-password rejection; error-code assertion.
- Code review: partial packets, mid-handshake disconnects, protocol-version mismatches, auth-bypass paths around the trust mode.
- Test review: does any test rely on timing (sleep-based) that could flake? Are rejection paths asserted by error code, not just "errored"?

**T1.5 Router v0 + RawFd handoff.**
- Goal: the connection lands on the worker as a bare fd and is adopted there.
- Do: route by connection count (single NUMA; affinity lands in M2/M5); send `RawFd` over a channel; worker adopts onto its own ring (R3's pinning requirement); housekeeper never touches it again; `active_connections` counter incremented on the worker, visible to the housekeeper.
- FRs: FR-11 (count component), FR-13, FR-15 (counter part) — FR-10/FR-12 arrive in M2/M5, flagged partial.
- Tests: handoff under the real executor; worker-side echo; adoption asserted on the worker's ring (thread-id + fd-ownership checks); counter visible to housekeeper.
- Code review: double-close of the fd; adoption on the wrong executor (the §5.1 bug class); counter races.
- Test review: would the adoption test fail if adoption happened on the housekeeper thread? (mutation check).

**T1.6 TenantContext + Span plumbing (before any query code — no retrofit).**
- Goal: the two cross-cutting hot-path requirements exist from the first query, not bolted on after.
- Do: every hot-path function signature in the slice takes `&TenantContext` (from `cascade-tenant`) and `&Span`; a default dev tenant/span for the debug engine; counters tagged per tenant.
- FRs: FR-20, FR-21.
- Tests: compile-shape assertion (a query-path call without context does not type-check — signature is the test); runtime propagation check (tenant id visible in emitted spans/counters).
- Code review: any hot-path fn missing the parameters; context cloned where a borrow would do.
- Test review: is the propagation test asserting *identity* of the context, not just non-null?

**T1.7 Tier A session + simple-query loop.**
- Goal: startup → query → results → ready, on the worker — carrying T1.6's signatures from birth.
- Do: `!Send` session (`Rc` internals so `Send` can't happen by accident — FR-14 compile-enforced); debug SQL subset per A1 (single fixed-row table: `SELECT` projection, `INSERT`, `BEGIN/COMMIT`); vectorized batches (`batch_rows` default 1024).
- FRs: FR-14, FR-16 (signatures per FR-20/FR-21).
- Tests: `postgres`-client integration for the subset; compile-time `!Send` assertion; batch framing vs `batch_rows`.
- Code review: any accidental `Send`/`Sync` bound; session state escaping the reactor.
- Test review: does the `!Send` assertion actually fail compilation when the type changes, or is it a no-op?

**T1.8 `yield_point()` + stall detector.**
- Goal: the cooperative-yield contract and the stall telemetry, measurable.
- Do: deadline-aware `yield_point()` (monotonic clock; yield on deadline *or* non-empty queue; no-op otherwise — §7.1's exact three cases); stall detector with soft/hard counters named `reactor_stall_soft_total`/`reactor_stall_hard_total`, INFO/WARN logs, published via `cascade-telemetry`; kill switch present but default-off (R12).
- FRs: FR-17, FR-18, FR-19.
- Tests: µs-shrunk deadline unit tests covering all three yield cases; test-operator sub-boundary yield (FR-18); planted slow task → counter + log level asserted; kill switch default-off asserted.
- Code review: clock wrap/monotonicity; a yield that never parks; stall sampling perturbing the reactor it samples.
- Test review: are the three yield cases each covered by a test that fails if that branch is removed? Timing-based assertions bounded so CI can't flake?

**T1.9 Debug storage: PageStore file + single BPM pool + PageGuard.**
- Goal: pages flow — hit → pin; miss → own-ring read → insert → pin.
- Do: debug file-per-table in `cascade-storage`; one BPM pool (single-NUMA case of FR-22) with atomic pin count + dirty bit; RAII `PageGuard` (release on panic, tested via `catch_unwind`).
- FRs: FR-22 (single-pool case), FR-23.
- Tests: hit/miss/pin/unpin; panic-during-pin releases; concurrent lookups from one reactor; handle never escapes the crate (I2 — compile-shape).
- Code review: pin leaks (guard forgotten); pin-count under/overflow; lookup vs insert races *within* one reactor.
- Test review: is the panic test asserting the pin count *after* unwinding? Does any test depend on eviction (deferred to M3) implicitly?

**T1.10 WAL writer: reserve/fill/filled watermark/leader flush.**
- Goal: the durability core, single-DB.
- Do: `append_wal` reserve→fill; flush eligibility stops at the filled watermark (wal invariant 5); `flush_wal` leader CAS → `fdatasync` on this reactor's own ring → notify waiters; records carry full row images (I8) even in the debug format.
- FRs: FR-25, FR-26, FR-27 (single-DB case).
- Tests: reserve-without-fill never advances flush (the torn-record test — including a panic between reserve and fill); leader election under concurrent flushers; durable-LSN notification ordering; CRC catches a torn record on reopen.
- Code review: panic between reserve and fill; two leaders; lost wakeup (waiter hangs); any path advancing flush past unfilled reservations.
- Test review: does the torn-record test plant the torn record *deliberately* and assert the watermark stops, or does it just test the happy path around it?

**T1.11 COMMIT end-to-end + disconnect cleanup.**
- Goal: an acked commit is really durable; a dropped connection leaves nothing behind.
- Do: COMMIT → append → flush → ack (FR-27 order); disconnect path aborts txn, releases pins, drops statements, frees arena, decrements `active_connections`.
- FRs: FR-15, FR-27.
- Tests: kill-after-ack replay shows the txn; disconnect mid-txn aborts; pin/guard counts return to zero; `active_connections` returns to baseline.
- Code review: ack before durable (the lying-ack bug); cleanup ordering (pins freed before WAL durable? counter decremented before cleanup finishes, skewing routing?).
- Test review: does the replay test read the WAL through the recovery path (not by parsing files in the test)?

### M2 — Topology & NUMA affinity

**T2.1 Multi-worker boot.**
- Goal: W workers across configured cores, each pinned and verified.
- FRs: FR-1, FR-2.
- Tests: multi-worker synthetic boot; per-worker affinity verification; one-worker-per-core uniqueness.
- Code review: core-list off-by-one; two workers on one core; worker spawned before its pool exists (boot-order vs §10.1).
- Test review: does uniqueness checking use the *verified* affinity, not the config intent?

**T2.2 SMT mode worker sets + sibling exclusion.**
- Goal: `physical` mode never assigns an HT sibling of a worker to any role and refuses sibling-collision configs; `logical` mode assigns one worker per vCPU.
- FRs: FR-3.
- Tests: synthetic SMT fixtures — exclusion + refusal (physical), full-vCPU sets (logical); non-contiguous worker sets accepted.
- Code review: sibling maps parsed inverted; asymmetric sibling lists; mode string typo falling through to a default.
- Test review: is the refusal test asserting *before* any thread spawns (no half-booted state)?

**T2.3 Per-NUMA pools + NUMA-local arena.**
- Goal: one BPM pool per NUMA sized per config, allocated on its node; per-reactor arenas NUMA-local.
- FRs: FR-22 (placement + sizing), FR-37.
- Tests: pool-node mapping from synthetic topology; per-NUMA sizes honored; bare-metal `#[ignore]` node-locality check (numa_maps); edge: more NUMA nodes than worker cores (§11) — validated, not crashed.
- Code review: first-touch by the booting thread (wrong node); pool registry built before topology resolved.
- Test review: does the CI (single-NUMA) test still exercise the *mapping* logic, with only the locality assertion relegated to bare metal?

**T2.4 Affinity routing + refusal.**
- Goal: the §5.2 policy, for real.
- Do: `single` → least-loaded within bound NUMA, else PG `53300` naming the NUMA (R10); `cross` → machine-wide; cap = `max_conns_per_worker` (A7); cluster default inheritance.
- FRs: FR-10, FR-11 (count phase), FR-12.
- Tests: both affinities; refusal via `postgres` client asserting error code + message; cap boundary; cluster-default inheritance for a DB with no explicit affinity.
- Code review: race between count-check and handoff (admitted after refusal decision); affinity read from boot config instead of live snapshot (until T5.5, snapshot is static — assert it's read *from the snapshot*, not config).
- Test review: does the inheritance test distinguish "default applied" from "value happened to match default"?

**T2.5 Cross placement in the lookup path.**
- Goal: `hash(PageId) mod num_numas` in the real lookup — single direct lookup, no miss-probing.
- FRs: FR-22.
- Tests: distribution over synthetic PageIds; cross-DB page lands in the hashed pool from any reactor; determinism (same PageId → same pool across restarts).
- Code review: hash computed on the wrong key fields (I3: `PageId` = `(DatabaseId, SegmentId, PageNo)`, never a file path).
- Test review: is determinism tested across process restarts, not just repeated calls?

**T2.6 WAL home-NUMA placement.**
- Goal: per-DB writers homed per affinity (single → bound NUMA; cross → designated NUMA persisted per A14).
- FRs: FR-27.
- Tests: homing table after DB creation; append routes to the home writer; persistence of the home choice.
- Code review: cross-DB writer accidentally created per-reactor (multi-home bug); home choice not persisted (reboot re-picks).
- Test review: does the persistence test survive a simulated restart (re-read from catalog), not just in-memory state?

**T2.7 Kernel-deployment verification (warn-only).**
- Goal: boot verifies actual CPU isolation vs config and warns loudly (§2.4) — never refuses.
- FRs: FR-4.
- Tests: synthetic mismatch → warning asserted (log capture); match → no warning; `#[ignore]` bare-metal real check.
- Code review: a mismatch that *refuses* boot (spec says warn); verification reading config instead of the kernel.
- Test review: does the log-capture test assert *loudness* (level), not just presence?

### M3 — Durability hardening

**T3.1 Group commit under concurrency.**
- Goal: many connections, one fsync, everyone waits correctly.
- FRs: FR-25, FR-26.
- Tests: N-writer stress — fsync count ≪ commit count; every waiter released exactly once; filled-watermark invariant probe thread; multi-reactor handle coverage: appends through *different reactors'* handles covered by one leader `fdatasync` (the §8.2 per-inode argument, tested).
- Code review: waiter missed in the race window between "flush in flight" check and enqueue; sync batching starving a latecomer; leader dying mid-sync (who re-leads?).
- Test review: is the "exactly once" assertion idempotent-tolerant or does it count wakeups precisely? Is the stress sized to actually hit the race windows (or fault-injected instead of hoping)?

**T3.2 WAL-before-data gating.**
- Goal: no dirty page reaches disk ahead of its WAL.
- FRs: FR-24.
- Tests: planted dirty page with undurable LSN — writeback blocks until flush advances; fault-injection at the gate; checkpointer path (once it exists, T4.2) re-tested through the same gate.
- Code review: any *second* writeback path bypassing the gate (eviction, checkpoint, debug tooling).
- Test review: does the fault-injection actually toggle the gate, or mock away the code under test?

**T3.3 Crash-consistency matrix + boot replay.**
- Goal: `kill -9` at every phase leaves a consistent, honest database — and boot replays before serving.
- FRs: FR-7, FR-25, FR-26, FR-27.
- Tests: crash harness × phases (post-append, mid-fill, post-sync, post-ack): acked txns always present; unacked may exist but consistent; torn record never fsynced (replay CRC check); boot refuses to bind the socket until replay reaches clean LSN (order asserted); clean marker present after graceful shutdown → no replay on restart.
- Code review: this task *is* the attack; plus: does the runtime skip replay when a marker exists but WAL is past clean LSN (contradictory state)?
- Test review: is the kill timed by phase *deterministically* (instrumented barrier), not by sleep-and-hope? Does the suite run in CI (signals allowed) or is it bare-metal only — and is that recorded?

### M4 — Background work & shutdown

**T4.1 Orchestrator tick loop.**
- Goal: timers tick, housekeeper sleeps — provably.
- FRs: FR-5 (sleeps), FR-31 (frame).
- Tests: tick firing; idle wakeup-count assertion (no busy poll); tick skew under load bounded.
- Code review: tick overtaking itself (re-entrant scheduling); timer drift compounding.
- Test review: is the wakeup-count assertion tolerant enough for CI yet strict enough to catch a `loop {}`?

**T4.2 Checkpointer (orchestrates-only).**
- Goal: housekeeper decides, workers flush in-pool, truncatable LSN advances.
- FRs: FR-31.
- Tests: dirty pages flushed through worker `submit_to`; housekeeper thread never issues I/O (instrumented assertion); truncatable LSN advances; re-run of the T3.2 gate through this path.
- Code review: any direct housekeeper I/O; cross-NUMA Tier B touch (§9/§6.2 violation); flush fan-out missing a shard.
- Test review: does the "never touches I/O" assertion instrument the *syscall layer* (can't be fooled by an abstraction change)?

**T4.3 WAL recycler + truncatable LSN.**
- Goal: old segments recycle, never needed ones.
- FRs: FR-32.
- Tests: recycle only below the per-DB truncatable LSN; needed-segment protection under concurrent checkpoint; per-DB isolation (one DB's recycle never touches another's segments).
- Code review: off-by-one LSN (recycling the segment recovery needs); cross-DB segment touch.
- Test review: is the boundary tested at exactly-truncatable-LSN (the ±1 case), not just "clearly old" and "clearly new"?

**T4.4 Shutdown drain.**
- Goal: §10.2 verbatim, in order.
- FRs: FR-8.
- Tests: clean drain under idle load; new statements refused with a clear error during drain; deadline expiry force-aborts; final per-DB WAL flush + final BPM flush + checkpoint LSN advance; clean marker written; restart after shutdown replays nothing; rings/files closed.
- Code review: hang paths (waiter never notified) silently turning every shutdown into the deadline path; flushes skipped when "nothing dirty".
- Test review: is the drain-order (stop-accept → refuse → drain → abort → flushes) asserted as a sequence, not just end-state?

**T4.5 Idle-connection reaper.**
- Goal: idle connections are reaped by timeout, via the owning reactor.
- FRs: FR-34.
- Tests: idle past timeout → reaper signals owner → connection dropped with cleanup (FR-15 path re-tested through the reaper); active connections untouched.
- Code review: reaper dropping a connection mid-query (idle definition wrong); signal path racing a simultaneous client disconnect.
- Test review: is "idle" measured from last statement completion (asserted), not connection age?

**T4.6 Statistics collector.**
- Goal: per-core counters, summed on read.
- FRs: FR-33.
- Tests: counters from multiple workers aggregate correctly (sum, no double-count); read is via `submit_to` fan-in; counter values survive a benchmark-sized workload sanity check.
- Code review: per-core counters accidentally shared (atomics contention / wrong attribution); aggregation reading stale counters without a fence where needed.
- Test review: does the double-count test run the same txn through two workers, or just sum two synthetic counters?

### M5 — Tier C, multi-reactor coordination, bare-metal gate

**T5.1 `submit_to` primitive (spec §6.1 semantics, verbatim).**
- Goal: cross-reactor calls where the closure is `Send` but the future's internal state may be `!Send`.
- FRs: FR-35.
- Tests: `Send` closure captured state runs on target (thread-id assert); `!Send` state *created inside the future* (e.g., an `Rc` made on the target) works; result returns to caller; queueing when target busy; compile-fail test that a `!Send` *closure* is rejected.
- Code review: accidental `Send` requirement leaking onto the future; future polled on the wrong reactor; result channel lost on target panic (caller hangs).
- Test review: does the compile-fail test use the trybuild pattern (asserted at CI time), not a commented-out line?

**T5.2 Tier C snapshot + pointer-load reads.**
- Goal: boot catalog loaded; all readers pointer-load.
- FRs: FR-28.
- Tests: read path has no lock/atomic-RMW (API-shape + review; race test under concurrent readers + writer); snapshot memory reclaimed after readers finish (refcount check).
- Code review: any reader cloning into a lock; snapshot dropped under a reader (Arc discipline); a second writer path (I9).
- Test review: does the race test actually interleave (yield points / barrier), or just run and hope?

**T5.3 Tier C writer + bounded fan-out + boot publication order.**
- Goal: the single-writer path with R11 semantics; boot publishes Tier C before serving (re-asserted multi-reactor).
- FRs: FR-6, FR-29, FR-30.
- Tests: change visible on all reactors; stalled reactor → `pending N` ack, lazy adoption verified; issuing reactor stores before ack; boot-order assertion — workers refuse traffic until the initial snapshot lands.
- Code review: a second writer path; fan-out deadlock on a full mailbox; bounded-wait timer started before the stores (too early) or after (too late).
- Test review: is the stalled-reactor case produced by a *controlled stall* (instrumented), not a sleep guess?

**T5.4 DDL end-to-end (the bounce).**
- Goal: `CREATE ROLE`-class statement: worker → housekeeper → snapshot → all reactors → ack.
- FRs: FR-29.
- Tests: full integration — after ack, a *different* connection on a *different* worker sees the change with no cache invalidation; failed apply → error, snapshot unchanged; **zero-`submit_to` probe** — a debug counter on `submit_to` shows an N-query OLTP workload (no DDL) issues zero calls from the query path (FR-36's test, not just review).
- Code review: ack before all stores (R11 violation); reader caching past the snapshot pointer; partial-failure leaving half-applied `cluster_state`.
- Test review: does the "different worker" in the test actually pin to a different reactor (asserted), or is it same-reactor luck on a 2-worker CI box?

**T5.5 Live affinity + DDL surface.**
- Goal: affinity changes/creations take effect from the live snapshot; the DDL surface is honest.
- FRs: FR-10, FR-12, FR-38.
- Tests: create `single` DB → routes only to that NUMA's workers; refusal after cap; `ALTER DATABASE … SET numa_affinity` → `unsupported_feature` error (asserted code); `CREATE … WITH` records `datnumaffinity`.
- Code review: affinity read from stale boot config instead of snapshot; `ALTER` actually attempting migration.
- Test review: does the `unsupported_feature` test assert the error *code* and that no state changed?

**T5.6 Load view completion (CPU EWMA + NIC tie-breaker).**
- Goal: the §5.2 load composite, finishing R9's phase two.
- FRs: FR-11.
- Tests: CPU samples published per reactor, EWMA computed per spec; NIC-proximity tie-breaker changes the pick only on count-ties and only for `cross` DBs; composite end-to-end routing test.
- Code review: EWMA update on the housekeeper from stale samples; tie-breaker applied to `single` DBs (spec forbids — candidate set is already one NUMA).
- Test review: is the tie case constructed to be exactly-tied (asserted), not approximately?

**T5.7 Bare-metal gate (R7 due).**
- Goal: all `#[ignore]` tests come due on real hardware.
- FRs: FR-2, FR-4, FR-22, FR-27, FR-37 (+ any bare-metal tests accumulated above).
- Tests: real pinning; kernel-cmdline verification against a genuinely isolated host; NUMA locality (numa_maps); affinity refusal on a 2-NUMA box; crash matrix at scale; **soak: 10 000 idle + 1 000 active connections** (§1 goal sanity, not a gate blocker).
- Code review: full-milestone diff re-reviewed against §5 end to end (not just the last task).
- Test review: every `#[ignore]` reason string verified still-accurate; flaky candidates from CI history re-run N times.

### Milestone gates

Each milestone ends when: every task committed with both review lines clean ·
full test suite green · the milestone demo runs (M1: connect/query/commit/crash-replay;
M2: affinity routing on synthetic+real topology; M3: crash matrix; M4: clean drain;
M5: DDL visible everywhere + bare-metal gate).

## 5. Standing adversary code-review checklist (R8, per commit)

Keyed to spec §12 invariants (I1–I13) plus crash/lifecycle classes. Run against the
**full diff**; findings block the commit; one line per item in the commit body.

1. **I1** — Tier A stays `!Send` and pinned; no `Send`/`Sync` bound creeps into session types.
2. **I2** — no `RawFd`/`File` handle escapes `cascade-storage`/`cascade-wal`.
3. **I3** — `PageId` is `(DatabaseId, SegmentId, PageNo)` — never `(file_path, offset)`.
4. **I4a/b/c** — NUMA-local allocation for per-reactor memory; `single`-DB Tier B touched only from its bound NUMA (background work goes via `submit_to` to the bound NUMA); `cross`-DB cross-NUMA reads stay within the documented trade.
5. **I5** — hot-path ops carry `TenantContext`.
6. **I6** — hot-path ops accept `&Span`.
7. **I7** — every dirty-page writeback path is gated on durable WAL. *All of them.*
8. **I8** — WAL records carry full row images.
9. **I9** — Tier C read via `ArcSwap::load()` only; writes only through the housekeeper.
10. **I10** — long operators yield at batch boundaries and internal sub-boundaries.
11. **I11** — no coarse mutex on the hot path; any `Mutex<…>` is justified in the review line.
12. **I12** — `submit_to` never on the OLTP query path.
13. **I13** — routing honors NUMA affinity; no silent fallback.
14. **Crash honesty** — for each state change: what survives `kill -9` right before/after the ack? Is the ack's promise kept?
15. **Lifecycles** — Arc cycles, unbounded queues, fd leaks, lost wakeups, backpressure, shutdown ordering.
16. **Hostile inputs** — partial packets, mid-handshake drops, empty/zero/degenerate configs, duplicate/oversized messages.

### 5.1 Standing test-review checklist (R8, per commit)

Run on the new/changed tests; findings block the commit.

1. **Mutation check** — mentally break the feature: does at least one new test fail?
2. **Behavior, not implementation** — assertions survive a refactor that preserves behavior.
3. **No timing luck** — sleep-based synchronization replaced by barriers/instrumented events; any remaining timing bounded and justified.
4. **Error paths asserted precisely** — error *codes* (PG/`unsupported_feature`), log *levels*, counter *names* — not "it errored".
5. **Coverage honesty** — does the test cover the branch the task claims (all three yield cases, the torn-fill path, the tie-breaker tie), or only the happy path?
6. **Placement** — CI vs `#[ignore]` bare-metal is a deliberate choice recorded in the test, not an accident.
7. **Determinism** — seed-controlled randomness; races exercised via injection/barrier, not hope.

## 6. Traceability matrix (FR → tasks → tests)

| FR | Tasks | Test evidence |
|---|---|---|
| FR-1 | T0.2, T1.1, T2.1 | config validation; synthetic boot; refusal tests |
| FR-2 | T1.1, T2.1, T5.7 | affinity verify (CI synthetic + bare metal) |
| FR-3 | T1.2, T2.2 | SMT fixtures: exclusion, refusal, logical sets |
| FR-4 | T2.7, T5.7 | synthetic mismatch warning; bare-metal real check |
| FR-5 | T1.3, T4.1 | idle-wakeup assertion; single-socket test |
| FR-6 | T5.3 | boot-order assertion (workers refuse until snapshot) |
| FR-7 | T3.3 | replay-before-bind order; clean-marker skip |
| FR-8 | T4.4 | drain-order sequence; final flushes; marker; no-replay restart |
| FR-9 | T1.4 | `postgres`-client auth (trust, SCRAM, rejection codes) |
| FR-10 | T1.5 (partial), T2.4, T5.5 | both affinities; default inheritance; live snapshot |
| FR-11 | T1.5 (count), T2.4, T5.6 | count routing; EWMA + tie-breaker composite test |
| FR-12 | T2.4, T5.5 | `53300` refusal integration, message asserts NUMA |
| FR-13 | T1.0, T1.5 | handoff/adoption tests (thread-id, fd ownership, I2 shape) |
| FR-14 | T1.7 | compile-time `!Send` assertion |
| FR-15 | T1.11 | cleanup tests; counter returns to baseline |
| FR-16 | T1.0, T1.7 | simple-query integration; batch framing vs `batch_rows` |
| FR-17 | T1.8 | three-branch yield tests at µs deadlines |
| FR-18 | T1.8 | test-operator sub-boundary yield |
| FR-19 | T1.8, T5.7 | counter/log-level tests; kill default-off; bare-metal stall |
| FR-20 | T1.6 | signature shape; propagation identity test |
| FR-21 | T1.6 | signature shape; span propagation test |
| FR-22 | T1.9, T2.3, T2.5 | pool mapping, per-NUMA sizing, hash determinism |
| FR-23 | T1.9 | hit/miss/pin; panic-pin release; I2 shape |
| FR-24 | T3.2 | writeback gating + fault-injection |
| FR-25 | T1.10, T3.1, T3.3 | torn-fill (incl. panic); watermark probe; crash matrix |
| FR-26 | T1.10, T3.1 | leader CAS stress; exactly-once; multi-handle coverage |
| FR-27 | T1.10, T1.11, T2.6, T3.3 | kill-after-ack replay; homing + persistence |
| FR-28 | T5.2 | lock-free read path; race + reclamation tests |
| FR-29 | T5.3, T5.4 | fan-out integration; cross-worker visibility |
| FR-30 | T5.3 | stalled-reactor pending-ack; issuing-reactor-first |
| FR-31 | T4.2 | in-pool flush; housekeeper-no-I/O instrumentation |
| FR-32 | T4.3 | boundary ±1 recycle; per-DB isolation |
| FR-33 | T4.6 | multi-worker aggregation; double-count test |
| FR-34 | T4.5 | reaper timeout; active-conn untouched |
| FR-35 | T5.1 | thread-id, internal-`!Send`, compile-fail closure |
| FR-36 | T5.4 (probe), all (review) | zero-`submit_to` probe over an OLTP workload; §5 checklist item 12 at every review |
| FR-37 | T2.3, T5.7 | bare-metal numa_maps locality check |
| FR-38 | T5.5 | `unsupported_feature` code test; `datnumaffinity` recorded |

## 7. Deferred — deliberate non-goals of *this plan* (not gaps)

- **Transaction layer**: XID allocators (§8.3), MVCC visibility, undo — Tier B rows in spec §4; lands with `cascade-mvcc`/`cascade-undo` milestones. The slice's debug engine is single-writer-per-row.
- **Full access methods**: heap/B+tree/columnar (§4) — `cascade-heap`/`cascade-btree` milestones; the slice has a fixed-row debug table.
- **Real SQL surface**: planner/extended protocol/prepared statements/cursors (§3.3 Tier A rows) — post-slice; parser decision made and landed (A1).
- **NUMA-affinity migration runner** (§2.5.4): Phase 7+; v1 ships the DDL surface returning `unsupported_feature` (FR-38) — exactly as the spec prescribes.
- **Parallel query**: absent from the spec by design; the escape-hatch shape (coordinator + per-shard scans + Tier B queues) is sketched in conversation only, not planned here.
- **Logical CDC decoder** (§9 future row), resource-group *enforcement* (only TenantContext plumbing per I5), TLS (A9), multi-DB parallel replay (A8 — single-DB replay until M5), hot-attach/detach of CPUs (spec: unsupported, docs only).

## 8. Ambiguities register (unresolved or slice-scoped; none silent)

| # | Ambiguity | Status |
|---|---|---|
| A1 | **SQL surface for the slice** — spec defines none. Debug subset: single fixed-row table (`SELECT` projection, `INSERT`, `BEGIN/COMMIT`). Parser: `cascade-pg-parser` if it builds at T1.0, else micro-parser. | **Resolved (T1.0), superseded (2026-10-26)** — T1.7 shipped a hand-rolled micro-parser while `cascade-pg-parser` was a stub. The perf-demo ruling (ADR 0002, re-confirmed 2026-10-25) then landed the real binding: libpg_query vendored in-tree (`cascade-pg-parser-sys`, see VENDORED.md; gcc-only build), and the runtime's `sql.rs` now parses with PG's grammar + resolves against the fixed schema (42P01/42703 semantics stay runtime-side; parsed-but-unmodeled → 0A000). Glommio pin findings: `Placement::Fixed(usize)` pins a reactor to one CPU id directly; fd adoption is the std `FromRawFd` impl on `glommio::net::TcpStream` (verified working by `tests/spike.rs`). |
| A2 | **Load metric** — §5.2 defines a composite but no sampling cadence. Slice: 1 s sample interval, EWMA α=0.5 (recorded in T0.1 backfill). | Default set |
| A3 | **Refusal shape** — §5.2 says "clear error". Ruled PG `53300` + NUMA named (R10). | Ruled R10 |
| A4 | **Fan-out vs ack ordering** — §8.4 shows ack after all stores; a stalled reactor would stall DDL. Ruled bounded wait + lazy adoption (R11); flagged as a deliberate spec deviation-with-safety-valve. | Ruled R11 |
| A5 | **Stall kill switch** — §7.2 makes it configurable/off. R12 matches the spec; v1 operates with it off. | Ruled R12 |
| A6 | **io_uring queue depth / readahead** unpinned (§3.3). Defaults: depth 64, readahead 128 KiB. | Backfill T0.1 |
| A7 | **"Worker available"** for refusal — undefined. Ruled: below `max_conns_per_worker` (200). | Backfill T0.1 |
| A8 | **Recovery scope** — §10.1 replays parallel across DBs; the slice plan recovers a single DB throughout (multi-DB parallel replay is post-plan work, not an M5 deliverable). | Scoped, deferred |
| A9 | **TLS / channel binding** — unmentioned by the spec. Out of slice; product decision needed eventually. | Open — product decision |
| A10 | **Config naming** — keys/file layout are each crate's ADR (workspace convention). | Convention |
| A11 | **SCRAM verifier storage** — spec doesn't say where verifiers live pre-catalog. Slice: config-file users; catalog `pg_authid` later. | Slice decision; revisit |
| A12 | **NUMA-local allocator mechanism** — §2.3 names mimalloc-style as example. Runtime ADR. | Open — runtime ADR |
| A13 | **Bound-NUMA selection at first activation** (§11 open). Slice default: least-loaded NUMA. | Backfill T0.1 |
| A14 | **`cross` WAL home column** (§11: "exact column TBD"). Slice: catalog debug table, column named in the BPM/WAL ADR. | Open — catalog ADR |
| A15 | **Stall plumbing** — housekeeper polling vs in-band self-report (§11). Slice: per-reactor counters, housekeeper polls (matches §9's assumed row). | Backfill T0.1 |
| A16 | **`submit_to` API visibility** — public to higher crates or restricted (§11). Slice: crate-public; ADR when the first external caller appears. | Open — runtime ADR |
| A17 | **BPM page-table impl** — flurry vs scc vs hand-rolled (§11). BPM crate ADR; slice uses whichever lands first. | Open — BPM ADR |
| A18 | **More NUMA nodes than worker cores** (§11). Slice: validated at boot; pool with no workers is legal (serves `cross` placement only). | Default set; backfill T0.1 |
