//! Boot sequence (spec §10.1; FR-1, FR-2).
//!
//! Order: check every configured core against the topology → spawn
//! housekeepers (plain `std::thread` per R5) → spawn workers (one glommio
//! reactor per core, `Placement::Fixed`) → each thread reports its observed
//! affinity and boot refuses unless every report matches its core. There is
//! no "boot anyway" fallback: an unpinned or unverified thread is a boot
//! failure (FR-1, FR-2).

use std::{
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
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
    config::{AuthConfig, RuntimeConfig, SmtMode},
    coop::{self, ReactorStall},
    handoff::Handoff,
    pin::{self, PinError},
    router::Router,
    tenant::TenantCounters,
    topology::{TopologyError, TopologyProvider},
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
}

pub struct Housekeeper {
    pub core: u32,
    join: std::thread::JoinHandle<()>,
}

pub struct Worker {
    pub numa: u32,
    pub core: u32,
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
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("topology unavailable: {0}")]
    Topology(#[from] TopologyError),
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
        Runtime::boot_routed(cfg, topology, None, AuthConfig::default())
    }

    /// Boot with the full connection path (spec §5.1): the first housekeeper
    /// accepts, authenticates (FR-9, before routing), and hands each
    /// connection to the least-loaded worker as a bare fd (FR-13); the worker
    /// adopts it onto its own ring. Per-worker channels and counters are
    /// created here in config order.
    pub fn boot_routed(
        cfg: &RuntimeConfig,
        topology: &dyn TopologyProvider,
        listener: Option<crate::accept::AcceptLoop>,
        auth: AuthConfig,
    ) -> Result<Runtime, BootError> {
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

        // One handoff channel + one live-connection counter per worker,
        // in config order. The router reads the counters; the workers own
        // the receivers.
        let n_workers: usize = cfg.worker_cores.iter().map(|w| w.cores.len()).sum();
        let mut senders = Vec::with_capacity(n_workers);
        let mut receivers = Vec::with_capacity(n_workers);
        let mut counters: Vec<Arc<AtomicU64>> = Vec::with_capacity(n_workers);
        let mut tenant_counters: Vec<Arc<TenantCounters>> = Vec::with_capacity(n_workers);
        let mut stalls: Vec<Arc<ReactorStall>> = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let (tx, rx) = new_bounded::<Handoff>(1024);
            senders.push(tx);
            receivers.push(rx);
            counters.push(Arc::new(AtomicU64::new(0)));
            tenant_counters.push(Arc::new(TenantCounters::new()));
            stalls.push(Arc::new(ReactorStall::new()));
        }

        let housekeepers = spawn_housekeepers(cfg, listener, auth, senders, counters.clone())?;
        // T1.11: one shared per-DB writer when a WAL directory is configured
        // (the debug engine's fixed dev database). Handles are opened
        // per-reactor, in-task, on each worker's own ring.
        let wal = cfg
            .wal_dir
            .as_ref()
            .map(|_| Arc::new(cascade_wal::WalWriter::new()));
        let workers = spawn_workers(
            cfg,
            receivers,
            &counters,
            &tenant_counters,
            &stalls,
            wal.as_ref(),
            cfg.batch_rows,
        )?;

        Ok(Runtime {
            housekeepers,
            workers,
            stall_soft: Duration::from_micros(cfg.stall.soft_us),
            stall_hard: Duration::from_micros(cfg.stall.hard_us),
            wal,
        })
    }

    pub fn housekeepers(&self) -> &[Housekeeper] {
        &self.housekeepers
    }

    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }

    /// One housekeeper-style stall sample over every worker reactor (A15),
    /// returning `(soft, hard)` totals across all reactors. The tick loop
    /// that calls this periodically lands with the housekeeper (M4+); this
    /// method is the poll surface today.
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
}

/// FR-3: in `physical` SMT mode (the default) one worker per *physical* core
/// — HT siblings are excluded from every role, so a config assigning two
/// sibling cores anywhere refuses boot. `logical` mode has no such rule.
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
            if topology.thread_siblings(a).contains(&b) {
                return Err(BootError::SmtSiblingsAssigned { a, b });
            }
        }
    }
    Ok(())
}

/// Spawn one `std::thread` per housekeeper core (R5); each pins itself and
/// reports. Boot fails unless every report arrives within the deadline and
/// matches its core. When a listener is given, the *first* housekeeper core
/// runs the router loop (accept → auth → handoff); the rest park.
fn spawn_housekeepers(
    cfg: &RuntimeConfig,
    listener: Option<crate::accept::AcceptLoop>,
    auth: AuthConfig,
    senders: Vec<SharedSender<Handoff>>,
    counters: Vec<Arc<AtomicU64>>,
) -> Result<Vec<Housekeeper>, BootError> {
    let (tx, rx) = mpsc::channel();
    let mut out = Vec::new();
    let max_conns = cfg.max_conns_per_worker;
    let mut serving = listener.map(|l| (l, auth, senders, counters));
    for &core in &cfg.housekeeper_cores {
        let tx = tx.clone();
        let body: Box<dyn FnOnce() + Send> = match serving.take() {
            Some((loop_, auth, senders, counters)) => Box::new(move || {
                // Pin first; then accept (blocking — sleeps between
                // events, FR-5) → authenticate → hand off.
                let _ = tx.send((core, pin::pin_current(core)));
                Router::serve(loop_, auth, senders, counters, max_conns);
            }),
            None => Box::new(move || {
                let _ = tx.send((core, pin::pin_current(core)));
                // Background ticks land later; park — no busy-wait (FR-5).
                loop {
                    std::thread::park();
                }
            }),
        };
        let join = std::thread::Builder::new()
            .name(format!("cascade-hk-{core}"))
            .spawn(body)
            .map_err(|e| BootError::HousekeeperSpawn {
                core,
                message: e.to_string(),
            })?;
        out.push(Housekeeper { core, join });
    }
    drop(tx); // so a dead thread's disconnect is observable
    confirm_pins(rx, &cfg.housekeeper_cores, |cores, deadline_secs| {
        BootError::HousekeeperUnconfirmed {
            cores,
            deadline_secs,
        }
    })?;
    Ok(out)
}

/// Spawn one glommio reactor per worker core, pinned via `Placement::Fixed`.
/// The reactor verifies its own affinity in-task (FR-2), connects its
/// handoff-channel receiver, and adopts each handed-off fd onto its own ring
/// (FR-13), where it becomes a Tier A session (FR-14) running the
/// simple-query loop (FR-16). The debug table is per-reactor: all sessions
/// on this worker share one `Rc<Table>` — same-thread sharing only, which is
/// exactly what the `Rc` inside `Session` enforces.
fn spawn_workers(
    cfg: &RuntimeConfig,
    receivers: Vec<SharedReceiver<Handoff>>,
    counters: &[Arc<AtomicU64>],
    tenant_counters: &[Arc<TenantCounters>],
    stalls: &[Arc<ReactorStall>],
    wal: Option<&Arc<WalWriter>>,
    batch_rows: u32,
) -> Result<Vec<Worker>, BootError> {
    let (tx, rx) = mpsc::channel();
    let mut out = Vec::new();
    let per_core: Vec<(u32, u32)> = cfg
        .worker_cores
        .iter()
        .flat_map(|w| w.cores.iter().map(move |&c| (w.numa, c)))
        .collect();
    for ((((numa, core), rx), counter), (tenant_counters, stall)) in per_core
        .into_iter()
        .zip(receivers)
        .zip(counters.iter())
        .zip(tenant_counters.iter().zip(stalls.iter()))
    {
        let tx = tx.clone();
        let task_counter = Arc::clone(counter);
        let task_tenants = Arc::clone(tenant_counters);
        let task_stall = Arc::clone(stall);
        let task_writer = wal.map(Arc::clone);
        let task_wal_dir = cfg.wal_dir.clone();
        let deadline = Duration::from_micros(cfg.yield_deadline_us);
        let join = LocalExecutorBuilder::new(Placement::Fixed(core as usize))
            .name(&format!("cascade-w-{core}"))
            .spawn(move || async move {
                let _ = tx.send((core, pin::verify_core(core)));
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
                let table = Rc::new(crate::session::Table::new());
                while let Some(handoff) = handoffs.recv().await {
                    let counter = task_counter.clone();
                    let tenants = task_tenants.clone();
                    let table = Rc::clone(&table);
                    let wal = wal.as_ref().map(|(writer, handle)| crate::session::Wal {
                        writer: Arc::clone(writer),
                        handle: Rc::clone(handle),
                    });
                    glommio::spawn_local(crate::session::run(
                        handoff, table, counter, tenants, wal, batch_rows,
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
            counter: Arc::clone(counter),
            tenant_counters: Arc::clone(tenant_counters),
            stall: Arc::clone(stall),
            join,
        });
    }
    drop(tx);
    confirm_pins(
        rx,
        &cfg.worker_cores
            .iter()
            .flat_map(|w| w.cores.clone())
            .collect::<Vec<_>>(),
        |cores, deadline_secs| BootError::WorkerUnconfirmed {
            cores,
            deadline_secs,
        },
    )?;
    Ok(out)
}

/// Collect pin reports: every expected core must report `Ok(())` within the
/// deadline. A wrong or missing read-back is a boot failure, not a warning.
fn confirm_pins(
    rx: mpsc::Receiver<(u32, Result<(), PinError>)>,
    expected: &[u32],
    unconfirmed: fn(Vec<u32>, u64) -> BootError,
) -> Result<(), BootError> {
    let mut remaining: Vec<u32> = expected.to_vec();
    let deadline = PIN_CONFIRM_DEADLINE;
    while !remaining.is_empty() {
        match rx.recv_timeout(deadline) {
            Ok((core, Ok(()))) => remaining.retain(|&c| c != core),
            Ok((core, Err(source))) => return Err(BootError::PinVerify { core, source }),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(unconfirmed(remaining, deadline.as_secs()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(unconfirmed(remaining, deadline.as_secs()))
            }
        }
    }
    Ok(())
}
