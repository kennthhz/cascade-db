//! T1.1: boot shape and pin verification (FR-1, FR-2).
//!
//! Cores are chosen from the *current* allowed set so the suite runs on any
//! machine; the topology provider is synthetic either way (fixed list), which
//! is what CI uses.

use cascade_runtime::{
    config::{NumaPool, NumaWorkers},
    pin, AllowedCpuTopology, BootError, Runtime, RuntimeConfig, SyntheticTopology,
};

/// A config shaped like production: housekeeper on one core, two workers on
/// NUMA 0, one pool.
fn test_config(hk: u32, workers: &[u32]) -> RuntimeConfig {
    RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: workers.to_vec(),
        }],
        numa_pools: vec![NumaPool {
            numa: 0,
            size_mb: 1,
        }],
        ..RuntimeConfig::default()
    }
}

#[test]
fn boots_synthetic_topology_and_verifies_pins() {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "boot test needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = test_config(allowed[0], &[allowed[1], allowed[2]]);

    let rt = Runtime::boot(&cfg, &topo).expect("boot succeeds on online cores");

    assert_eq!(
        rt.housekeepers().len(),
        1,
        "one housekeeper thread per housekeeper core"
    );
    assert_eq!(rt.housekeepers()[0].core, allowed[0]);
    assert_eq!(
        rt.workers().iter().map(|w| w.core).collect::<Vec<_>>(),
        vec![allowed[1], allowed[2]],
        "one worker reactor per configured core",
    );
    assert!(rt.workers().iter().all(|w| w.numa == 0));

    // FR-1 refusal: a core the topology does not report online must fail
    // boot, not half-start.
    let bad_cfg = test_config(allowed[0], &[99]);
    match Runtime::boot(&bad_cfg, &topo).expect_err("offline worker core must refuse boot") {
        BootError::CoreOffline { core } => assert_eq!(core, 99),
        other => panic!("expected CoreOffline(99), got {other:?}"),
    }
}

#[test]
fn refuses_housekeeper_core_offline() {
    let topo = SyntheticTopology::new([0, 1]);
    let cfg = test_config(42, &[1]);
    match Runtime::boot(&cfg, &topo).expect_err("offline housekeeper core must refuse boot") {
        BootError::CoreOffline { core } => assert_eq!(core, 42),
        other => panic!("expected CoreOffline(42), got {other:?}"),
    }
}

/// A provider that lies (claims a core the kernel will not pin) must still
/// end in refusal — the pin layer is the backstop. This is the mutation
/// check from the plan's T1.1 test review: a bad `TopologyProvider` cannot
/// produce a half-started or silently unpinned runtime.
#[test]
fn refuses_core_provider_claims_but_kernel_rejects() {
    let allowed = pin::current_affinity();
    assert!(allowed.len() >= 2, "needs at least 2 CPUs");

    let lying = SyntheticTopology::new(allowed.iter().copied().chain([9999]));
    let cfg = test_config(allowed[0], &[9999]);
    match Runtime::boot(&cfg, &lying).expect_err("kernel-refused core must refuse boot") {
        err @ (BootError::WorkerSpawn { .. } | BootError::CoreOffline { .. }) => {
            // Exact variant depends on the host: either the provider check
            // already knows 9999 is impossible, or the kernel refuses at
            // spawn. Both are refusals — neither may be Ok(_).
            let msg = format!("{err}");
            assert!(msg.contains("9999"), "error must name the core: {msg}");
        }
        other => panic!("expected a refusal naming core 9999, got {other:?}"),
    }
}

/// Bare-metal smoke: boot against the process's real allowed CPU set. Ignored
/// by default (it pins real threads); run explicitly with
/// `cargo test -- --ignored`.
#[test]
#[ignore]
fn boots_bare_metal() {
    let allowed = pin::current_affinity();
    assert!(
        allowed.len() >= 3,
        "bare-metal boot test needs at least 3 CPUs"
    );

    let topo = AllowedCpuTopology;
    let cfg = test_config(allowed[0], &[allowed[1], allowed[2]]);
    let rt = Runtime::boot(&cfg, &topo).expect("boot on the real topology");

    // FR-2's whole point: boot returning Ok means every pin was verified
    // against a read-back, not assumed.
    assert_eq!(rt.workers().len(), 2);
}
