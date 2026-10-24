//! CPU topology discovery (spec §2.2, §2.4; FR-3).
//!
//! T1.1 ships the trait plus two in-memory providers. The real `/sys` parser
//! (T1.2) lands behind the same trait, so boot code never knows the source.

/// Source of truth for what is schedulable on this host.
pub trait TopologyProvider: Send + Sync {
    /// Online (schedulable) CPU ids, ascending.
    fn online_cpus(&self) -> Result<Vec<u32>, TopologyError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TopologyError {
    #[error("topology read failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Fixed-list provider — CI and tests. It asserts nothing about the host:
/// a core it claims online that the kernel refuses to pin simply fails boot
/// at the pin step.
pub struct SyntheticTopology {
    cpus: Vec<u32>,
}

impl SyntheticTopology {
    pub fn new(cpus: impl IntoIterator<Item = u32>) -> Self {
        let mut cpus: Vec<u32> = cpus.into_iter().collect();
        cpus.sort_unstable();
        cpus.dedup();
        Self { cpus }
    }
}

impl TopologyProvider for SyntheticTopology {
    fn online_cpus(&self) -> Result<Vec<u32>, TopologyError> {
        Ok(self.cpus.clone())
    }
}

/// Reports the current process's allowed CPU set. Stand-in for the `/sys`
/// parser on bare metal until T1.2 lands; also the honest source for
/// containers where `/sys` topology may not match the cgroup set.
pub struct AllowedCpuTopology;

impl TopologyProvider for AllowedCpuTopology {
    fn online_cpus(&self) -> Result<Vec<u32>, TopologyError> {
        Ok(crate::pin::current_affinity())
    }
}
