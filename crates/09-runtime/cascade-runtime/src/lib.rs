//! Thread-per-core async scheduler.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

pub mod boot;
pub mod config;
pub mod pin;
pub mod topology;

pub use boot::{BootError, Housekeeper, Runtime, Worker};
pub use config::{ConfigError, NumaAffinity, RuntimeConfig, SmtMode};
pub use topology::{AllowedCpuTopology, SyntheticTopology, TopologyError, TopologyProvider};
