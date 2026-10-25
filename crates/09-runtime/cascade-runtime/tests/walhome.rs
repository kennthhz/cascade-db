//! T2.6: WAL home-NUMA placement (A14, FR-27).
//!
//! The homing table is asserted right after "DB creation" (boot); appends
//! route to the one home writer even from a reactor sitting on a *different*
//! NUMA; and the persistence test is a real simulated restart per the test
//! review — a *second* boot re-reads the catalog file from disk (a wholly
//! separate `Runtime`, no in-memory state carried over) and honors the
//! persisted home even where re-derivation, and a changed pool layout,
//! would pick differently.
//!
//! Layout discriminator: pools are configured in *non-sorted* order
//! `[NUMA 1, NUMA 0]`, so the cross derivation (`db_hash(0) mod 2` → index
//! 1 → **NUMA 0**) differs from the A13 bound pick (fewest workers →
//! **NUMA 1**). A home that merely copies the bound NUMA, or a derivation
//! that drifts, cannot pass both tests.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use cascade_runtime::{
    config::{AuthConfig, NumaAffinity, NumaPool, NumaWorkers},
    pin, walhome, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology, WalHome,
};
use cascade_types::DatabaseId;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_wal_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cascade-walhome-{tag}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("wal temp dir");
    dir
}

/// (housekeeper, worker, worker, worker) — two workers on NUMA 0, one on
/// NUMA 1, so the A13 boot pick is NUMA 1 (fewest workers).
fn layout() -> (u32, u32, u32, u32) {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 4, "wal-home tests need at least 4 CPUs");
    allowed.truncate(4);
    (allowed[0], allowed[1], allowed[2], allowed[3])
}

fn topo(hk: u32, w0a: u32, w0b: u32, w1: u32) -> SyntheticTopology {
    SyntheticTopology::new([hk, w0a, w0b, w1])
        .with_numa(hk, 0)
        .with_numa(w0a, 0)
        .with_numa(w0b, 0)
        .with_numa(w1, 1)
}

/// Pools in *config order* `[1, 0]` (+ the optional worker-less NUMA 2 for
/// the changed-layout restart); see the module doc for why the order
/// matters. `cap1` sets `max_conns_per_worker = 1` for deterministic
/// routing. Returns the config plus every core, for topology building.
#[allow(clippy::type_complexity)]
fn cfg_for(
    default: NumaAffinity,
    dir: &Path,
    extra_pool: bool,
    cap1: bool,
) -> (RuntimeConfig, u32, u32, u32, u32) {
    let (hk, w0a, w0b, w1) = layout();
    let mut pools = vec![
        NumaPool { numa: 1, size_mb: 1 },
        NumaPool { numa: 0, size_mb: 1 },
    ];
    if extra_pool {
        pools.push(NumaPool { numa: 2, size_mb: 1 });
    }
    let base = RuntimeConfig::default();
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![
            NumaWorkers {
                numa: 0,
                cores: vec![w0a, w0b],
            },
            NumaWorkers {
                numa: 1,
                cores: vec![w1],
            },
        ],
        numa_pools: pools,
        default_db_numa_affinity: default,
        wal_dir: Some(dir.to_path_buf()),
        max_conns_per_worker: if cap1 { 1 } else { base.max_conns_per_worker },
        ..base
    };
    (cfg, hk, w0a, w0b, w1)
}

fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for: {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `single`: the dev DB's writer homes to the Tier C bound NUMA (A13 →
/// NUMA 1), the writer is constructed *on* that NUMA (its constructor ran
/// pinned to the NUMA 1 worker core), and the choice is persisted for the
/// next boot.
#[test]
fn single_writer_homes_to_the_bound_numa() {
    let dir = temp_wal_dir("single-bound");
    let (cfg, hk, w0a, w0b, w1) = cfg_for(NumaAffinity::Single, &dir, false, false);
    let t = topo(hk, w0a, w0b, w1);
    let rt = Runtime::boot(&cfg, &t).expect("boot");

    let dev = DatabaseId(0);
    assert!(
        rt.debug_wal().is_some(),
        "the home writer exists under wal_dir"
    );
    assert_eq!(
        rt.wal_homes().home(dev),
        Some(WalHome {
            numa: 1,
            constructed_on: Some(w1),
        }),
        "single affinity: writer homed to the bound NUMA, constructed there"
    );
    // A14: the choice is recorded where the next boot will re-read it.
    assert_eq!(
        walhome::read_homes(&dir),
        std::collections::HashMap::from([(0u32, 1u32)]),
        "the home choice is persisted"
    );
}

/// `cross`: appends from a connection running on a *different* NUMA than
/// the home still route to the one home writer (FR-27 through the home
/// writer — never a per-reactor copy of it).
#[test]
fn appends_route_to_the_home_writer_even_from_off_home_reactors() {
    let dir = temp_wal_dir("cross-append");
    let (cfg, hk, w0a, w0b, w1) = cfg_for(NumaAffinity::Cross, &dir, false, true);
    let t = topo(hk, w0a, w0b, w1);
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime::boot_routed(&cfg, &t, Some(listener), AuthConfig::default())
        .expect("boot routed");

    // Cross affinity: the designated home is the derivation, NUMA 0
    // (db_hash(0) mod 2 = 1 → registry index 1 = NUMA 0) — not the bound
    // pick a `single` boot would make (NUMA 1).
    let dev = DatabaseId(0);
    assert_eq!(
        rt.wal_homes().home(dev),
        Some(WalHome {
            numa: 0,
            constructed_on: Some(w0a),
        }),
        "cross affinity: writer homed to the derived NUMA, constructed there"
    );

    let writer = rt.debug_wal().expect("home writer wired").clone();
    assert_eq!(writer.syncs(), 0, "no barriers before any commit");

    // Three connections, settled one at a time (cap 1 per worker): the
    // third lands on the NUMA 1 worker — off the home NUMA.
    let url = |u: &str| format!("postgres://{u}:pw@127.0.0.1:{port}/dev");
    let mut c1 = postgres::Client::connect(&url("u1"), postgres::NoTls).expect("first connection");
    wait_until(|| rt.workers()[0].active_connections() == 1, "c1 on worker 0");
    let mut c2 = postgres::Client::connect(&url("u2"), postgres::NoTls).expect("second connection");
    wait_until(|| rt.workers()[1].active_connections() == 1, "c2 on worker 1");
    let mut c3 = postgres::Client::connect(&url("u3"), postgres::NoTls)
        .expect("third connection crosses to the NUMA 1 worker");
    wait_until(|| rt.workers()[2].active_connections() == 1, "c3 on the NUMA 1 worker");

    // A commit from the off-home reactor is durable through the home
    // writer — one writer, wherever the session runs.
    c3.simple_query("BEGIN").expect("begin");
    c3.simple_query("INSERT INTO debug_table VALUES (41, 'offhome')")
        .expect("insert");
    c3.simple_query("COMMIT").expect("commit ack");
    assert!(
        writer.syncs() >= 1,
        "the off-home commit synced through the home writer"
    );
    let _ = (c1.simple_query("SELECT 1"), c2.simple_query("SELECT 1"));
}

/// The A14 contract, end to end: first boot *derives* the cross home
/// (golden: `db_hash(0) mod 2` → NUMA 0 with this registry order); a later
/// boot honors whatever the catalog file says — even when re-derivation
/// would pick differently, and even when the pool layout *changed* under
/// it. Each boot below is a fresh `Runtime`; the file is the only channel
/// between them.
#[test]
fn cross_home_is_derived_then_the_persisted_choice_survives_restarts() {
    let dir = temp_wal_dir("cross-persist");
    let (cfg, hk, w0a, w0b, w1) = cfg_for(NumaAffinity::Cross, &dir, false, false);
    let t = topo(hk, w0a, w0b, w1);

    // Boot #1 (creation): the derivation picks NUMA 0 — *not* NUMA 1 (the
    // bound pick a copy-the-bound home would make).
    let rt1 = Runtime::boot(&cfg, &t).expect("first boot");
    let dev = DatabaseId(0);
    assert_eq!(
        rt1.wal_homes().home(dev).map(|h| h.numa),
        Some(0),
        "first creation derives the cross home from the frozen hash"
    );
    assert_eq!(
        walhome::read_homes(&dir),
        std::collections::HashMap::from([(0u32, 0u32)]),
        "the derived choice is persisted"
    );

    // Simulate a home chosen under an older layout: NUMA 1.
    walhome::write_homes(&dir, &std::collections::HashMap::from([(0u32, 1u32)]));

    // Boot #2 (restart, same layout): the persisted choice wins — the
    // derivation would say NUMA 0 again, and A14 forbids re-picking.
    let rt2 = Runtime::boot(&cfg, &t).expect("restart boot");
    assert_eq!(
        rt2.wal_homes().home(dev).map(|h| h.numa),
        Some(1),
        "a restart re-reads the persisted home instead of re-deriving"
    );

    // Boot #3 (restart, *changed* layout): a third pool shifts the
    // derivation to NUMA 2 — the persisted home still holds.
    let (cfg3, _, _, _, _) = cfg_for(NumaAffinity::Cross, &dir, true, false);
    let rt3 = Runtime::boot(&cfg3, &t).expect("changed-layout boot");
    assert_eq!(
        rt3.wal_homes().home(dev).map(|h| h.numa),
        Some(1),
        "a changed pool layout must not migrate a database's WAL home"
    );
    // And the file still says so (the rewrite preserved the row).
    assert_eq!(
        walhome::read_homes(&dir),
        std::collections::HashMap::from([(0u32, 1u32)]),
        "the persisted row survives every restart"
    );
}

/// A persisted home naming a NUMA the config no longer pools is unusable:
/// boot warns and re-derives rather than crashing or silently placing the
/// writer nowhere — then rewrites the file so the next boot is clean.
#[test]
fn persisted_home_absent_from_pools_degrades_to_derivation() {
    let dir = temp_wal_dir("cross-stale");
    let (cfg, hk, w0a, w0b, w1) = cfg_for(NumaAffinity::Cross, &dir, false, false);
    let t = topo(hk, w0a, w0b, w1);
    walhome::write_homes(&dir, &std::collections::HashMap::from([(0u32, 7u32)]));

    let rt = Runtime::boot(&cfg, &t).expect("stale home degrades, does not crash");
    let dev = DatabaseId(0);
    assert_eq!(
        rt.wal_homes().home(dev).map(|h| h.numa),
        Some(0),
        "the unusable row falls back to the derivation"
    );
    assert_eq!(
        walhome::read_homes(&dir),
        std::collections::HashMap::from([(0u32, 0u32)]),
        "boot rewrote the file with the usable home"
    );
}
