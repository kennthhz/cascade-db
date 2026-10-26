//! Kernel-deployment verification (T2.7; FR-4, §2.4).
//!
//! §2.4's CPU isolation is a *provisioning-time* requirement — the kernel
//! cmdline lists (`isolcpus` / `nohz_full` / `rcu_nocbs` = workers,
//! `irqaffinity` = housekeepers) must exist before the kernel boots, so
//! Cascade cannot set them; it can only verify. At startup boot reads the
//! kernel's own cmdline (`/proc/cmdline` — the kernel's view, never the
//! config's), compares it against the configured core sets, and logs a
//! **loud warning per mismatch — never a refusal** (FR-4). A config whose
//! workers are not isolated still boots; it just says so.
//!
//! Parsing honesty: CPU lists are comma-separated items and `a-b` ranges.
//! `isolcpus` also accepts flag tokens (`nohz`, `domain`, `managed_irq`,
//! …) — non-numeric items are skipped, which is exactly right for this
//! comparison (the isolated *cores* are the numeric items). The last
//! occurrence of a parameter wins, as in the kernel. A pathologically
//! huge range (> 2²⁰ cpus) is skipped rather than iterated — no real
//! kernel list looks like that.

use std::collections::BTreeSet;

use cascade_telemetry::Level;

use crate::config::RuntimeConfig;

/// The kernel's declared CPU-isolation deployment, as parsed from its
/// cmdline. Sorted sets, so mismatch reports are deterministic.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KernelDeployment {
    pub isolcpus: BTreeSet<u32>,
    pub nohz_full: BTreeSet<u32>,
    pub rcu_nocbs: BTreeSet<u32>,
    pub irqaffinity: BTreeSet<u32>,
}

/// One parameter whose kernel set does not cover the configured cores of
/// its role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The cmdline parameter (`isolcpus`, `nohz_full`, `rcu_nocbs`,
    /// `irqaffinity`).
    pub param: &'static str,
    /// The configured role whose cores are not covered (`worker`,
    /// `housekeeper`).
    pub role: &'static str,
    /// The configured cores the kernel deployment does not cover.
    pub cores: Vec<u32>,
}

/// Parses one kernel CPU list: numeric items and `a-b` ranges; anything
/// else (isolcpus flag syntax) is skipped.
fn parse_cpu_list(value: &str) -> BTreeSet<u32> {
    const MAX_RANGE: u32 = 1 << 20;
    let mut out = BTreeSet::new();
    for item in value.split(',') {
        match item.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                    if b >= a && b - a <= MAX_RANGE {
                        out.extend(a..=b);
                    }
                }
            }
            None => {
                if let Ok(cpu) = item.trim().parse() {
                    out.insert(cpu);
                }
            }
        }
    }
    out
}

/// Parses a kernel cmdline (space-separated `key=value` tokens, as
/// `/proc/cmdline` renders it). The last occurrence of each tracked
/// parameter wins.
pub fn parse_cmdline(text: &str) -> KernelDeployment {
    let mut d = KernelDeployment::default();
    for token in text.split_whitespace() {
        if let Some(v) = token.strip_prefix("isolcpus=") {
            d.isolcpus = parse_cpu_list(v);
        } else if let Some(v) = token.strip_prefix("nohz_full=") {
            d.nohz_full = parse_cpu_list(v);
        } else if let Some(v) = token.strip_prefix("rcu_nocbs=") {
            d.rcu_nocbs = parse_cpu_list(v);
        } else if let Some(v) = token.strip_prefix("irqaffinity=") {
            d.irqaffinity = parse_cpu_list(v);
        }
    }
    d
}

/// FR-4's comparison: every configured worker core must appear in
/// `isolcpus`, `nohz_full`, and `rcu_nocbs`; every housekeeper core in
/// `irqaffinity` (§2.4). Returns one [`Mismatch`] per uncovered parameter.
pub fn check_deployment(cfg: &RuntimeConfig, kernel: &KernelDeployment) -> Vec<Mismatch> {
    let workers: BTreeSet<u32> = cfg
        .worker_cores
        .iter()
        .flat_map(|w| w.cores.iter().copied())
        .collect();
    let housekeepers: BTreeSet<u32> = cfg.housekeeper_cores.iter().copied().collect();
    let mut out = Vec::new();
    for (param, set, role, need) in [
        ("isolcpus", &kernel.isolcpus, "worker", &workers),
        ("nohz_full", &kernel.nohz_full, "worker", &workers),
        ("rcu_nocbs", &kernel.rcu_nocbs, "worker", &workers),
        ("irqaffinity", &kernel.irqaffinity, "housekeeper", &housekeepers),
    ] {
        let missing: Vec<u32> = need.iter().filter(|c| !set.contains(c)).copied().collect();
        if !missing.is_empty() {
            out.push(Mismatch {
                param,
                role,
                cores: missing,
            });
        }
    }
    out
}

/// Boot's wiring: verify against the kernel cmdline and log every mismatch
/// at [`Level::Warn`] — loud, structured, and *advisory* (FR-4: warning,
/// not refusal). `None` (unreadable cmdline) logs an Info note instead of
/// pretending to have verified. Returns the mismatches for tests.
pub fn verify_and_log(cfg: &RuntimeConfig, cmdline: Option<&str>) -> Vec<Mismatch> {
    let Some(text) = cmdline else {
        cascade_telemetry::log(
            Level::Info,
            "kernel_deployment_unverified",
            &[("reason", "kernel cmdline unreadable")],
        );
        return Vec::new();
    };
    let mismatches = check_deployment(cfg, &parse_cmdline(text));
    for m in &mismatches {
        let cores = m
            .cores
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        cascade_telemetry::log(
            Level::Warn,
            "kernel_deployment_mismatch",
            &[
                ("param", m.param),
                ("role", m.role),
                ("cores", cores.as_str()),
                ("policy", "warn-only (provisioning-time requirement, §2.4)"),
            ],
        );
    }
    mismatches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(hk: &[u32], workers: &[u32]) -> RuntimeConfig {
        use crate::config::{NumaPool, NumaWorkers};
        RuntimeConfig {
            housekeeper_cores: hk.to_vec(),
            worker_cores: vec![NumaWorkers {
                numa: 0,
                cores: workers.to_vec(),
            }],
            numa_pools: vec![NumaPool { numa: 0, size_mb: 1 }],
            ..RuntimeConfig::default()
        }
    }

    #[test]
    fn cmdline_parses_items_ranges_flags_and_last_wins() {
        let d = parse_cmdline(
            "quiet BOOT_IMAGE=/vmlinuz isolcpus=2,3,8-11 nohz_full=2-3 rcu_nocbs=2,3 \
             irqaffinity=0,1 isolcpus=domain,managed_irq,4-5",
        );
        // Flag syntax: non-numeric items skipped, the numeric core list kept;
        // the second isolcpus wins (last occurrence).
        assert_eq!(d.isolcpus, [4, 5].into_iter().collect());
        assert_eq!(d.nohz_full, [2, 3].into_iter().collect());
        assert_eq!(d.rcu_nocbs, [2, 3].into_iter().collect());
        assert_eq!(d.irqaffinity, [0, 1].into_iter().collect());
    }

    #[test]
    fn absent_or_empty_params_are_empty_sets() {
        let d = parse_cmdline("isolcpus= nohz_full= quiet");
        assert_eq!(d, KernelDeployment::default());
    }

    #[test]
    fn a_full_match_produces_no_mismatch() {
        let cfg = cfg(&[0, 1], &[2, 3, 4]);
        let kernel = parse_cmdline("isolcpus=2-4 nohz_full=2,3,4 rcu_nocbs=2-4 irqaffinity=0-1");
        assert_eq!(check_deployment(&cfg, &kernel), Vec::new());
    }

    #[test]
    fn a_partial_match_reports_only_the_uncovered_cores() {
        let cfg = cfg(&[0, 1], &[2, 3]);
        // Worker 3 is missing from nohz_full; housekeeper 1 from irqaffinity.
        let kernel = parse_cmdline("isolcpus=2,3 nohz_full=2 rcu_nocbs=2-3 irqaffinity=0");
        assert_eq!(
            check_deployment(&cfg, &kernel),
            vec![
                Mismatch {
                    param: "nohz_full",
                    role: "worker",
                    cores: vec![3],
                },
                Mismatch {
                    param: "irqaffinity",
                    role: "housekeeper",
                    cores: vec![1],
                },
            ]
        );
    }

    #[test]
    fn an_absent_deployment_reports_every_parameter() {
        let cfg = cfg(&[0], &[2, 3]);
        let kernel = parse_cmdline("quiet");
        assert_eq!(check_deployment(&cfg, &kernel).len(), 4);
    }

    #[test]
    fn absurd_ranges_are_skipped_not_iterated() {
        let d = parse_cmdline("isolcpus=0-4294967295,bogus,9");
        assert_eq!(d.isolcpus, [9].into_iter().collect());
        // An inverted range is empty, not a panic.
        let d = parse_cmdline("isolcpus=9-3");
        assert!(d.isolcpus.is_empty());
    }
}
