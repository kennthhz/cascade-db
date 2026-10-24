//! Thread-per-core async scheduler.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

pub mod config;

pub use config::{ConfigError, NumaAffinity, RuntimeConfig, SmtMode};

/// Runtime entry point. Implementation lands in Phase 4.
pub struct Runtime {
    _placeholder: (),
}
