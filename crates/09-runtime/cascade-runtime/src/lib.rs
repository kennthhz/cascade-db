//! Thread-per-core async scheduler.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

pub mod accept;
pub mod arena;
pub mod boot;
pub mod checkpointer;
pub mod config;
pub mod coop;
pub mod demoheap;
pub mod deployment;
pub mod handoff;
mod mapping;
pub mod orchestrator;
pub mod pg;
pub mod pin;
pub mod recovery;
pub mod pools;
pub mod reaper;
pub mod recycler;
pub mod router;
pub mod scram;
pub mod session;
pub mod shutdown;
pub mod stats;
pub mod submit;
pub mod sql;
pub mod tenant;
pub mod tierc;
pub mod topology;
pub mod walgate;
pub mod walhome;

pub use accept::{AcceptLoop, AcceptStats, AcceptedConn};
pub use arena::{ReactorArena, ARENA_RESERVE_BYTES};
pub use boot::{BootError, Housekeeper, Runtime, Worker};
pub use checkpointer::{
    CheckpointFanout, CheckpointSite, CheckpointStats, FlushReport, TruncatableLsn,
};
pub use config::{ConfigError, NumaAffinity, RuntimeConfig, SmtMode};
pub use recycler::{
    recycle_loop, RecycleInit, RecycleSignal, RecycleSite, RecycleStats, WalRecycler,
};
pub use reaper::{reap_loop, ConnId, IdleReaper, ReapInit, ReapRegistry, ReapStats, ReapTick};
pub use shutdown::{
    drain_loop, run_drain, send_final_flush, DrainMsg, FinalFlushCtx, ServingShutdown,
    ShutdownTrace,
};
pub use stats::{StatsSnapshot, WorkerCounters};
pub use submit::{submit_loop, SubmitError, SubmitHandle, SubmitInit, SubmitJob, SubmitStats};
pub use orchestrator::{Orchestrator, OrchestratorStats, StallWatch, Tick, TickJob};
pub use pools::{NumaPoolHandle, PoolRegistry};
pub use tierc::{Catalog, ClusterSnapshot, Placement};
pub use walhome::{WalHome, WalHomeTable};
pub use topology::{AllowedCpuTopology, SyntheticTopology, TopologyError, TopologyProvider};
