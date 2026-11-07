//! Boot failures that fire AFTER the housekeepers spawn (M2), and
//! config-vs-topology label mismatches (M5) — the post-M5 review's
//! boot-boundary fixes.
//!
//! The M2 shape: every boot error past `spawn_housekeepers` used to
//! leak the spawned threads DETACHED — the serving housekeeper still
//! held the listener, so the port stayed bound for the life of the
//! process. The fix drops the boot-owned channel halves (a connect
//! resolves when its peer disconnects), releases the shutdown-wake fd
//! dup, and joins under a watchdog. The observable is the port: a
//! failed boot must give it back.

use std::{net::TcpListener, time::Duration};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin, AcceptLoop, BootError, Runtime, RuntimeConfig, SyntheticTopology,
};

/// M2: a `DemoTooLarge` boot failure — it fires after the housekeepers
/// spawn, before the workers — must not leave the serving housekeeper
/// holding the listener. The failed boot returns, and the port comes
/// back. Pre-fix, the detached housekeeper held it forever and every
/// rebind here failed with EADDRINUSE.
#[test]
fn failed_boot_reclaims_the_serving_port() {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![allowed[0]],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: vec![allowed[1], allowed[2]],
        }],
        numa_pools: vec![NumaPool {
            numa: 0,
            size_mb: 1,
        }],
        // 1 MB pool = 256 frames; 100k demo rows cannot fit — the
        // refusal fires at DemoHeap::populate, after the housekeepers
        // spawned and before the workers do.
        demo_rows: 100_000,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let err = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect_err("the oversized demo refuses the boot");
    assert!(
        matches!(err, BootError::DemoTooLarge { rows: 100_000, .. }),
        "the failure is the demo budget: {err}"
    );

    // The port is free again — bounded (the watchdog allows 5s; the
    // resolved connects let the serving thread exit in milliseconds).
    let mut rebound = false;
    for _ in 0..100 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            rebound = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        rebound,
        "the failed boot released the serving port (housekeepers joined, not detached)"
    );
}

/// M5: a worker NUMA label no online core carries refuses the boot —
/// the label drives the designated grouping, the WAL home, and the
/// drain flags, so a lying label must not silently first-touch
/// wherever the fallback lands. (Pool labels alone are outside this
/// check: T2.3's best-effort pool placement stays.)
#[test]
fn refuses_worker_numa_unknown_to_the_topology() {
    // All three cores on node 0; the config claims a NUMA 9 worker
    // group. No pin is attempted — the refusal fires before anything
    // spawns — so synthetic core ids are safe here.
    let topo = SyntheticTopology::new([0, 1, 2]);
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![0],
        worker_cores: vec![NumaWorkers {
            numa: 9,
            cores: vec![1],
        }],
        numa_pools: vec![NumaPool {
            numa: 9,
            size_mb: 1,
        }],
        ..RuntimeConfig::default()
    };
    cascade_runtime::testutil::arm_suite_watchdog();
    let err = Runtime::boot_routed(&cfg, &topo, None, AuthConfig::default(), None)
        .expect_err("an unknown worker numa refuses the boot");
    assert!(
        matches!(err, BootError::NumaLabelUnknown { numa: 9 }),
        "the refusal names the label: {err}"
    );
}
