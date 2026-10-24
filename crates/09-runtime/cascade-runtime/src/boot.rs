//! Boot sequence (spec §10.1; FR-1, FR-2).
//!
//! Order: check every configured core against the topology → spawn
//! housekeepers (plain `std::thread` per R5) → spawn workers (one glommio
//! reactor per core, `Placement::Fixed`) → each thread reports its observed
//! affinity and boot refuses unless every report matches its core. There is
//! no "boot anyway" fallback: an unpinned or unverified thread is a boot
//! failure (FR-1, FR-2).

use std::{
    sync::mpsc,
    time::Duration,
};

use glommio::{ExecutorJoinHandle, LocalExecutorBuilder, Placement};

use crate::{
    config::{RuntimeConfig, SmtMode},
    pin::{self, PinError},
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
}

pub struct Housekeeper {
    pub core: u32,
    join: std::thread::JoinHandle<()>,
}

pub struct Worker {
    pub numa: u32,
    pub core: u32,
    join: ExecutorJoinHandle<()>,
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
            .finish()
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
    PinVerify { core: u32, #[source] source: PinError },
    #[error("housekeeper pins unconfirmed for cores {cores:?} within {deadline_secs:?}")]
    HousekeeperUnconfirmed {
        cores: Vec<u32>,
        deadline_secs: u64,
    },
    #[error("worker pins unconfirmed for cores {cores:?} within {deadline_secs:?}")]
    WorkerUnconfirmed { cores: Vec<u32>, deadline_secs: u64 },
    #[error("physical SMT mode: cores {a} and {b} share a physical core; assign only one of any sibling pair (FR-3, spec §2.4)")]
    SmtSiblingsAssigned { a: u32, b: u32 },
}

impl Runtime {
    /// Boot exactly as configured (FR-1), with every pin verified after it
    /// happens (FR-2). Housekeepers are `std::thread`s (R5); workers are
    /// glommio reactors pinned via `Placement::Fixed` and verified in-task.
    pub fn boot(
        cfg: &RuntimeConfig,
        topology: &dyn TopologyProvider,
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

        let housekeepers = spawn_housekeepers(cfg)?;
        let workers = spawn_workers(cfg)?;

        Ok(Runtime { housekeepers, workers })
    }

    pub fn housekeepers(&self) -> &[Housekeeper] {
        &self.housekeepers
    }

    pub fn workers(&self) -> &[Worker] {
        &self.workers
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
/// matches its core.
fn spawn_housekeepers(cfg: &RuntimeConfig) -> Result<Vec<Housekeeper>, BootError> {
    let (tx, rx) = mpsc::channel();
    let mut out = Vec::new();
    for &core in &cfg.housekeeper_cores {
        let tx = tx.clone();
        let join = std::thread::Builder::new()
            .name(format!("cascade-hk-{core}"))
            .spawn(move || {
                let _ = tx.send((core, pin::pin_current(core)));
                // The accept loop and background ticks land in T1.3+; park
                // until then — no busy-wait (FR-5).
                loop {
                    std::thread::park();
                }
            })
            .map_err(|e| BootError::HousekeeperSpawn {
                core,
                message: e.to_string(),
            })?;
        out.push(Housekeeper { core, join });
    }
    drop(tx); // so a dead thread's disconnect is observable
    confirm_pins(
        rx,
        &cfg.housekeeper_cores,
        |cores, deadline_secs| BootError::HousekeeperUnconfirmed {
            cores,
            deadline_secs,
        },
    )?;
    Ok(out)
}

/// Spawn one glommio reactor per worker core, pinned via `Placement::Fixed`.
/// The reactor verifies its own affinity in-task — boot refuses if the pin
/// did not stick (FR-2) — then parks; the ring stays serviced while parked,
/// which is how accept loops and sessions will live here from T1.3/T1.7.
fn spawn_workers(cfg: &RuntimeConfig) -> Result<Vec<Worker>, BootError> {
    let (tx, rx) = mpsc::channel();
    let mut out = Vec::new();
    for workers in &cfg.worker_cores {
        for &core in &workers.cores {
            let tx = tx.clone();
            let join = LocalExecutorBuilder::new(Placement::Fixed(core as usize))
                .name(&format!("cascade-w-{core}"))
                .spawn(move || async move {
                    let _ = tx.send((core, pin::verify_core(core)));
                    std::future::pending::<()>().await;
                })
                .map_err(|e| BootError::WorkerSpawn {
                    core,
                    message: e.to_string(),
                })?;
            out.push(Worker {
                numa: workers.numa,
                core,
                join,
            });
        }
    }
    drop(tx);
    confirm_pins(
        rx,
        &cfg.worker_cores.iter().flat_map(|w| w.cores.clone()).collect::<Vec<_>>(),
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
