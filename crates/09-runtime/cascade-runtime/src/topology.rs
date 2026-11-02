//! CPU topology discovery (spec §2.2, §2.4; FR-3).
//!
//! Three providers behind one trait, so boot never knows the source:
//! [`SysTopology`] parses `/sys` (no C deps — plan R4), [`SyntheticTopology`]
//! serves a fixed layout for CI, [`AllowedCpuTopology`] reports the process's
//! own CPU set for containers where `/sys` doesn't match the cgroup mask.
//!
//! Degradation is deliberate and honest: absent per-CPU topology files yield
//! "no siblings"; an absent `node/` tree (e.g. WSL2) yields "NUMA unknown".
//! A file that *exists but doesn't parse* is an error — kernel truth must not
//! be guessed at.

use std::{collections::HashMap, path::Path};

/// Source of truth for what is schedulable on this host.
pub trait TopologyProvider: Send + Sync {
    /// Online (schedulable) CPU ids, ascending.
    fn online_cpus(&self) -> Result<Vec<u32>, TopologyError>;
    /// SMT siblings of `core`, including `core` itself; empty if unknown.
    fn thread_siblings(&self, core: u32) -> Vec<u32>;
    /// NUMA node hosting `core`, if the platform exposes one.
    fn numa_node(&self, core: u32) -> Option<u32>;
}

#[derive(Debug, thiserror::Error)]
pub enum TopologyError {
    #[error("topology read failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed cpu list {path:?}: {detail}")]
    Parse { path: String, detail: String },
}

/// `SysTopology` root: the directory holding the `cpu/` and `node/` subtrees
/// (on Linux: `/sys/devices/system`).
pub const SYS_DEVICES_SYSTEM: &str = "/sys/devices/system";

/// Parse a kernel cpu list — "0-3,8,10-11" → [0,1,2,3,8,10,11].
pub fn parse_cpu_list(s: &str) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((lo, hi)) => {
                let lo: u32 = lo
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad range start {part:?}"))?;
                let hi: u32 = hi
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad range end {part:?}"))?;
                if hi < lo {
                    return Err(format!("inverted range {part:?}"));
                }
                // No Linux box has more than ~16k CPUs; a wider range means
                // the file is garbage and expanding it would eat memory.
                if hi - lo > 65_535 {
                    return Err(format!("implausible range {part:?}"));
                }
                out.extend(lo..=hi);
            }
            None => out.push(part.parse().map_err(|_| format!("bad cpu id {part:?}"))?),
        }
    }
    Ok(out)
}

fn read_trimmed(path: &Path) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_owned())
}

/// Real topology from the `/sys` filesystem (plan R4: hand-rolled, no C deps).
#[derive(Debug, Clone, Default)]
pub struct SysTopology {
    online: Vec<u32>,
    /// cpu → SMT sibling set (includes the cpu itself).
    siblings: HashMap<u32, Vec<u32>>,
    /// cpu → NUMA node; absent when the platform exposes no node tree.
    numa: HashMap<u32, u32>,
}

impl SysTopology {
    /// Probe the real host.
    pub fn probe() -> Result<Self, TopologyError> {
        Self::from_root(Path::new(SYS_DEVICES_SYSTEM))
    }

    /// Build from a sysfs-shaped root — `root/cpu/online`,
    /// `root/cpu/cpuN/topology/thread_siblings_list`, `root/node/nodeN/cpulist`.
    /// Fixture trees for CI take this path.
    pub fn from_root(root: &Path) -> Result<Self, TopologyError> {
        let online_path = root.join("cpu/online");
        let online = parse_cpu_list(
            &read_trimmed(&online_path)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        std::io::Error::new(
                            e.kind(),
                            format!("{} missing: not a Linux sysfs root", online_path.display()),
                        )
                    } else {
                        e
                    }
                })
                .map_err(TopologyError::Io)?,
        )
        .map_err(|detail| TopologyError::Parse {
            path: online_path.display().to_string(),
            detail,
        })?;

        // Per-CPU sibling sets. A cpu whose topology files are absent (some
        // VMs expose only the online mask) degrades to "its own sibling".
        let mut siblings = HashMap::new();
        for &cpu in &online {
            let list = root.join(format!("cpu/cpu{cpu}/topology/thread_siblings_list"));
            let set = match read_trimmed(&list) {
                Ok(s) => parse_cpu_list(&s).map_err(|detail| TopologyError::Parse {
                    path: list.display().to_string(),
                    detail,
                })?,
                Err(_) => vec![cpu],
            };
            siblings.insert(cpu, set);
        }

        // NUMA map. Absent `node/` tree (WSL2, some VMs) → NUMA unknown.
        let mut numa = HashMap::new();
        let node_dir = root.join("node");
        if node_dir.is_dir() {
            for entry in std::fs::read_dir(&node_dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(node_id) = name
                    .strip_prefix("node")
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    continue;
                };
                let list = entry.path().join("cpulist");
                let Ok(text) = read_trimmed(&list) else {
                    continue;
                };
                let cpus = parse_cpu_list(&text).map_err(|detail| TopologyError::Parse {
                    path: list.display().to_string(),
                    detail,
                })?;
                for cpu in cpus {
                    numa.insert(cpu, node_id);
                }
            }
        }

        Ok(Self {
            online,
            siblings,
            numa,
        })
    }
}

impl TopologyProvider for SysTopology {
    fn online_cpus(&self) -> Result<Vec<u32>, TopologyError> {
        Ok(self.online.clone())
    }

    fn thread_siblings(&self, core: u32) -> Vec<u32> {
        self.siblings.get(&core).cloned().unwrap_or_default()
    }

    fn numa_node(&self, core: u32) -> Option<u32> {
        self.numa.get(&core).copied()
    }
}

/// Fixed-list provider — CI and tests. Siblings/NUMA default to "each core
/// alone / node 0" and can be set explicitly. It asserts nothing about the
/// host: a core it claims online that the kernel refuses to pin simply fails
/// boot at the pin step.
pub struct SyntheticTopology {
    cpus: Vec<u32>,
    siblings: HashMap<u32, Vec<u32>>,
    numa: HashMap<u32, u32>,
}

impl SyntheticTopology {
    pub fn new(cpus: impl IntoIterator<Item = u32>) -> Self {
        let mut cpus: Vec<u32> = cpus.into_iter().collect();
        cpus.sort_unstable();
        cpus.dedup();
        Self {
            siblings: cpus.iter().map(|&c| (c, vec![c])).collect(),
            numa: cpus.iter().map(|&c| (c, 0)).collect(),
            cpus,
        }
    }

    /// Declare `a` and `b` SMT siblings of each other.
    pub fn with_siblings(mut self, a: u32, b: u32) -> Self {
        self.siblings.entry(a).or_insert_with(|| vec![a]).push(b);
        self.siblings.entry(b).or_insert_with(|| vec![b]).push(a);
        self
    }

    /// Place `cpu` on NUMA `node`.
    pub fn with_numa(mut self, cpu: u32, node: u32) -> Self {
        self.numa.insert(cpu, node);
        self
    }
}

impl TopologyProvider for SyntheticTopology {
    fn online_cpus(&self) -> Result<Vec<u32>, TopologyError> {
        Ok(self.cpus.clone())
    }

    fn thread_siblings(&self, core: u32) -> Vec<u32> {
        self.siblings.get(&core).cloned().unwrap_or_default()
    }

    fn numa_node(&self, core: u32) -> Option<u32> {
        self.numa.get(&core).copied()
    }
}

/// Reports the current process's allowed CPU set. The honest source for
/// containers where `/sys` topology may not match the cgroup set; sibling
/// and NUMA facts degrade to "unknown" rather than guessed.
pub struct AllowedCpuTopology;

impl TopologyProvider for AllowedCpuTopology {
    fn online_cpus(&self) -> Result<Vec<u32>, TopologyError> {
        Ok(crate::pin::current_affinity())
    }

    fn thread_siblings(&self, core: u32) -> Vec<u32> {
        vec![core]
    }

    fn numa_node(&self, _core: u32) -> Option<u32> {
        None
    }
}

/// Sysfs root for NIC discovery — factored for the unit test, which builds
/// the same shape under a temp root.
pub const NET_DEVICES_ROOT: &str = "/sys/class/net";

/// The NIC's NUMA node for the §5.2 composite's proximity tie-breaker
/// (T5.6, FR-11) — the node whose memory and driver queues the NIC's PCI
/// device is local to. Discovered from sysfs, no C deps (plan R4):
/// the first interface (sorted by name, so the pick is deterministic)
/// whose `device/numa_node` exists and reports a real node (>= 0).
/// `None` when nothing is discoverable — loopback-only hosts (the usual
/// test rig; loopback has no PCI device at all) and NICs whose firmware
/// reports `-1` — and the tie-breaker is then inert, exactly like an
/// unset `nic_numa` config.
pub fn discover_nic_numa(root: &str) -> Option<u32> {
    let mut ifaces: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    ifaces.sort();
    for iface in ifaces {
        let path = std::path::Path::new(root)
            .join(iface)
            .join("device/numa_node");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // virtual interface, or no PCI device behind it
        };
        match text.trim().parse::<i64>() {
            // -1 is the kernel's "unknown node" — a discovery miss, not
            // node 0; keep looking (a later interface may report one).
            Ok(node) if node >= 0 => return Some(node as u32),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod nic_tests {
    use super::*;

    /// Discovery walks the sorted interfaces, skips virtual ones (no
    /// `device/numa_node`) and unknown-node ones (`-1`), and takes the
    /// first real node — deterministically, whatever the OS's
    /// directory order.
    #[test]
    fn nic_discovery_skips_virtual_and_unknown_takes_the_first_real_node() {
        let root = std::env::temp_dir().join(format!("cascade-nic-{}", std::process::id()));
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        // eth0: virtual (no device link at all — just the interface dir).
        // lo: loopback (same). eth1: PCI device whose firmware reports no
        // node (-1). eth2: a real node 0. eth3: a real node 1 — must NOT
        // win; the sort put eth2 first.
        std::fs::create_dir_all(root.join("eth0")).unwrap();
        std::fs::create_dir_all(root.join("lo")).unwrap();
        write("eth1/device/numa_node", "-1\n");
        write("eth2/device/numa_node", "0\n");
        write("eth3/device/numa_node", "1\n");
        assert_eq!(discover_nic_numa(root.to_str().unwrap()), Some(0));
        // Only an unknown-node NIC: no discovery, inert tie-breaker.
        let only_unknown = root.join("only-unknown");
        std::fs::create_dir_all(only_unknown.join("device")).unwrap();
        std::fs::write(only_unknown.join("device/numa_node"), "-1\n").unwrap();
        assert_eq!(discover_nic_numa(only_unknown.to_str().unwrap()), None);
        // A missing root is the same honest miss (no panic, no guess).
        assert_eq!(discover_nic_numa("/nonexistent-cascade-nic"), None);
        std::fs::remove_dir_all(&root).ok();
    }
}
