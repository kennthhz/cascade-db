//! Runtime configuration (`[runtime]` table of `cascade.toml`).
//!
//! Surface per implementation plan T0.2 / FR-1: explicit core placement
//! (`housekeeper_cores`, `worker_cores` per NUMA — nothing inferred), SMT mode,
//! and every knob the runtime FRs name. Validation is strict: an invalid config
//! is an error here, so boot (T1.1) can refuse cleanly.
//!
//! Rulings referenced: R6 (explicit placement), R9 (load sampling), R10 (refusal
//! cap), R11 (fan-out wait), R12 (stall thresholds, kill default-off),
//! A6 (ring defaults), A7 (availability cap). TOML layout is this crate's ADR.

use serde::Deserialize;

/// SMT policy — cluster-wide (spec §2.2). Default `physical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtMode {
    /// One worker per physical core; HT siblings idle (spec §2.4).
    Physical,
    /// One worker per vCPU; HT siblings used (spec §2.2).
    Logical,
}

/// Per-database NUMA affinity — cluster default here, per-DB override via DDL
/// (spec §2.5.1). Default `single`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NumaAffinity {
    /// DB bound to one NUMA node; latency-optimized.
    Single,
    /// DB spans all NUMA nodes; throughput-optimized.
    Cross,
}

/// Worker cores for one NUMA node (spec §10.1 step 2: workers grouped by NUMA).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumaWorkers {
    pub numa: u32,
    /// Explicit CPU list — nothing inferred (plan R6).
    pub cores: Vec<u32>,
}

/// Buffer-pool sizing for one NUMA node (spec §8.1: each NUMA's BPM sized
/// independently). A pool may exist without workers on its node — legal, serves
/// `cross`-affinity placement only (plan A18).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumaPool {
    pub numa: u32,
    pub size_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ListenConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self { host: "127.0.0.1".into(), port: 5432 }
    }
}

/// Stall detector thresholds (spec §7.2, plan R12).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StallConfig {
    /// Soft threshold — INFO + `reactor_stall_soft_total` (default 1 ms).
    pub soft_us: u64,
    /// Hard threshold — WARN + `reactor_stall_hard_total` (default 10 ms).
    pub hard_us: u64,
    /// Kill switch — admin opt-in only; rejected by config validation until the
    /// kill wiring exists (default off).
    pub kill: bool,
}

impl Default for StallConfig {
    fn default() -> Self {
        Self { soft_us: 1_000, hard_us: 10_000, kill: false }
    }
}

/// Authentication mode (spec §5.1, plan R2; FR-9). `trust` is for dev — it
/// performs no credential check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Trust,
    ScramSha256,
}

/// One user's SCRAM verifier (A11: verifiers live in config until the
/// catalog's `pg_authid` exists). Format: PG's
/// `SCRAM-SHA-256$<iters>:<salt>$<storedkey>:<serverkey>`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthUser {
    pub name: String,
    pub verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub mode: AuthMode,
    pub users: Vec<AuthUser>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { mode: AuthMode::Trust, users: Vec::new() }
    }
}

/// The `[runtime]` table. Every field has a default except the explicit core
/// placement lists, which fail validation when missing/empty (plan R6).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RuntimeConfig {
    pub topology: TopologyConfig,
    pub default_db_numa_affinity: NumaAffinity,
    pub housekeeper_cores: Vec<u32>,
    pub worker_cores: Vec<NumaWorkers>,
    pub numa_pools: Vec<NumaPool>,
    pub listen: ListenConfig,
    /// Soft CPU deadline for `yield_point()` (spec §7.1, D7; default 500 µs).
    pub yield_deadline_us: u64,
    /// Vectorized batch size (spec §7.1; default 1 024 rows).
    pub batch_rows: u32,
    /// Shutdown drain budget (spec §10.2; default 30 s).
    pub drain_deadline_s: u64,
    /// Per-worker availability cap (spec §5.2, plan A7; default 200).
    pub max_conns_per_worker: u32,
    /// Tier C fan-out bounded wait (spec §8.4, plan R11; default 1 000 ms).
    pub tierc_fanout_wait_ms: u64,
    pub stall: StallConfig,
    pub auth: AuthConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            topology: TopologyConfig::default(),
            default_db_numa_affinity: NumaAffinity::Single,
            housekeeper_cores: vec![0],
            worker_cores: Vec::new(),
            numa_pools: Vec::new(),
            listen: ListenConfig::default(),
            yield_deadline_us: 500,
            batch_rows: 1024,
            drain_deadline_s: 30,
            max_conns_per_worker: 200,
            tierc_fanout_wait_ms: 1000,
            stall: StallConfig::default(),
            auth: AuthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TopologyConfig {
    pub mode: SmtMode,
}

impl Default for TopologyConfig {
    fn default() -> Self {
        Self { mode: SmtMode::Physical }
    }
}

/// A configuration error — distinct from runtime boot failures (T1.1 refuses
/// boot for topology/pinning reasons; this type rejects the *document*).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("runtime.housekeeper_cores must not be empty (spec §2.1: housekeeper cores 0..H)")]
    EmptyHousekeeperCores,
    #[error("runtime.worker_cores must list at least one NUMA with worker cores (plan R6: explicit placement)")]
    EmptyWorkerCores,
    #[error("runtime.worker_cores entry for NUMA {numa} has no cores")]
    WorkerWithEmptyCores { numa: u32 },
    #[error("runtime.worker_cores references NUMA {numa} with no matching runtime.numa_pools entry")]
    MissingPool { numa: u32 },
    #[error("runtime.worker_cores lists NUMA {numa} more than once")]
    DuplicateWorkerNuma { numa: u32 },
    #[error("runtime.numa_pools lists NUMA {numa} more than once")]
    DuplicatePoolNuma { numa: u32 },
    #[error("runtime.numa_pools must list at least one pool")]
    EmptyNumaPools,
    #[error("runtime.housekeeper_cores lists core {core} more than once")]
    DuplicateHousekeeperCore { core: u32 },
    #[error("core {core} is assigned to more than one worker NUMA")]
    DuplicateCore { core: u32 },
    #[error("core {core} is assigned to both housekeeper and worker roles")]
    CoreInBothRoles { core: u32 },
    #[error("runtime.numa_pools entry for NUMA {numa} has size_mb = 0")]
    InvalidBpmSize { numa: u32 },
    #[error("runtime.yield_deadline_us must be > 0")]
    ZeroYieldDeadline,
    #[error("runtime.batch_rows must be > 0")]
    ZeroBatchRows,
    #[error("runtime.drain_deadline_s must be > 0")]
    ZeroDrainDeadline,
    #[error("runtime.max_conns_per_worker must be > 0")]
    ZeroMaxConns,
    #[error("runtime.tierc_fanout_wait_ms must be > 0")]
    ZeroFanoutWait,
    #[error("runtime.stall.soft_us ({soft_us}) must be <= stall.hard_us ({hard_us}) and both > 0")]
    StallThresholds { soft_us: u64, hard_us: u64 },
    #[error("runtime.stall.kill = true is not supported until kill wiring lands; leave it off (plan R12: default off)")]
    KillUnsupported,
    #[error("runtime.listen.port must be > 0")]
    InvalidPort,
    #[error("runtime.auth mode scram_sha256 requires at least one runtime.auth.users entry")]
    ScramWithoutUsers,
    #[error("runtime.auth.users names user {name:?} more than once")]
    DuplicateAuthUser { name: String },
    #[error("config has no [runtime] table")]
    MissingRuntimeTable,
    #[error("invalid config: {0}")]
    Parse(String),
}

impl RuntimeConfig {
    /// Parse a full `cascade.toml` document, taking the `[runtime]` table.
    pub fn from_toml_str(doc: &str) -> Result<Self, ConfigError> {
        let table: toml::Table = toml::from_str(doc)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        let section = table.get("runtime").ok_or(ConfigError::MissingRuntimeTable)?;
        let cfg: RuntimeConfig = section.clone().try_into().map_err(|e: toml::de::Error| {
            ConfigError::Parse(e.to_string())
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Enforce every rule the FRs need. Missing/empty placement lists are errors
    /// here, so boot (FR-1) never has to guess.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.housekeeper_cores.is_empty() {
            return Err(ConfigError::EmptyHousekeeperCores);
        }
        let mut seen_hk = Vec::new();
        for &core in &self.housekeeper_cores {
            if seen_hk.contains(&core) {
                return Err(ConfigError::DuplicateHousekeeperCore { core });
            }
            seen_hk.push(core);
        }
        if self.worker_cores.is_empty() {
            return Err(ConfigError::EmptyWorkerCores);
        }
        if self.numa_pools.is_empty() {
            return Err(ConfigError::EmptyNumaPools);
        }
        if self.listen.port == 0 {
            return Err(ConfigError::InvalidPort);
        }
        if self.yield_deadline_us == 0 {
            return Err(ConfigError::ZeroYieldDeadline);
        }
        if self.batch_rows == 0 {
            return Err(ConfigError::ZeroBatchRows);
        }
        if self.drain_deadline_s == 0 {
            return Err(ConfigError::ZeroDrainDeadline);
        }
        if self.max_conns_per_worker == 0 {
            return Err(ConfigError::ZeroMaxConns);
        }
        if self.tierc_fanout_wait_ms == 0 {
            return Err(ConfigError::ZeroFanoutWait);
        }
        if self.stall.soft_us == 0 || self.stall.hard_us == 0 || self.stall.soft_us > self.stall.hard_us {
            return Err(ConfigError::StallThresholds {
                soft_us: self.stall.soft_us,
                hard_us: self.stall.hard_us,
            });
        }
        if self.stall.kill {
            return Err(ConfigError::KillUnsupported);
        }

        let mut seen_pools = Vec::new();
        for pool in &self.numa_pools {
            if pool.size_mb == 0 {
                return Err(ConfigError::InvalidBpmSize { numa: pool.numa });
            }
            if seen_pools.contains(&pool.numa) {
                return Err(ConfigError::DuplicatePoolNuma { numa: pool.numa });
            }
            seen_pools.push(pool.numa);
        }

        let mut seen_cores = Vec::new();
        let mut seen_numas = Vec::new();
        for workers in &self.worker_cores {
            if workers.cores.is_empty() {
                return Err(ConfigError::WorkerWithEmptyCores { numa: workers.numa });
            }
            if !seen_pools.contains(&workers.numa) {
                return Err(ConfigError::MissingPool { numa: workers.numa });
            }
            if seen_numas.contains(&workers.numa) {
                return Err(ConfigError::DuplicateWorkerNuma { numa: workers.numa });
            }
            seen_numas.push(workers.numa);
            for &core in &workers.cores {
                if seen_cores.contains(&core) {
                    return Err(ConfigError::DuplicateCore { core });
                }
                if self.housekeeper_cores.contains(&core) {
                    return Err(ConfigError::CoreInBothRoles { core });
                }
                seen_cores.push(core);
            }
        }
        if self.auth.mode == AuthMode::ScramSha256 {
            if self.auth.users.is_empty() {
                return Err(ConfigError::ScramWithoutUsers);
            }
            let mut seen_users = Vec::new();
            for user in &self.auth.users {
                if seen_users.contains(&user.name) {
                    return Err(ConfigError::DuplicateAuthUser { name: user.name.clone() });
                }
                seen_users.push(user.name.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[runtime]
[[runtime.worker_cores]]
numa = 0
cores = [2, 3]
[[runtime.numa_pools]]
numa = 0
size_mb = 1024
"#;

    #[test]
    fn minimal_config_parses_with_defaults() {
        let cfg = RuntimeConfig::from_toml_str(MINIMAL).expect("minimal config is valid");
        assert_eq!(cfg.topology.mode, SmtMode::Physical);
        assert_eq!(cfg.default_db_numa_affinity, NumaAffinity::Single);
        assert_eq!(cfg.housekeeper_cores, vec![0]);
        assert_eq!(cfg.listen.host, "127.0.0.1");
        assert_eq!(cfg.listen.port, 5432);
        assert_eq!(cfg.yield_deadline_us, 500);
        assert_eq!(cfg.batch_rows, 1024);
        assert_eq!(cfg.drain_deadline_s, 30);
        assert_eq!(cfg.max_conns_per_worker, 200);
        assert_eq!(cfg.tierc_fanout_wait_ms, 1000);
        assert_eq!(cfg.stall.soft_us, 1_000);
        assert_eq!(cfg.stall.hard_us, 10_000);
        assert!(!cfg.stall.kill);
    }

    #[test]
    fn full_explicit_config_parses() {
        let cfg = RuntimeConfig::from_toml_str(
            r#"
[runtime]
default_db_numa_affinity = "cross"
housekeeper_cores = [0, 1]
yield_deadline_us = 250
batch_rows = 512
drain_deadline_s = 10
max_conns_per_worker = 50
tierc_fanout_wait_ms = 250
[runtime.topology]
mode = "logical"
[runtime.listen]
host = "0.0.0.0"
port = 6543
[runtime.stall]
soft_us = 500
hard_us = 5_000
[[runtime.worker_cores]]
numa = 0
cores = [2, 4]
[[runtime.worker_cores]]
numa = 1
cores = [8, 10]
[[runtime.numa_pools]]
numa = 0
size_mb = 2048
[[runtime.numa_pools]]
numa = 1
size_mb = 4096
"#,
        )
        .expect("full config is valid");
        assert_eq!(cfg.topology.mode, SmtMode::Logical);
        assert_eq!(cfg.default_db_numa_affinity, NumaAffinity::Cross);
        assert_eq!(cfg.worker_cores.len(), 2);
        assert_eq!(cfg.numa_pools.len(), 2);
        assert_eq!(cfg.listen.port, 6543);
        assert_eq!(cfg.yield_deadline_us, 250);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = RuntimeConfig::from_toml_str(
            r#"
[runtime]
wat = 1
[[runtime.worker_cores]]
numa = 0
cores = [2]
[[runtime.numa_pools]]
numa = 0
size_mb = 1
"#,
        )
        .expect_err("unknown key must be rejected");
        assert!(err.to_string().contains("unknown field"), "got: {err}");
    }

    #[test]
    fn missing_runtime_table_is_rejected() {
        let err = RuntimeConfig::from_toml_str("[other]\nx = 1\n")
            .expect_err("missing [runtime] must be rejected");
        assert_eq!(err, ConfigError::MissingRuntimeTable);
    }

    fn assert_err(cfg: &str, expected: &ConfigError) {
        let err = RuntimeConfig::from_toml_str(cfg).expect_err("config must be rejected");
        assert_eq!(err, *expected, "got: {err}");
    }

    fn with_workers(override_table: &str) -> String {
        format!("[runtime]\n{override_table}\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n")
    }

    #[test]
    fn empty_numa_pools_rejected() {
        assert_err(
            "[runtime]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n",
            &ConfigError::EmptyNumaPools,
        );
    }

    #[test]
    fn duplicate_housekeeper_core_rejected() {
        assert_err(
            &with_workers("housekeeper_cores = [0, 0]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n"),
            &ConfigError::DuplicateHousekeeperCore { core: 0 },
        );
    }

    #[test]
    fn empty_housekeeper_cores_rejected() {
        assert_err(
            &with_workers("housekeeper_cores = []\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n"),
            &ConfigError::EmptyHousekeeperCores,
        );
    }

    #[test]
    fn missing_worker_cores_rejected() {
        assert_err("[runtime]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n", &ConfigError::EmptyWorkerCores);
    }

    #[test]
    fn worker_with_empty_cores_rejected() {
        assert_err(
            &with_workers("[[runtime.worker_cores]]\nnuma = 0\ncores = []\n"),
            &ConfigError::WorkerWithEmptyCores { numa: 0 },
        );
    }

    #[test]
    fn worker_numa_without_pool_rejected() {
        assert_err(
            "[runtime]\n[[runtime.worker_cores]]\nnuma = 3\ncores = [2]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n",
            &ConfigError::MissingPool { numa: 3 },
        );
    }

    #[test]
    fn duplicate_worker_numa_rejected() {
        assert_err(
            "[runtime]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [3]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n",
            &ConfigError::DuplicateWorkerNuma { numa: 0 },
        );
    }

    #[test]
    fn duplicate_pool_numa_rejected() {
        assert_err(
            "[runtime]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 2\n",
            &ConfigError::DuplicatePoolNuma { numa: 0 },
        );
    }

    #[test]
    fn duplicate_core_across_numas_rejected() {
        assert_err(
            "[runtime]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n[[runtime.worker_cores]]\nnuma = 1\ncores = [2]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n[[runtime.numa_pools]]\nnuma = 1\nsize_mb = 1\n",
            &ConfigError::DuplicateCore { core: 2 },
        );
    }

    #[test]
    fn core_in_both_roles_rejected() {
        assert_err(
            "[runtime]\nhousekeeper_cores = [0, 2]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n",
            &ConfigError::CoreInBothRoles { core: 2 },
        );
    }

    #[test]
    fn zero_bpm_size_rejected() {
        assert_err(
            "[runtime]\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 0\n",
            &ConfigError::InvalidBpmSize { numa: 0 },
        );
    }

    #[test]
    fn zero_positive_scalars_rejected() {
        for (key, expected) in [
            ("yield_deadline_us", &ConfigError::ZeroYieldDeadline),
            ("batch_rows", &ConfigError::ZeroBatchRows),
            ("drain_deadline_s", &ConfigError::ZeroDrainDeadline),
            ("max_conns_per_worker", &ConfigError::ZeroMaxConns),
            ("tierc_fanout_wait_ms", &ConfigError::ZeroFanoutWait),
        ] {
            let cfg = with_workers(&format!("{key} = 0\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n"));
            assert_err(&cfg, expected);
        }
    }

    #[test]
    fn port_zero_rejected() {
        assert_err(
            &with_workers("listen.port = 0\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n"),
            &ConfigError::InvalidPort,
        );
    }

    #[test]
    fn soft_above_hard_rejected() {
        assert_err(
            &with_workers("stall.soft_us = 20_000\nstall.hard_us = 10_000\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n"),
            &ConfigError::StallThresholds { soft_us: 20_000, hard_us: 10_000 },
        );
    }

    #[test]
    fn kill_switch_rejected_until_wiring_exists() {
        assert_err(
            &with_workers("stall.kill = true\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n"),
            &ConfigError::KillUnsupported,
        );
    }
}
