//! Thread-per-core async scheduler.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

pub mod accept;
pub mod boot;
pub mod config;
pub mod coop;
pub mod handoff;
pub mod pg;
pub mod pin;
pub mod router;
pub mod scram;
pub mod session;
pub mod sql;
pub mod tenant;
pub mod topology;

pub use accept::{AcceptLoop, AcceptStats, AcceptedConn};
pub use boot::{BootError, Housekeeper, Runtime, Worker};
pub use config::{ConfigError, NumaAffinity, RuntimeConfig, SmtMode};
pub use topology::{AllowedCpuTopology, SyntheticTopology, TopologyError, TopologyProvider};
