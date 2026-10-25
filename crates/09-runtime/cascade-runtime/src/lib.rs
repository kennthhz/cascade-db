//! Thread-per-core async scheduler.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

pub mod accept;
pub mod arena;
pub mod boot;
pub mod config;
pub mod coop;
pub mod deployment;
pub mod handoff;
mod mapping;
pub mod pg;
pub mod pin;
pub mod pools;
pub mod router;
pub mod scram;
pub mod session;
pub mod sql;
pub mod tenant;
pub mod tierc;
pub mod topology;
pub mod walhome;

pub use accept::{AcceptLoop, AcceptStats, AcceptedConn};
pub use arena::{ReactorArena, ARENA_RESERVE_BYTES};
pub use boot::{BootError, Housekeeper, Runtime, Worker};
pub use config::{ConfigError, NumaAffinity, RuntimeConfig, SmtMode};
pub use pools::{NumaPoolHandle, PoolRegistry};
pub use tierc::{Placement, TierCSnapshot};
pub use walhome::{WalHome, WalHomeTable};
pub use topology::{AllowedCpuTopology, SyntheticTopology, TopologyError, TopologyProvider};
