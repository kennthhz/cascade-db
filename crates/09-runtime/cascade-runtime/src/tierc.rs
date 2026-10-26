//! Static Tier C placement snapshot (FR-10; spec §5.2, §8.4, §2.5.1).
//!
//! Routing resolves `DatabaseId → numa_affinity` from *this snapshot*, not
//! from boot config (the T2.4 code-review item): boot builds the snapshot
//! from the cluster default and hands the router an [`Arc`] of it, so the
//! swap to the live Tier C feed (T5.5) is a construction change, not a
//! routing change. Until then the snapshot is immutable after boot.
//!
//! Per-DB rows (`pg_database.datnumaffinity` / `datnumanode`) land with the
//! real catalog; today every database — named or not — inherits the
//! cluster default, which is exactly the inheritance FR-10 requires and
//! the T2.4 test-review's "no explicit affinity" case. A `single` bound
//! NUMA is picked per A13 (least-loaded NUMA at first activation); at boot
//! every NUMA is idle, so the standing proxy is fewest workers, tie-broken
//! by config order.

use crate::config::NumaAffinity;

/// The placement a connection's database resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub affinity: NumaAffinity,
    /// For `single` affinity: the bound NUMA (A13 pick). `None` when the
    /// cluster has no workers at all — routing then has no candidates.
    pub bound_numa: Option<u32>,
}

/// The routing half of the §8.4 Tier C snapshot, as of boot.
#[derive(Debug)]
pub struct TierCSnapshot {
    default_affinity: NumaAffinity,
    /// A13 bound NUMA for default-`single` databases.
    default_bound: Option<u32>,
}

impl TierCSnapshot {
    /// Builds the snapshot from the cluster default and the boot-time
    /// worker layout (numa → worker count, in config order). `single`
    /// binds per A13; `cross` binds nothing.
    pub fn build(default_affinity: NumaAffinity, worker_numas: &[u32]) -> Self {
        let default_bound = match default_affinity {
            NumaAffinity::Single => least_loaded_numa(worker_numas),
            NumaAffinity::Cross => None,
        };
        TierCSnapshot {
            default_affinity,
            default_bound,
        }
    }

    /// FR-10 lookup: the placement for a connection's database name.
    /// With no catalog rows yet, every name inherits the cluster default.
    pub fn resolve(&self, _database: Option<&str>) -> Placement {
        Placement {
            affinity: self.default_affinity,
            bound_numa: self.default_bound,
        }
    }

    /// The cluster default the snapshot was built with (diagnostics).
    pub fn default_affinity(&self) -> NumaAffinity {
        self.default_affinity
    }
}

/// A13's boot-time proxy: the NUMA hosting the fewest workers (most
/// headroom per worker), ties broken by first appearance in config order.
/// `None` when the cluster has no workers (config validation forbids it,
/// but a directly-built config reaches this before `validate`).
fn least_loaded_numa(worker_numas: &[u32]) -> Option<u32> {
    let mut counts: Vec<(u32, u32)> = Vec::new();
    for &numa in worker_numas {
        match counts.iter_mut().find(|(n, _)| *n == numa) {
            Some((_, c)) => *c += 1,
            None => counts.push((numa, 1)),
        }
    }
    counts
        .iter()
        .min_by_key(|(_, count)| *count)
        .map(|(numa, _)| *numa)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_binds_the_least_loaded_numa() {
        // Fewest workers wins over config order: numa 1 has one worker to
        // numa 0's two.
        let snap = TierCSnapshot::build(NumaAffinity::Single, &[0, 0, 1]);
        assert_eq!(
            snap.resolve(Some("dev")),
            Placement {
                affinity: NumaAffinity::Single,
                bound_numa: Some(1)
            }
        );
        // Fewest workers wins regardless of config order: numa 1 leads the
        // list with one worker, numa 0 follows with two.
        let snap = TierCSnapshot::build(NumaAffinity::Single, &[1, 0, 0]);
        assert_eq!(snap.resolve(None).bound_numa, Some(1));
        // An exact tie (one worker per numa): config order decides — numa 1
        // was seen first, so numa 1 wins over numa 0.
        let snap = TierCSnapshot::build(NumaAffinity::Single, &[1, 0]);
        assert_eq!(snap.resolve(None).bound_numa, Some(1));
        // No workers at all: nothing to bind.
        assert_eq!(
            TierCSnapshot::build(NumaAffinity::Single, &[]).resolve(None).bound_numa,
            None
        );
    }

    #[test]
    fn cross_binds_nothing() {
        let snap = TierCSnapshot::build(NumaAffinity::Cross, &[0, 1, 1]);
        assert_eq!(
            snap.resolve(Some("anything")),
            Placement {
                affinity: NumaAffinity::Cross,
                bound_numa: None
            }
        );
    }
}
