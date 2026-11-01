//! Boot sequence (spec §10.1; FR-1, FR-2).
//!
//! Order: validate the placement rules (boot is the enforcement point — a
//! config built directly, not parsed, never skipped validation) → check
//! every configured core against the topology → verify the kernel deployment (FR-4, T2.7, warn-only) → spawn housekeepers (plain
//! `std::thread` per R5) → allocate the per-NUMA BPM pools (§10.1 step 5,
//! T2.3: node-bound memory, sized per config, before any worker exists) →
//! construct the per-DB writer on its WAL home NUMA (T2.6, A14) →
//! spawn workers (one glommio reactor per core, `Placement::Fixed`; each
//! verifies its pin, then reserves its NUMA-local arena — §10.1 step 6b)
//! → each thread reports its *verified* affinity and boot refuses unless
//! every report is a singleton equal to its assignment and no core was
//! verified twice. There is no "boot anyway" fallback: an unpinned or
//! unverified thread is a boot failure (FR-1, FR-2), and the exposed
//! `Worker`/`Housekeeper` cores are the verified values, not the config's
//! intent (T2.1).

use std::{
    collections::{HashMap, HashSet},
    io,
    os::unix::io::RawFd,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

use glommio::{
    channels::shared_channel::{new_bounded, SharedReceiver, SharedSender},
    ExecutorJoinHandle, LocalExecutorBuilder, Placement,
};

use cascade_wal::WalWriter;

use crate::{
    checkpointer::{CheckpointSite, CheckpointStats, TruncatableLsn},
    config::{AuthConfig, RuntimeConfig, SmtMode},
    coop::{self, ReactorStall},
    deployment,
    handoff::{ControlMsg, Handoff},
    pin::{self, PinError},
    pools::PoolRegistry,
    router::Router,
    tenant::TenantCounters,
    tierc::TierCSnapshot,
    topology::{TopologyError, TopologyProvider},
    walhome::{self, WalHome, WalHomeTable},
};

/// How long boot waits for a freshly spawned thread to report its pin before
/// declaring it unconfirmed.
const PIN_CONFIRM_DEADLINE: Duration = Duration::from_secs(10);

/// A booted runtime in the spec's shape: `housekeeper_cores` housekeeper
/// threads + one worker reactor per configured core (FR-1).
pub struct Runtime {
    housekeepers: Vec<Housekeeper>,
    workers: Vec<Worker>,
    /// §7.2 thresholds captured at boot for [`Runtime::poll_stalls`].
    stall_soft: Duration,
    stall_hard: Duration,
    /// The debug engine's shared per-DB writer (T1.11), when
    /// `runtime.wal_dir` is configured. Workers hold `Arc` clones; this one
    /// exposes the FR-27 observables (e.g. [`WalWriter::syncs`]) to tests
    /// and, later, diagnostics.
    wal: Option<Arc<WalWriter>>,
    /// The WAL homing table (T2.6, A14): each database's home NUMA as of
    /// boot. Empty without `runtime.wal_dir` — no writer, no home.
    wal_homes: WalHomeTable,
    /// The §10.1 step-5 pool registry (T2.3): one node-bound BPM pool per
    /// configured NUMA, in config order.
    pools: PoolRegistry,
    /// The perf-demo relation, when `runtime.demo_rows` > 0.
    demo_heap: Option<std::sync::Arc<crate::demoheap::DemoHeap>>,
    /// The static Tier C placement snapshot routing reads (FR-10, T2.4).
    /// Live catalog updates arrive with T5.5.
    tierc: Arc<TierCSnapshot>,
    /// The orchestrator tick-loop observables (T4.1), present only when a
    /// listener was given — D9 centralizes background work on the
    /// *serving* housekeeper, so a compute-only boot never ticks.
    orchestrator: Option<crate::orchestrator::OrchestratorStats>,
    /// The checkpointer's orchestration counters (T4.2, FR-31), same
    /// presence rule as `orchestrator`: nothing orchestrates without a
    /// serving housekeeper.
    checkpoint: Option<CheckpointStats>,
    /// The recycler's orchestration counters (T4.3, FR-32). Present only
    /// when something serves AND a WAL exists — no listener means nothing
    /// orchestrates; no `wal_dir` means there is nothing to recycle.
    recycle: Option<crate::recycler::RecycleStats>,
    /// The reaper's orchestration counters (T4.5, FR-34). Present only
    /// when something serves AND reaping is enabled (`idle_reap_ms > 0`,
    /// not the default — a default that kills client connections must be
    /// opted into).
    reap: Option<crate::reaper::ReapStats>,
    /// The per-DB WAL-truncatable frontier (T4.3's recycler reads it),
    /// present whenever a WAL was configured. Stays at 0 until passes
    /// actually flush stamped pages — which, with no live worker BPM yet,
    /// means the whole log stays reclaimable-by-nothing (the module doc's
    /// safety argument).
    truncatable: Option<Arc<TruncatableLsn>>,
    /// The flag [`Runtime::shutdown`] sets to start §10.2 on the serving
    /// thread (T4.4) — and to release the parked housekeepers (their park
    /// loop checks it on every wake, so a listenerless boot's shutdown is
    /// still a clean join, D9).
    shutdown_flag: Arc<AtomicBool>,
    /// The same trace the serving thread's drain appends to — `shutdown`
    /// reads it after the joins and reports the sequence.
    shutdown_trace: crate::shutdown::ShutdownTrace,
    /// A dup of the listening fd, taken before the listener moved into the
    /// serving thread. `shutdown(2)` on the dup wakes a blocked `accept(2)`
    /// immediately (Linux fails the accept with EINVAL; the dup shares the
    /// open file description) instead of waiting out the accept timeout.
    listen_dup: Option<RawFd>,
}

pub struct Housekeeper {
    pub core: u32,
    /// For the shutdown unpark: the parked (non-serving) housekeepers
    /// sleep in `park()`, and [`Runtime::shutdown`] must wake them to
    /// check the shutdown flag or their join would hang forever.
    thread: std::thread::Thread,
    join: std::thread::JoinHandle<()>,
}

pub struct Worker {
    pub numa: u32,
    pub core: u32,
    /// §10.1 step 6c: this reactor's pool binding — the index into
    /// [`Runtime::pools`] of its own NUMA's pool (`single`-affinity home;
    /// other pools stay reachable for `cross` lookups, §8.1).
    pool: usize,
    /// §10.1 step 6b (FR-37): the arena this reactor reserved in-task.
    arena_bytes: u64,
    counter: Arc<AtomicU64>,
    tenant_counters: Arc<TenantCounters>,
    stall: Arc<ReactorStall>,
    join: ExecutorJoinHandle<()>,
}

impl Worker {
    /// Live connections on this reactor — the same counter the router reads
    /// for least-loaded routing (FR-11 count component; FR-15 decrement).
    pub fn active_connections(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Per-tenant counters recorded by this reactor's sessions (FR-20).
    pub fn tenant_counters(&self) -> &TenantCounters {
        &self.tenant_counters
    }

    /// This reactor's stall totals (§7.2) — the per-reactor counters the
    /// housekeeper poll reads (A15).
    pub fn stall(&self) -> &ReactorStall {
        &self.stall
    }

    /// Index into [`Runtime::pools`] of this NUMA's pool (§10.1 step 6c).
    /// Affinity routing (T2.4) consumes this via `numa`; the in-reactor
    /// `Arc<[NumaBpm]>` handle lands with the M3 concurrent BPM.
    pub fn pool_index(&self) -> usize {
        self.pool
    }

    /// Bytes this reactor reserved for its NUMA-local working arena
    /// (FR-37). Virtual until faulted — the reservation is the placement
    /// contract.
    pub fn arena_reserved_bytes(&self) -> u64 {
        self.arena_bytes
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field(
                "housekeepers",
                &self.housekeepers.iter().map(|h| h.core).collect::<Vec<_>>(),
            )
            .field(
                "workers",
                &self
                    .workers
                    .iter()
                    .map(|w| (w.numa, w.core))
                    .collect::<Vec<_>>(),
            )
            .field("pools", &self.pools.iter().map(|p| p.numa()).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("topology unavailable: {0}")]
    Topology(#[from] TopologyError),
    /// FR-1 makes *boot* the enforcement point for placement rules: a config
    /// that reached boot unvalidated (built directly, not parsed from TOML)
    /// is refused here with the same rules the document parse applies.
    #[error("invalid placement: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("configured core {core} is not online per topology")]
    CoreOffline { core: u32 },
    #[error("housekeeper thread on core {core} failed to spawn: {message}")]
    HousekeeperSpawn { core: u32, message: String },
    #[error("worker reactor on core {core} failed to spawn: {message}")]
    WorkerSpawn { core: u32, message: String },
    #[error("pin verification failed for core {core}: {source}")]
    PinVerify {
        core: u32,
        #[source]
        source: PinError,
    },
    #[error("housekeeper pins unconfirmed for cores {cores:?} within {deadline_secs:?}")]
    HousekeeperUnconfirmed { cores: Vec<u32>, deadline_secs: u64 },
    #[error("worker pins unconfirmed for cores {cores:?} within {deadline_secs:?}")]
    WorkerUnconfirmed { cores: Vec<u32>, deadline_secs: u64 },
    #[error("physical SMT mode: cores {a} and {b} share a physical core; assign only one of any sibling pair (FR-3, spec §2.4)")]
    SmtSiblingsAssigned { a: u32, b: u32 },
    /// Two threads reported verified affinities naming the same core. The
    /// kernel cannot honestly do this (a verified pin is a singleton set),
    /// so a collision means the assignment raced or the pin layer lied —
    /// exactly what the verified-report bookkeeping exists to catch.
    #[error("more than one thread verified onto core {core}")]
    CoreVerifiedTwice { core: u32 },
    /// The pool's `size_mb` implies more frames than the BPM's frame-width
    /// (u32) can address — refuse rather than wrap (T2.3).
    #[error("demo_rows = {rows} needs {rows} pages but the tightest pool holds only {frame_budget} frames — raise numa_pools size_mb")]
    DemoTooLarge {
        rows: u32,
        frame_budget: usize,
    },
    #[error("runtime.numa_pools entry for NUMA {numa} with size_mb = {size_mb} needs more frames than the BPM can address")]
    PoolTooLarge { numa: u32, size_mb: u64 },
    /// The kernel refused the pool's memory reservation (mmap) — out of
    /// address space or memory policy limits.
    #[error("pool memory for NUMA {numa} failed to reserve: {source}")]
    PoolReserve {
        numa: u32,
        #[source]
        source: io::Error,
    },
}

impl Runtime {
    /// Boot exactly as configured (FR-1), with every pin verified after it
    /// happens (FR-2). Housekeepers are `std::thread`s (R5); workers are
    /// glommio reactors pinned via `Placement::Fixed` and verified in-task.
    /// No listening socket: the process only computes.
    pub fn boot(
        cfg: &RuntimeConfig,
        topology: &dyn TopologyProvider,
    ) -> Result<Runtime, BootError> {
        Runtime::boot_routed(cfg, topology, None, AuthConfig::default(), None)
    }

    /// Boot with the full connection path (spec §5.1): the first housekeeper
    /// accepts, authenticates (FR-9, before routing), and hands each
    /// connection to the least-loaded worker as a bare fd (FR-13); the worker
    /// adopts it onto its own ring. Per-worker channels and counters are
    /// created here in config order.
    ///
    /// `recovery` is the caller's [`crate::recovery::RecoveryPlan`] from
    /// `recovery::prepare` (T3.3, FR-7) — recovery itself happens *before*
    /// boot (and before any socket bind, which is the server's ordering to
    /// keep); boot consumes the decision: the WAL writer resumes at the
    /// replayed log end and every worker's table is seeded with the
    /// replayed rows. `None` boots without WAL memory — every existing
    /// non-server caller.
    pub fn boot_routed(
        cfg: &RuntimeConfig,
        topology: &dyn TopologyProvider,
        listener: Option<crate::accept::AcceptLoop>,
        auth: AuthConfig,
        recovery: Option<&crate::recovery::RecoveryPlan>,
    ) -> Result<Runtime, BootError> {
        // FR-1: boot is the enforcement point, not the config parser. A
        // document that arrived through `from_toml_str` was already
        // validated; one built directly was not — the rules must hold
        // regardless of the construction path. Duplicate cores (two workers
        // on one core, T2.1's review case) and cross-role overlaps are
        // refused here, before anything spawns.
        cfg.validate()?;
        let online = topology.online_cpus()?;

        // FR-1: an unpinnable (offline) core refuses boot before anything
        // spawns — the config cannot be honored, so do not half-start.
        for core in &cfg.housekeeper_cores {
            if !online.contains(core) {
                return Err(BootError::CoreOffline { core: *core });
            }
        }
        for workers in &cfg.worker_cores {
            for core in &workers.cores {
                if !online.contains(core) {
                    return Err(BootError::CoreOffline { core: *core });
                }
            }
        }

        check_smt_mode(cfg, topology)?;

        // FR-4 (T2.7): verify the kernel's CPU-isolation deployment (§2.4)
        // against the configured sets — a loud warning per mismatch, never
        // a refusal (the lists are provisioning-time; the runtime can only
        // verify). The comparison reads the kernel's own cmdline, not the
        // config.
        let cmdline = std::fs::read_to_string("/proc/cmdline").ok();
        deployment::verify_and_log(cfg, cmdline.as_deref());

        // One handoff channel + one live-connection counter per worker,
        // in config order. The router reads the counters; the workers own
        // the receivers.
        let n_workers: usize = cfg.worker_cores.iter().map(|w| w.cores.len()).sum();
        let mut senders = Vec::with_capacity(n_workers);
        let mut receivers = Vec::with_capacity(n_workers);
        let mut control_tx = Vec::with_capacity(n_workers);
        let mut control_rx = Vec::with_capacity(n_workers);
        let mut counters: Vec<Arc<AtomicU64>> = Vec::with_capacity(n_workers);
        let mut tenant_counters: Vec<Arc<TenantCounters>> = Vec::with_capacity(n_workers);
        let mut stalls: Vec<Arc<ReactorStall>> = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let (tx, rx) = new_bounded::<Handoff>(1024);
            let (ctx, crx) = new_bounded::<ControlMsg>(64);
            senders.push(tx);
            receivers.push(rx);
            control_tx.push(ctx);
            control_rx.push(crx);
            counters.push(Arc::new(AtomicU64::new(0)));
            tenant_counters.push(Arc::new(TenantCounters::new()));
            stalls.push(Arc::new(ReactorStall::new()));
        }

        // Verified affinities, shared across roles so the one-thread-per-core
        // check spans housekeepers and workers (T2.1: uniqueness over the
        // verified affinity, not the config's intent).
        let mut verified = HashSet::new();
        // §10.1 step 7's routing half (FR-10, T2.4): the placement snapshot
        // routing reads. It is config-derived and immutable, so building it
        // before the housekeepers (which §10.1 orders at step 4) cannot
        // race anything; the live Tier C feed replaces this at T5.5.
        let worker_numas: Vec<u32> = cfg
            .worker_cores
            .iter()
            .flat_map(|w| std::iter::repeat(w.numa).take(w.cores.len()))
            .collect();
        let tierc = Arc::new(TierCSnapshot::build(
            cfg.default_db_numa_affinity,
            &worker_numas,
        ));
        // T4.1 (D9, FR-5, FR-31's frame): when serving, the first
        // housekeeper owns the orchestrator tick loop — §M4 background jobs
        // (stall sampling, checkpointer fanout now; recycler, reaper,
        // statistics later) run on its thread between accepts. Boot ships
        // only the Send ingredient bundle; the serving thread assembles
        // the orchestrator and registers its jobs (some hold
        // executor-bound state — connected control senders — so the
        // orchestrator itself never crosses a thread). Without a listener
        // nothing serves and nothing ticks: the compute-only `boot` shape.
        let (tick_init, orchestrator_stats) = if listener.is_some() {
            let ticks = Arc::new(AtomicU64::new(0));
            let max_late_us = Arc::new(AtomicU64::new(0));
            let wakeups = Arc::new(AtomicU64::new(0));
            let stats = crate::orchestrator::OrchestratorStats::new(
                Duration::from_millis(cfg.tick_ms),
                Arc::clone(&ticks),
                Arc::clone(&max_late_us),
                Arc::clone(&wakeups),
            );
            let init = crate::orchestrator::OrchestratorInit {
                interval: Duration::from_millis(cfg.tick_ms),
                ticks,
                max_late_us,
                wakeups,
                stalls: stalls.clone(),
                stall_soft: Duration::from_micros(cfg.stall.soft_us),
                stall_hard: Duration::from_micros(cfg.stall.hard_us),
            };
            (Some(init), Some(stats))
        } else {
            (None, None)
        };
        // T4.2 (FR-31): the checkpointer. The truncatable frontier exists
        // whenever a WAL exists (the recycler reads it, T4.3); the fanout
        // job exists only when something orchestrates (D9: the serving
        // housekeeper — it registers on that thread, once the control
        // senders are connected against its executor). Designated reactor
        // per NUMA pool = the first worker on that pool's NUMA; the other
        // workers' control senders drop here (their loops end quietly;
        // nothing can signal them).
        let truncatable = cfg
            .wal_dir
            .is_some()
            .then(|| Arc::new(TruncatableLsn::new()));
        let checkpoint_passes = Arc::new(AtomicU64::new(0));
        let recycle_passes = Arc::new(AtomicU64::new(0));
        let recycle_reclaimed = Arc::new(AtomicU64::new(0));
        let mut designated: Vec<(u32, glommio::channels::shared_channel::SharedSender<ControlMsg>)> =
            Vec::new();
        {
            let mut seen = HashSet::new();
            for (numa, tx) in worker_numas.iter().zip(control_tx) {
                if seen.insert(*numa) {
                    designated.push((*numa, tx));
                }
            }
        }
        let checkpoint_stats = listener
            .is_some()
            .then(|| CheckpointStats::new(Arc::clone(&checkpoint_passes)));
        // T4.3 (FR-32): the recycler's target — the dev DB's WAL-home
        // NUMA — is resolved HERE, before the housekeepers spawn. Tick
        // time is too late: resolution reads the persisted-homes file,
        // and the housekeeper must never touch I/O (the syscall-layer
        // tests would catch it). The writer-construction step below
        // reuses the same decision, so the two can never drift.
        let (persisted_homes, dev_home) = match cfg.wal_dir.as_ref() {
            Some(dir) => {
                let dev = crate::tenant::dev_context().db;
                let persisted = walhome::read_homes(dir);
                let pool_numas: Vec<u32> = cfg.numa_pools.iter().map(|p| p.numa).collect();
                let home = walhome::resolve_wal_home(
                    dev,
                    &tierc.resolve(Some("dev")),
                    &persisted,
                    &pool_numas,
                );
                (persisted, Some(home))
            }
            None => (HashMap::new(), None),
        };
        let recycle_stats = (listener.is_some() && cfg.wal_dir.is_some()).then(|| {
            crate::recycler::RecycleStats::new(
                Arc::clone(&recycle_passes),
                Arc::clone(&recycle_reclaimed),
            )
        });
        // The recycler's dedicated channel (T4.3): SPSC means it cannot
        // share a sender with the checkpoint fanout, and that is the
        // point — the fanout's set stays whole while the recycle signal
        // is structurally unable to reach any reactor but the home one.
        let (recycle_tx, recycle_rx) = recycle_stats
            .is_some()
            .then(|| new_bounded::<crate::recycler::RecycleSignal>(64))
            .unzip();
        let recycle_init = match (recycle_tx, dev_home, recycle_stats.clone()) {
            (Some(sender), Some(_home), Some(stats)) => {
                let trunc = Arc::clone(truncatable.as_ref().expect("wal dir implies a frontier"));
                Some(crate::recycler::RecycleInit {
                    db: crate::tenant::dev_context().db,
                    sender,
                    truncatable: trunc,
                    stats,
                })
            }
            _ => None,
        };
        let (wal, wal_homes) = match cfg.wal_dir.as_ref() {
            Some(dir) => {
                let dev = crate::tenant::dev_context().db;
                // The same home the recycler targets (resolved above,
                // before the housekeepers spawned) — one decision, two
                // consumers.
                let home = dev_home.expect("wal dir implies a resolved home");
                // T3.3: the writer resumes at the replayed log end —
                // seeding it at 0 after a recovery would overwrite durable
                // history with this boot's first flush.
                let resume = recovery
                    .map(|p| cascade_types::Lsn(p.resume_offset))
                    .unwrap_or(cascade_types::Lsn(0));
                let (writer, constructed_on) =
                    walhome::construct_writer_on(cfg, &online, topology, home, resume);
                if let Some(plan) = recovery {
                    cascade_telemetry::log(
                        cascade_telemetry::Level::Info,
                        "recovery_applied",
                        &[
                            ("resume_offset", plan.resume_offset.to_string().as_str()),
                            ("rows", plan.seed_rows.len().to_string().as_str()),
                            ("unclean", plan.unclean.to_string().as_str()),
                            (
                                "torn_tail",
                                plan.torn_tail
                                    .map(|l| l.0.to_string())
                                    .unwrap_or_else(|| "none".into())
                                    .as_str(),
                            ),
                        ],
                    );
                }
                let mut table = WalHomeTable::new();
                table.set(dev, WalHome { numa: home, constructed_on });
                // Persist whatever this boot decided (A14), preserving any
                // rows for databases other than the dev one.
                let mut to_persist = persisted_homes;
                to_persist.insert(dev.0, home);
                walhome::write_homes(dir, &to_persist);
                (Some(writer), table)
            }
            None => (None, WalHomeTable::new()),
        };
        // T4.4 (FR-8, §10.2): the serving thread's shutdown state. The
        // draining flag and the per-worker in-flight gauges exist for every
        // boot (sessions carry them as their [`crate::session::SessionEnv`]
        // whether or not anything orchestrates); the [`ServingShutdown`]
        // bundle goes to the serving housekeeper only when a listener
        // exists. The writer is constructed ABOVE (before the housekeepers
        // spawn) precisely so this bundle can carry it — the clean marker's
        // durable LSN reads it after the final flush.
        let draining = Arc::new(AtomicBool::new(false));
        let inflight: Vec<Arc<AtomicU64>> =
            (0..n_workers).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let shutdown_trace = crate::shutdown::ShutdownTrace::new();
        // The flag exists for EVERY boot — the parked (non-serving)
        // housekeepers' park loop checks it on each wake, so
        // `Runtime::shutdown` can release them on a listenerless boot too
        // (D9 degrades to joins, not to a hang).
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let serving_shutdown = listener.is_some().then(|| crate::shutdown::ServingShutdown {
            flag: Arc::clone(&shutdown_flag),
            draining: Arc::clone(&draining),
            inflight: inflight.clone(),
            deadline: Duration::from_secs(cfg.drain_deadline_s),
            trace: shutdown_trace.clone(),
            wal: wal
                .as_ref()
                .zip(cfg.wal_dir.clone())
                .map(|(w, dir)| (Arc::clone(w), dir)),
        });
        // A dup of the listening fd, taken before the listener moves into
        // the serving thread (T4.4): `shutdown(2)` on the dup wakes a
        // blocked `accept(2)` at once — Linux fails the accepter with
        // EINVAL on a shut-down listener — instead of waiting out the
        // accept timeout. A failed dup degrades gracefully (the accept
        // timeout arm still notices the flag within one tick); `None`
        // skips the wake rather than `shutdown(2)` a bad fd.
        let listen_dup = listener.as_ref().and_then(|l| {
            let fd = unsafe { libc::dup(l.as_raw_fd()) };
            if fd < 0 {
                let err = std::io::Error::last_os_error();
                cascade_telemetry::log(
                    cascade_telemetry::Level::Warn,
                    "shutdown_listen_dup_failed",
                    &[("reason", err.to_string().as_str())],
                );
                None
            } else {
                Some(fd)
            }
        });
        // The drain channels (T4.4): one SPSC channel per designated
        // reactor — the third signal family (control: T4.2, recycle:
        // T4.3), so §10.2's one-shot final flushes never queue behind tick
        // traffic and vice versa. The HOME IDENTITY rides the tuple — if
        // the resolved WAL home names a pooled NUMA with no workers, no
        // sender is flagged, and the drain (not ack slot position) knows
        // no one can flush the tail: the marker is refused, honestly.
        // Empty on a listenerless boot.
        let (drain_tx, drain_rx_by_numa) = if listener.is_some() {
            let mut txs = Vec::new();
            let mut rxs = Vec::new();
            for (numa, _) in &designated {
                let (tx, rx) = new_bounded::<crate::shutdown::DrainMsg>(8);
                txs.push((*numa, Some(*numa) == dev_home, tx));
                rxs.push((*numa, rx));
            }
            (txs, rxs)
        } else {
            (Vec::new(), Vec::new())
        };
        // The reap channels (T4.5, FR-34): the FOURTH signal family
        // (control: T4.2, recycle: T4.3, drain: T4.4) — SPSC forbids
        // sharing, and targeting = scope again, but the scope differs:
        // idle connections live on EVERY worker, not just designated
        // ones, so this family fans out one channel per worker core
        // rather than per pool. Disabled (`idle_reap_ms == 0`, the
        // default) or listenerless: no channels, no job, no loops.
        let worker_core_count: usize = cfg
            .worker_cores
            .iter()
            .map(|w| w.cores.len())
            .sum();
        let reap_enabled = listener.is_some() && cfg.idle_reap_ms > 0;
        let reap_signals = Arc::new(AtomicU64::new(0));
        let reap_refused = Arc::new(AtomicU64::new(0));
        let reap_reaped = Arc::new(AtomicU64::new(0));
        let reap_stats = reap_enabled.then(|| {
            crate::reaper::ReapStats::new(
                Arc::clone(&reap_signals),
                Arc::clone(&reap_refused),
                Arc::clone(&reap_reaped),
            )
        });
        // One `new_bounded` per channel binds BOTH halves — the senders
        // go into `ReapInit` (serving side), the receivers stay here for
        // the workers, in the same per-core order `spawn_workers` zips.
        let (reap_init, reap_receivers): (
            Option<crate::reaper::ReapInit>,
            Vec<Option<glommio::channels::shared_channel::SharedReceiver<crate::reaper::ReapTick>>>,
        ) = match reap_stats.clone() {
            Some(stats) => {
                let mut senders = Vec::with_capacity(worker_core_count);
                let mut rxs = Vec::with_capacity(worker_core_count);
                for _ in 0..worker_core_count {
                    let (tx, rx) = new_bounded::<crate::reaper::ReapTick>(8);
                    senders.push(tx);
                    rxs.push(rx);
                }
                let init = crate::reaper::ReapInit { senders, stats };
                (Some(init), rxs.into_iter().map(Some).collect())
            }
            None => (
                None,
                (0..worker_core_count).map(|_| None).collect::<Vec<_>>(),
            ),
        };
        let housekeepers = spawn_housekeepers(
            cfg,
            listener,
            auth,
            senders,
            worker_numas.clone(),
            counters.clone(),
            Arc::clone(&tierc),
            tick_init,
            designated,
            checkpoint_stats.clone(),
            recycle_init,
            serving_shutdown,
            drain_tx,
            reap_init,
            Arc::clone(&shutdown_flag),
            &mut verified,
        )?;
        // §10.1 step 5 (T2.3): one BPM pool per configured NUMA, sized per
        // config, its memory bound to its node — after topology is resolved
        // and placement validated, before any worker exists to fault it.
        let mut pools = PoolRegistry::build(cfg)?;
        // T2.6 (A14, §8.2): the per-DB writer, homed per affinity —
        // `single` follows the Tier C bound NUMA (the writers sit with the
        // only workers allowed to serve the DB, I13); `cross` gets a
        // designated NUMA, derived on first creation and persisted under
        // the WAL directory from then on (A14: a reboot re-reads the
        // choice, it never re-derives — a changed pool layout must not
        // migrate a database's WAL stream). One shared writer (T1.11):
        // every reactor's sessions append through it, so it is constructed
        // once, on its home NUMA — never per-reactor (the multi-home bug).
        // The per-reactor handle *open* (§10.1 step 8) stays in-task on
        // each worker's own ring.
        // Perf-demo relation: populated once, on the boot thread, before
        // any reactor exists to read it (module-doc discipline).
        let demo_heap = match cfg.demo_rows {
            0 => None,
            rows => {
                let db = crate::tenant::dev_context().db;
                match crate::demoheap::DemoHeap::populate(rows, db, &mut pools) {
                    Ok(heap) => Some(std::sync::Arc::new(heap)),
                    Err(frame_budget) => {
                        return Err(BootError::DemoTooLarge {
                            rows,
                            frame_budget,
                        })
                    }
                }
            }
        };

        // The recycle channel's receiving end lands on the FIRST worker of
        // the WAL-home NUMA — the same per-NUMA designated-reactor rule the
        // checkpoint fanout's senders follow. A boot with no recycler (no
        // listener, or no WAL) hands every worker `None`: no loop, no
        // channel, nothing to drain.
        let mut recycle_rx_slot = recycle_rx;
        let recycle_receivers: Vec<Option<_>> = worker_numas
            .iter()
            .map(|numa| {
                if recycle_rx_slot.is_some() && Some(*numa) == dev_home {
                    recycle_rx_slot.take()
                } else {
                    None
                }
            })
            .collect();
        // The drain channels' receiving ends follow the same rule (T4.4):
        // the first worker of each NUMA pool is that pool's designated
        // reactor, and `drain_rx_by_numa` was created in designated order,
        // so the k-th distinct NUMA takes the k-th receiver.
        let mut drain_rx_left = drain_rx_by_numa.into_iter();
        let mut drain_seen = HashSet::new();
        let drain_receivers: Vec<Option<_>> = worker_numas
            .iter()
            .map(|numa| {
                if drain_seen.insert(*numa) {
                    drain_rx_left.next().map(|(_, rx)| rx)
                } else {
                    None
                }
            })
            .collect();
        let workers = spawn_workers(
            cfg,
            receivers,
            control_rx,
            recycle_receivers,
            drain_receivers,
            reap_receivers,
            &reap_reaped,
            draining,
            inflight,
            dev_home,
            &checkpoint_passes,
            &recycle_passes,
            &recycle_reclaimed,
            &counters,
            &tenant_counters,
            &stalls,
            wal.as_ref(),
            cfg.batch_rows,
            &pools,
            demo_heap.clone(),
            recovery.map(|p| p.seed_rows.as_slice()).unwrap_or(&[]),
            &mut verified,
        )?;

        Ok(Runtime {
            housekeepers,
            workers,
            stall_soft: Duration::from_micros(cfg.stall.soft_us),
            stall_hard: Duration::from_micros(cfg.stall.hard_us),
            wal,
            wal_homes,
            pools,
            tierc,
            demo_heap,
            orchestrator: orchestrator_stats,
            checkpoint: checkpoint_stats,
            recycle: recycle_stats,
            reap: reap_stats,
            truncatable,
            shutdown_flag,
            shutdown_trace,
            listen_dup,
        })
    }

    pub fn housekeepers(&self) -> &[Housekeeper] {
        &self.housekeepers
    }

    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }

    /// The orchestrator tick-loop observables (T4.1): ticks fired, worst
    /// tick lateness, housekeeper wakeups. `None` when booted without a
    /// listener — nothing serves, nothing ticks (D9).
    pub fn orchestrator(&self) -> Option<&crate::orchestrator::OrchestratorStats> {
        self.orchestrator.as_ref()
    }

    /// The checkpointer's orchestration counters (T4.2, FR-31): pass
    /// signals sent / dropped-full / passes workers ran. `None` when
    /// nothing orchestrates (no listener, D9).
    pub fn checkpoint_stats(&self) -> Option<&CheckpointStats> {
        self.checkpoint.as_ref()
    }

    /// The recycler's orchestration counters (T4.3, FR-32): recycle
    /// signals to the WAL-home reactor / dropped-full / recycles run /
    /// bytes computed reclaimable. `None` without a listener or a WAL.
    pub fn recycle_stats(&self) -> Option<&crate::recycler::RecycleStats> {
        self.recycle.as_ref()
    }

    /// The reaper's orchestration counters (T4.5, FR-34): reap signals
    /// fanned out / dropped-full / connections condemned across all
    /// workers. `None` without a listener or with reaping disabled
    /// (`idle_reap_ms == 0`, the default).
    pub fn reap_stats(&self) -> Option<&crate::reaper::ReapStats> {
        self.reap.as_ref()
    }

    /// The WAL-truncatable frontier (T4.2/FR-31, consumed by the T4.3
    /// recycler): WAL strictly below this LSN is reclaimable. `None`
    /// without a WAL; `Some(Lsn(0))` means nothing is reclaimable — the
    /// live engine's standing truth until durable page images exist.
    pub fn truncatable_lsn(&self) -> Option<cascade_types::Lsn> {
        self.truncatable.as_ref().map(|t| t.get())
    }

    /// One housekeeper-style stall sample over every worker reactor (A15),
    /// returning `(soft, hard)` totals across all reactors. The orchestrator
    /// runs the same poll every tick via its `StallWatch` job (T4.1); this
    /// method remains the synchronous surface for tests and the shutdown
    /// path.
    pub fn poll_stalls(&self) -> (u64, u64) {
        let mut soft = 0;
        let mut hard = 0;
        for w in &self.workers {
            coop::poll_stall(w.stall(), self.stall_soft, self.stall_hard);
            soft += w.stall().soft_total();
            hard += w.stall().hard_total();
        }
        (soft, hard)
    }

    /// The debug engine's shared WAL writer, when `runtime.wal_dir` is
    /// configured (T1.11). Its watermarks and barrier count are the FR-27
    /// observables.
    pub fn debug_wal(&self) -> Option<&Arc<WalWriter>> {
        self.wal.as_ref()
    }

    /// The WAL homing table (T2.6, A14): each database's home NUMA, plus
    /// the first-touch probe of where its writer was constructed.
    pub fn wal_homes(&self) -> &WalHomeTable {
        &self.wal_homes
    }

    /// The perf-demo relation (read-only, pool-resident), when booted
    /// with `demo_rows > 0`.
    pub fn demo_heap(&self) -> Option<&std::sync::Arc<crate::demoheap::DemoHeap>> {
        self.demo_heap.as_ref()
    }

    /// The §10.1 step-5 pool registry (T2.3): one node-bound BPM pool per
    /// configured NUMA, in config order.
    pub fn pools(&self) -> &PoolRegistry {
        &self.pools
    }

    /// The Tier C placement snapshot routing reads (FR-10, T2.4) — the
    /// same `Arc` the router holds. Static until T5.5's live feed.
    pub fn tierc(&self) -> &Arc<TierCSnapshot> {
        &self.tierc
    }

    /// §10.2 graceful shutdown (T4.4, FR-8) — the caller's half, consuming
    /// the runtime:
    ///
    /// 1–4 run on the *serving* housekeeper (see
    /// [`crate::shutdown`]): this method sets the flag the accept loop
    /// polls and wakes a blocked `accept(2)` via the dup'd fd, then joins
    /// the housekeepers — a completed housekeeper join means the whole
    /// drain finished (final flushes acked, clean marker written, trace
    /// recorded).
    /// 5 is here: joining a worker drops its executor, and a dropped
    /// executor kills the detached session tasks and closes their fds,
    /// rings, and WAL handles — §10.2's force-abort-by-drop for whatever
    /// the deadline could not drain.
    ///
    /// Returns the phase trace in completion order — the same sequence the
    /// integration suite asserts.
    pub fn shutdown(self) -> ShutdownReport {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        // Release the parked housekeepers now — their join would otherwise
        // hang forever (they sleep in `park()`, which the flag store does
        // not wake).
        for h in &self.housekeepers {
            h.thread.unpark();
        }
        if let Some(dup) = self.listen_dup {
            // Wake a blocked accept(2) immediately. The dup shares the
            // listening socket's open file description, so this lands on
            // the real listener; the dup has served its purpose and is
            // closed here (the serving thread's accept reads the failure
            // and its flag check recognizes shutdown).
            unsafe {
                libc::shutdown(dup, libc::SHUT_RDWR);
                libc::close(dup);
            }
        }
        // Housekeepers first: a worker must stay alive to ack the final
        // flushes, so the reactors drop only after the drain is done.
        let mut housekeepers_stopped = 0;
        for h in self.housekeepers {
            if h.join.join().is_ok() {
                housekeepers_stopped += 1;
            }
        }
        let mut workers_stopped = 0;
        for w in self.workers {
            if w.join.join().is_ok() {
                workers_stopped += 1;
            }
        }
        self.shutdown_trace.record("reactors-dropped");
        ShutdownReport {
            phases: self.shutdown_trace.phases(),
            housekeepers_stopped,
            workers_stopped,
        }
    }
}

/// What [`Runtime::shutdown`] reports: the §10.2 phase trace in completion
/// order and the join tallies (a tally below the booted count means a
/// thread panicked — every other failure mode refuses boot, T2.1).
#[derive(Debug, Clone)]
pub struct ShutdownReport {
    pub phases: Vec<&'static str>,
    pub housekeepers_stopped: usize,
    pub workers_stopped: usize,
}

/// FR-3: in `physical` SMT mode (the default) one worker per *physical* core
/// — HT siblings are excluded from every role, so a config assigning two
/// sibling cores anywhere (worker+worker or housekeeper+worker) refuses
/// boot. `logical` mode has no such rule: one worker per vCPU, siblings
/// included. Runs before anything spawns — a refusal never leaves a
/// half-booted state (T2.2 test-review).
///
/// The sibling relation is tested in *both* directions: a provider whose
/// sibling map is asymmetric (records siblings on one cpu only — malformed
/// but not impossible) must not slip a pair through (T2.2 code review).
pub fn check_smt_mode(
    cfg: &RuntimeConfig,
    topology: &dyn TopologyProvider,
) -> Result<(), BootError> {
    if cfg.topology.mode != SmtMode::Physical {
        return Ok(());
    }
    let mut assigned: Vec<u32> = cfg.housekeeper_cores.clone();
    for workers in &cfg.worker_cores {
        assigned.extend_from_slice(&workers.cores);
    }
    for (i, &a) in assigned.iter().enumerate() {
        for &b in &assigned[i + 1..] {
            let siblings_a = topology.thread_siblings(a);
            let siblings_b = topology.thread_siblings(b);
            if siblings_a.contains(&b) || siblings_b.contains(&a) {
                return Err(BootError::SmtSiblingsAssigned { a, b });
            }
        }
    }
    Ok(())
}

/// Spawn one `std::thread` per housekeeper core (R5); each pins itself and
/// reports its *verified* affinity. Boot fails unless every report arrives
/// within the deadline and matches its core. When a listener is given, the
/// *first* housekeeper core runs the router loop (accept → auth → handoff)
/// with the orchestrator tick loop on the same thread (T4.1, D9); the rest
/// park.
#[allow(clippy::too_many_arguments)]
fn spawn_housekeepers(
    cfg: &RuntimeConfig,
    listener: Option<crate::accept::AcceptLoop>,
    auth: AuthConfig,
    senders: Vec<SharedSender<Handoff>>,
    worker_numas: Vec<u32>,
    counters: Vec<Arc<AtomicU64>>,
    tierc: Arc<TierCSnapshot>,
    tick_init: Option<crate::orchestrator::OrchestratorInit>,
    designated: Vec<(u32, SharedSender<ControlMsg>)>,
    checkpoint: Option<CheckpointStats>,
    recycle: Option<crate::recycler::RecycleInit>,
    shutdown: Option<crate::shutdown::ServingShutdown>,
    drain: Vec<(u32, bool, SharedSender<crate::shutdown::DrainMsg>)>,
    reap: Option<crate::reaper::ReapInit>,
    shutdown_flag: Arc<AtomicBool>,
    verified_all: &mut HashSet<u32>,
) -> Result<Vec<Housekeeper>, BootError> {
    let (tx, rx) = mpsc::channel();
    let mut out = Vec::new();
    let max_conns = cfg.max_conns_per_worker;
    let mut serving = match (listener, tick_init, checkpoint, recycle, shutdown, drain, reap) {
        (Some(loop_), Some(init), Some(stats), recycle, shutdown, drain, reap) => {
            Some((loop_, auth, senders, worker_numas, counters, tierc, init, designated, stats, recycle, shutdown, drain, reap))
        }
        _ => None,
    };
    for &core in &cfg.housekeeper_cores {
        let tx = tx.clone();
        let body: Box<dyn FnOnce() + Send> = match serving.take() {
            Some((
                loop_,
                auth,
                senders,
                numas,
                counters,
                tierc,
                tick_init,
                designated,
                stats,
                recycle,
                shutdown,
                drain,
                reap,
            )) => Box::new(move || {
                // Pin first; then accept (blocking, bounded by the next
                // tick deadline — sleeps between events, FR-5) →
                // authenticate → route by affinity → hand off; due
                // ticks fire between accepts (T4.1, D9). Housekeepers
                // reserve no arena (the trailing 0 below). After the
                // accept loop breaks on the shutdown flag (T4.4), serve
                // runs the whole §10.2 drain on this same thread before
                // returning.
                let _ = tx.send((core, pin::pin_current(core), 0));
                Router::serve(
                    loop_, auth, senders, numas, counters, tierc, max_conns, tick_init,
                    designated, stats, recycle, shutdown, drain, reap,
                );
            }),
            None => {
                // The orchestrator ticks on the *serving* housekeeper (D9,
                // T4.1); this one has no work yet — park, no busy-wait
                // (FR-5). The park loop exits when shutdown releases it:
                // without this check the join in `Runtime::shutdown`
                // would hang forever (T4.4 review fix).
                let flag = Arc::clone(&shutdown_flag);
                Box::new(move || {
                    let _ = tx.send((core, pin::pin_current(core), 0));
                    loop {
                        std::thread::park();
                        if flag.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                })
            }
        };
        let join = std::thread::Builder::new()
            .name(format!("cascade-hk-{core}"))
            .spawn(body)
            .map_err(|e| BootError::HousekeeperSpawn {
                core,
                message: e.to_string(),
            })?;
        let thread = join.thread().clone();
        out.push(Housekeeper { core, thread, join });
    }
    drop(tx); // so a dead thread's disconnect is observable
    let (verified, _arenas) =
        confirm_pins(rx, &cfg.housekeeper_cores, verified_all, |cores, deadline_secs| {
            BootError::HousekeeperUnconfirmed {
                cores,
                deadline_secs,
            }
        })?;
    // Expose what the kernel verified, not what the config asked for.
    for h in &mut out {
        h.core = verified[&h.core];
    }
    Ok(out)
}

/// Spawn one glommio reactor per worker core, pinned via `Placement::Fixed`.
/// The reactor verifies its own affinity in-task (FR-2), reserves its
/// NUMA-local working arena (§10.1 step 6b, FR-37) — in-task, on the
/// verified core, so its pages first-touch local by construction — connects
/// its handoff-channel receiver, and adopts each handed-off fd onto its own
/// ring (FR-13), where it becomes a Tier A session (FR-14) running the
/// simple-query loop (FR-16). The debug table is per-reactor: all sessions
/// on this worker share one `Rc<Table>` — same-thread sharing only, which is
/// exactly what the `Rc` inside `Session` enforces.
#[allow(clippy::too_many_arguments)]
fn spawn_workers(
    cfg: &RuntimeConfig,
    receivers: Vec<SharedReceiver<Handoff>>,
    control_receivers: Vec<SharedReceiver<ControlMsg>>,
    recycle_receivers: Vec<Option<SharedReceiver<crate::recycler::RecycleSignal>>>,
    drain_receivers: Vec<Option<SharedReceiver<crate::shutdown::DrainMsg>>>,
    reap_receivers: Vec<Option<SharedReceiver<crate::reaper::ReapTick>>>,
    reap_reaped: &Arc<AtomicU64>,
    draining: Arc<AtomicBool>,
    inflight: Vec<Arc<AtomicU64>>,
    dev_home: Option<u32>,
    checkpoint_passes: &Arc<AtomicU64>,
    recycle_passes: &Arc<AtomicU64>,
    recycle_reclaimed: &Arc<AtomicU64>,
    counters: &[Arc<AtomicU64>],
    tenant_counters: &[Arc<TenantCounters>],
    stalls: &[Arc<ReactorStall>],
    wal: Option<&Arc<WalWriter>>,
    batch_rows: u32,
    pools: &PoolRegistry,
    demo_heap: Option<Arc<crate::demoheap::DemoHeap>>,
    seed_rows: &[(i32, String)],
    verified_all: &mut HashSet<u32>,
) -> Result<Vec<Worker>, BootError> {
    let (tx, rx) = mpsc::channel();
    let mut out = Vec::new();
    let per_core: Vec<(u32, u32)> = cfg
        .worker_cores
        .iter()
        .flat_map(|w| w.cores.iter().map(move |&c| (w.numa, c)))
        .collect();
    for (((((((( (numa, core), rx), ctrl_rx), recycle_rx), drain_rx), reap_rx), counter), inflight), (tenant_counters, stall)) in
        per_core
            .into_iter()
            .zip(receivers)
            .zip(control_receivers)
            .zip(recycle_receivers)
            .zip(drain_receivers)
            .zip(reap_receivers)
            .zip(counters.iter())
            .zip(inflight)
            .zip(tenant_counters.iter().zip(stalls.iter()))
    {
        // §10.1 step 6c: bind this reactor to its own NUMA's pool. A
        // validated config gives every worker NUMA a pool
        // (`ConfigError::MissingPool`), so the index always exists.
        let pool = pools
            .index_for_numa(numa)
            .expect("validated config gives every worker numa a pool");
        let tx = tx.clone();
        let task_counter = Arc::clone(counter);
        let task_tenants = Arc::clone(tenant_counters);
        let task_stall = Arc::clone(stall);
        let task_site = CheckpointSite::new(Arc::clone(checkpoint_passes));
        let task_recycle = crate::recycler::RecycleSite::new(
            Arc::clone(recycle_passes),
            Arc::clone(recycle_reclaimed),
        );
        // The drain plane (T4.4): the designated reactors get their own
        // loop over the drain channel, with a site over the same pass
        // counter and — on the WAL-home reactor — a cloned WAL pair for
        // the final tail flush. `is_home` is the same dev-home decision
        // the boot thread made (one decision, two consumers again).
        let task_drain_site = CheckpointSite::new(Arc::clone(checkpoint_passes));
        let task_env = crate::session::SessionEnv {
            draining: Arc::clone(&draining),
            inflight: Arc::clone(&inflight),
        };
        let task_is_home = Some(numa) == dev_home;
        let reap_reaped_counter = Arc::clone(reap_reaped);
        let reap_timeout_ms = cfg.idle_reap_ms;
        let task_writer = wal.map(Arc::clone);
        let task_wal_dir = cfg.wal_dir.clone();
        let task_heap = demo_heap.clone();
        let task_seed = seed_rows.to_vec();
        let deadline = Duration::from_micros(cfg.yield_deadline_us);
        let join = LocalExecutorBuilder::new(Placement::Fixed(core as usize))
            .name(&format!("cascade-w-{core}"))
            .spawn(move || async move {
                let verified = pin::verify_core(core);
                // §10.1 step 6b (FR-37): reserve this reactor's working
                // arena here — on the verified core — before anything else
                // can run on this ring. Kept for the task's lifetime; the
                // allocator that consumes it is the runtime ADR.
                let arena = crate::arena::ReactorArena::reserve(
                    crate::arena::ARENA_RESERVE_BYTES,
                )
                .expect("reactor arena reservation");
                let _ = tx.send((core, verified, arena.len() as u64));
                // Reactor-local yield/stall state (§7.1/§7.2) — installed
                // before any session can run on this ring.
                coop::configure_for_reactor(Arc::clone(&task_stall), deadline);
                coop::spawn_heartbeat();
                // T1.11 (I2): this reactor's WAL handle, opened on — and
                // owned by — this ring, shared by all its sessions.
                let wal = match (task_writer, task_wal_dir) {
                    (Some(writer), Some(dir)) => {
                        let handle =
                            cascade_wal::WalHandle::open(&dir, crate::tenant::dev_context().db)
                                .await
                                .expect("worker wal handle opens");
                        Some((writer, Rc::new(handle)))
                    }
                    _ => None,
                };
                let handoffs = rx.connect().await;
                // The control plane (T4.2): checkpoint signals arrive on
                // their own channel and run as a sibling task on this
                // same ring — never queued behind connection handoffs.
                let controls = ctrl_rx.connect().await;
                glommio::spawn_local(crate::checkpointer::control_loop(controls, task_site))
                    .detach();
                // The recycle plane (T4.3): its own channel, its own loop,
                // on the WAL-home NUMA's first worker only.
                if let Some(recycle_rx) = recycle_rx {
                    let recycles = recycle_rx.connect().await;
                    glommio::spawn_local(crate::recycler::recycle_loop(recycles, task_recycle))
                        .detach();
                }
                // The drain plane (T4.4, §10.2 steps 3+4): §10.2's final
                // flushes arrive on their own channel — designated
                // reactors only. The loop dies when the serving side drops
                // its sender (the drain is one-shot) or with this ring.
                if let Some(drain_rx) = drain_rx {
                    let drains = drain_rx.connect().await;
                    let final_flush = crate::shutdown::FinalFlushCtx {
                        wal: wal
                            .as_ref()
                            .map(|(w, h)| (Arc::clone(w), Rc::clone(h))),
                        is_home: task_is_home,
                    };
                    glommio::spawn_local(crate::shutdown::drain_loop(
                        drains,
                        task_drain_site,
                        Some(final_flush),
                    ))
                    .detach();
                }
                // The reap plane (T4.5, FR-34): its own channel, its own
                // loop — on EVERY worker, not just designated ones, because
                // idle connections live everywhere. The registry is the
                // sessions' Tier A bookkeeping, created on this ring and
                // shared (Rc) with the reap loop and every session task
                // below; the loop is spawned only when reaping is enabled,
                // but the registry's absence is exactly what tells a
                // session to skip the bookkeeping (`reap == None`).
                let task_reap_registry =
                    reap_rx.as_ref().map(|_| Rc::new(crate::reaper::ReapRegistry::new()));
                if let Some(rx) = reap_rx {
                    let reaps = rx.connect().await;
                    let registry = task_reap_registry
                        .as_ref()
                        .expect("registry built alongside its receiver");
                    glommio::spawn_local(crate::reaper::reap_loop(
                        reaps,
                        Rc::clone(registry),
                        Duration::from_millis(reap_timeout_ms),
                        Arc::clone(&reap_reaped_counter),
                    ))
                    .detach();
                }
                let table = Rc::new(crate::session::Table::new());
                // T3.3: replayed committed rows land in every worker's
                // table before the first session — the debug slice has no
                // page store to read back from, so the seed *is* the
                // recovered state.
                table.seed(&task_seed);
                while let Some(handoff) = handoffs.recv().await {
                    let counter = task_counter.clone();
                    let tenants = task_tenants.clone();
                    let table = Rc::clone(&table);
                    let heap = task_heap.clone();
                    let wal = wal.as_ref().map(|(writer, handle)| crate::session::Wal {
                        writer: Arc::clone(writer),
                        handle: Rc::clone(handle),
                    });
                    glommio::spawn_local(crate::session::run(
                        handoff, table, counter, tenants, wal, heap, batch_rows,
                        task_env.clone(),
                        task_reap_registry.as_ref().map(Rc::clone),
                    ))
                    .detach();
                }
            })
            .map_err(|e| BootError::WorkerSpawn {
                core,
                message: e.to_string(),
            })?;
        out.push(Worker {
            numa,
            core,
            pool,
            arena_bytes: 0,
            counter: Arc::clone(counter),
            tenant_counters: Arc::clone(tenant_counters),
            stall: Arc::clone(stall),
            join,
        });
    }
    drop(tx);
    let (verified, arenas) = confirm_pins(
        rx,
        &cfg.worker_cores
            .iter()
            .flat_map(|w| w.cores.clone())
            .collect::<Vec<_>>(),
        verified_all,
        |cores, deadline_secs| BootError::WorkerUnconfirmed {
            cores,
            deadline_secs,
        },
    )?;
    // Expose what the kernel verified, not what the config asked for
    // (T2.1): routing, tests, and diagnostics all read the verified fact.
    // The arena reports are keyed by the assigned core — look them up
    // before the fixup renames it.
    for w in &mut out {
        let assigned = w.core;
        w.core = verified[&assigned];
        w.arena_bytes = arenas[&assigned];
    }
    Ok(out)
}

/// What a confirmed boot phase reports back: assigned core → verified
/// core, and assigned core → arena bytes reserved in-task (0 for
/// housekeepers).
type Verified = (HashMap<u32, u32>, HashMap<u32, u64>);

/// Collect pin reports: every expected core must report its *verified*
/// affinity within the deadline, the set must be a singleton equal to its
/// assignment, and no two threads — across every role that shares
/// `verified_all` — may verify onto the same core. Uniqueness is checked
/// over the kernel's read-back (T2.1), not the config's intent: the
/// assignment was already validated by `RuntimeConfig::validate`; this is
/// the layer that catches a pin that landed somewhere else. Each report
/// also carries the reactor's arena reservation size (0 for housekeepers,
/// which reserve none). Returns assigned → verified core, and assigned →
/// arena bytes.
fn confirm_pins(
    rx: mpsc::Receiver<(u32, Result<Vec<u32>, PinError>, u64)>,
    expected: &[u32],
    verified_all: &mut HashSet<u32>,
    unconfirmed: fn(Vec<u32>, u64) -> BootError,
) -> Result<Verified, BootError> {
    let mut remaining: Vec<u32> = expected.to_vec();
    let mut verified: HashMap<u32, u32> = HashMap::new();
    let mut arenas: HashMap<u32, u64> = HashMap::new();
    let deadline = PIN_CONFIRM_DEADLINE;
    while !remaining.is_empty() {
        match rx.recv_timeout(deadline) {
            Ok((core, Ok(seen), arena_bytes)) => {
                // verify_core only succeeds on a singleton set equal to the
                // assignment; re-assert so the uniqueness check below is
                // always over a well-formed value.
                let &v = seen.first().ok_or_else(|| BootError::PinVerify {
                    core,
                    source: PinError::VerifyFailed {
                        core,
                        seen: seen.clone(),
                    },
                })?;
                if !verified_all.insert(v) {
                    return Err(BootError::CoreVerifiedTwice { core: v });
                }
                verified.insert(core, v);
                arenas.insert(core, arena_bytes);
                remaining.retain(|&c| c != core);
            }
            Ok((core, Err(source), _)) => return Err(BootError::PinVerify { core, source }),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(unconfirmed(remaining, deadline.as_secs()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(unconfirmed(remaining, deadline.as_secs()))
            }
        }
    }
    Ok((verified, arenas))
}
