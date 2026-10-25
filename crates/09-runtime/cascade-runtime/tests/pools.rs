//! T2.3: per-NUMA pool registry (FR-22 placement + sizing) and per-reactor
//! arenas (FR-37).
//!
//! Per the plan's test review, the CI tests exercise the *mapping and
//! sizing* logic on synthetic topologies (synthetic NUMA grouping over the
//! host's real, pinnable cores); only the node-locality *assertion* itself
//! is relegated to the bare-metal `#[ignore]` test — this host (WSL2)
//! exposes no NUMA information at all.

use std::collections::HashMap;

use cascade_runtime::{
    arena::ARENA_RESERVE_BYTES,
    config::{NumaPool, NumaWorkers},
    pin, Runtime, RuntimeConfig, SyntheticTopology, TopologyProvider,
};
use cascade_runtime::topology::SysTopology;

fn allowed4() -> Vec<u32> {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 4, "pool boot tests need at least 4 CPUs");
    allowed.truncate(4);
    allowed
}

/// Housekeeper + one worker per NUMA + a pool per NUMA, with *different*
/// sizes so per-NUMA sizing (§8.1) is observable.
fn two_numa_config(c: [u32; 4]) -> (RuntimeConfig, SyntheticTopology) {
    let (hk, w0, w1, spare) = (c[0], c[1], c[2], c[3]);
    let topo = SyntheticTopology::new([hk, w0, w1, spare])
        .with_numa(hk, 0)
        .with_numa(w0, 0)
        .with_numa(w1, 1)
        .with_numa(spare, 1);
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![
            NumaWorkers {
                numa: 0,
                cores: vec![w0],
            },
            NumaWorkers {
                numa: 1,
                cores: vec![w1],
            },
        ],
        numa_pools: vec![
            NumaPool { numa: 0, size_mb: 1 },
            NumaPool { numa: 1, size_mb: 2 },
        ],
        ..RuntimeConfig::default()
    };
    (cfg, topo)
}

#[test]
fn pools_map_to_numas_with_per_numa_sizes() {
    let (cfg, topo) = two_numa_config(
        allowed4()[..4]
            .try_into()
            .expect("four allowed cores"),
    );
    let rt = Runtime::boot(&cfg, &topo).expect("boot succeeds on online cores");

    // Mapping: one pool per configured NUMA, in config order (FR-22).
    assert_eq!(
        rt.pools().iter().map(|p| p.numa()).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(rt.pools().num_numas(), 2);

    // Sizing: 1 MiB and 2 MiB → 128 and 256 8-KiB frames — each pool sized
    // by its own config entry, never one total split uniformly (§8.1).
    assert_eq!(rt.pools().pool_for_numa(0).expect("pool 0").frames(), 128);
    assert_eq!(rt.pools().pool_for_numa(1).expect("pool 1").frames(), 256);

    // §10.1 step 6c: each worker is bound to its own NUMA's pool index.
    let bindings: Vec<(u32, usize)> = rt
        .workers()
        .iter()
        .map(|w| (w.numa, w.pool_index()))
        .collect();
    assert!(bindings.contains(&(0, 0)), "worker on numa 0 → pool 0");
    assert!(bindings.contains(&(1, 1)), "worker on numa 1 → pool 1");

    // FR-37: every reactor reserved its working arena in-task.
    assert!(
        rt.workers()
            .iter()
            .all(|w| w.arena_reserved_bytes() == ARENA_RESERVE_BYTES)
    );
}

/// §11 A18: more NUMA pools than worker NUMAs is legal — the worker-less
/// pool exists for `cross`-affinity placement, and boot validates rather
/// than crashes.
#[test]
fn worker_less_pool_is_legal() {
    let allowed = allowed4();
    let (hk, w0, w1, spare) = (allowed[0], allowed[1], allowed[2], allowed[3]);
    // Three nodes; workers configured on only two of them.
    let topo = SyntheticTopology::new([hk, w0, w1, spare])
        .with_numa(hk, 0)
        .with_numa(w0, 0)
        .with_numa(w1, 1)
        .with_numa(spare, 2);
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![
            NumaWorkers {
                numa: 0,
                cores: vec![w0],
            },
            NumaWorkers {
                numa: 1,
                cores: vec![w1],
            },
        ],
        numa_pools: vec![
            NumaPool { numa: 0, size_mb: 1 },
            NumaPool { numa: 1, size_mb: 1 },
            NumaPool { numa: 2, size_mb: 1 },
        ],
        ..RuntimeConfig::default()
    };

    let rt = Runtime::boot(&cfg, &topo).expect("a worker-less pool is legal (A18)");
    assert_eq!(rt.pools().num_numas(), 3);
    let pool2 = rt.pools().pool_for_numa(2).expect("worker-less pool exists");
    assert_eq!(pool2.frames(), 128);
    assert!(
        rt.workers().iter().all(|w| w.pool_index() != 2),
        "no worker is bound to the worker-less pool"
    );
}

/// A pool NUMA no host kernel can serve (node 999 — the guard skips mbind
/// outright above 64 nodes) degrades to first-touch with a warning instead
/// of failing boot. This is also the WSL2-shaped reality: NUMA unknown,
/// placement best-effort, boot honest about it.
#[test]
fn pool_numa_absent_from_host_degrades_not_crashes() {
    let allowed = allowed4();
    let (hk, w0, _w1, _spare) = (allowed[0], allowed[1], allowed[2], allowed[3]);
    let topo = SyntheticTopology::new([hk, w0]).with_numa(hk, 0).with_numa(w0, 0);
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: vec![w0],
        }],
        numa_pools: vec![
            NumaPool { numa: 0, size_mb: 1 },
            NumaPool {
                numa: 999,
                size_mb: 1,
            },
        ],
        ..RuntimeConfig::default()
    };

    let rt = Runtime::boot(&cfg, &topo).expect("unplaceable pool warns, does not crash");
    let ghost = rt.pools().pool_for_numa(999).expect("pool still exists");
    assert_eq!(ghost.frames(), 128);
    assert!(
        !ghost.node_bound(),
        "no kernel has node 999 — placement must be reported as best-effort"
    );
}

/// Bare metal only: pool page placement, verified through
/// `/proc/self/numa_maps` (FR-22's "allocated on its node"; FR-37's
/// "verified on bare metal"). Skips on hosts with fewer than two known
/// NUMA nodes (CI, WSL2). The arena's placement is by construction
/// (reserved in-task on the pinned core) and needs no separate check.
#[test]
#[ignore = "requires a real multi-NUMA host"]
fn pools_are_node_local_on_bare_metal() {
    let topo = SysTopology::probe().expect("real sysfs topology");
    let online = topo.online_cpus().expect("online cpus");
    let mut by_node: HashMap<u32, Vec<u32>> = HashMap::new();
    for &cpu in &online {
        if let Some(node) = topo.numa_node(cpu) {
            by_node.entry(node).or_default().push(cpu);
        }
    }
    let mut nodes: Vec<(u32, &Vec<u32>)> = by_node.iter().map(|(&n, v)| (n, v)).collect();
    nodes.sort_by_key(|(n, _)| *n);
    if nodes.len() < 2 {
        eprintln!("single-NUMA host (or no NUMA info); skipping locality check");
        return;
    }
    // One core per node, two nodes: housekeeper + worker on node A,
    // worker on node B.
    let (na, cores_a) = nodes[0];
    let (nb, cores_b) = nodes[1];
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![cores_a[0]],
        worker_cores: vec![
            NumaWorkers {
                numa: na,
                cores: vec![cores_a[cores_a.len() - 1]],
            },
            NumaWorkers {
                numa: nb,
                cores: vec![cores_b[0]],
            },
        ],
        numa_pools: vec![
            NumaPool { numa: na, size_mb: 2 },
            NumaPool { numa: nb, size_mb: 2 },
        ],
        ..RuntimeConfig::default()
    };

    let rt = Runtime::boot(&cfg, &topo).expect("bare-metal boot");
    for pool in rt.pools().iter() {
        // Without a bound node the locality claim is best-effort by
        // definition (first-touch) — only a bound pool asserts placement.
        if !pool.node_bound() {
            eprintln!("pool numa {} not node-bound; skipping", pool.numa());
            continue;
        }
        pool.memory().touch();
        let counts = numa_maps_counts(pool.memory().addr());
        let total: u64 = counts.values().sum();
        assert!(total > 0, "pool numa {} has no resident pages", pool.numa());
        let on_node = counts.get(&pool.numa()).copied().unwrap_or(0);
        assert_eq!(
            on_node, total,
            "pool numa {} pages span nodes: {counts:?}",
            pool.numa()
        );
    }
}

/// Parses `/proc/self/numa_maps`, returning the N<k> per-node page counts
/// of the mapping containing `addr` (zero map if the range is not resident
/// yet).
fn numa_maps_counts(addr: usize) -> HashMap<u32, u64> {
    let text = std::fs::read_to_string("/proc/self/numa_maps").expect("numa_maps readable");
    for line in text.lines() {
        let (range, rest) = line.split_once(' ').expect("numa_maps line shape");
        let (start, end) = range.split_once('-').expect("numa_maps range shape");
        let start: usize = start.parse().expect("range start");
        let end: usize = end.parse().expect("range end");
        if start <= addr && addr < end {
            let mut counts = HashMap::new();
            for field in rest.split_whitespace() {
                if let Some(kv) = field.strip_prefix('N') {
                    if let Some((node, count)) = kv.split_once('=') {
                        if let (Ok(node), Ok(count)) = (node.parse(), count.parse()) {
                            counts.insert(node, count);
                        }
                    }
                }
            }
            return counts;
        }
    }
    HashMap::new()
}
