//! T2.6: WAL home-NUMA placement (A14, FR-27).
//!
//! The homing table is asserted right after "DB creation" (boot); appends
//! route to the one home writer even from a reactor sitting on a *different*
//! NUMA; and the persistence test is a real simulated restart per the test
//! review — a *second* boot re-reads the catalog file from disk (a wholly
//! separate `Runtime`, no in-memory state carried over) and honors the
//! persisted home even where re-derivation would now pick differently.
//!
//! Layout discriminator (M4, post-M5 review): cross derivation runs over
//! the DESIGNATED numas — one entry per numa that has a worker, in worker
//! config order — never over every pool numa. With workers `[NUMA 0: two
//! cores, NUMA 1: one]` the designated set is `[0, 1]` and the frozen
//! hash (`db_hash(0) mod 2` → index 1) picks **NUMA 1**, while the A13
//! bound pick (fewest workers) is also **NUMA 1** — so the appended
//! worker-less-pool test carries the real discriminator: pools `[0, 7]`
//! with no worker on 7 would pre-M4 derive the home onto 7, where no
//! reactor can flush the tail or stamp the clean marker. A home that
//! merely copies the bound NUMA, or a derivation over pool numas, cannot
//! pass the whole file.

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

/// (housekeeper, two workers on NUMA 0, one worker on NUMA 1, spare) —
/// the A13 boot pick for `single` is NUMA 1 (fewest workers), and the
/// designated set for cross derivation is `[0, 1]` in worker config
/// order. The spare is labeled (NUMA 2) but in no worker group: it makes
/// the changed-layout restart's extra pool a label the topology knows
/// (M5) while keeping the pool worker-less.
fn layout() -> (u32, u32, u32, u32, u32) {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 5, "wal-home tests need at least 5 CPUs");
    allowed.truncate(5);
    (allowed[0], allowed[1], allowed[2], allowed[3], allowed[4])
}

fn topo(hk: u32, w0a: u32, w0b: u32, w1: u32, spare: u32) -> SyntheticTopology {
    SyntheticTopology::new([hk, w0a, w0b, w1, spare])
        .with_numa(hk, 0)
        .with_numa(w0a, 0)
        .with_numa(w0b, 0)
        .with_numa(w1, 1)
        .with_numa(spare, 2)
}

/// Pools in *config order* `[1, 0]` (+ the optional worker-less NUMA 2 for
/// the changed-layout restart) — order that M4's designated-only
/// derivation deliberately ignores (pools never shift a derived home
/// anymore; the order stays to pin that). `cap1` sets
/// `max_conns_per_worker = 1` for deterministic routing. Returns the
/// config plus every core, for topology building.
#[allow(clippy::type_complexity)]
fn cfg_for(
    default: NumaAffinity,
    dir: &Path,
    extra_pool: bool,
    cap1: bool,
) -> (RuntimeConfig, u32, u32, u32, u32, u32) {
    let (hk, w0a, w0b, w1, spare) = layout();
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
        // NIC tie-breaker pinned off: these landings assume the count-only
        // tie order (first-wins) — loadview.rs owns the composite's determinism.
        nic_numa: None,
        max_conns_per_worker: if cap1 { 1 } else { base.max_conns_per_worker },
        ..base
    };
    (cfg, hk, w0a, w0b, w1, spare)
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
    let (cfg, hk, w0a, w0b, w1, _spare) = cfg_for(NumaAffinity::Single, &dir, false, false);
    let t = topo(hk, w0a, w0b, w1, _spare);
    cascade_runtime::testutil::arm_suite_watchdog();
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
            constructed_on: None,
        }),
        "single affinity: writer homed to the bound NUMA (M6: constructed on the bootstrap reactor; first-touch defers)"
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
    let (cfg, hk, w0a, w0b, w1, spare) = cfg_for(NumaAffinity::Cross, &dir, false, true);
    let t = topo(hk, w0a, w0b, w1, spare);
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &t, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");

    // Cross affinity: the designated home is the derivation over the
    // designated set `[0, 1]` in worker config order — db_hash(0) is
    // odd, so index 1 → NUMA 1, the first worker of which is `w1`.
    let dev = DatabaseId(0);
    assert_eq!(
        rt.wal_homes().home(dev),
        Some(WalHome {
            numa: 1,
            constructed_on: None,
        }),
        "cross affinity: writer homed to the derived NUMA (M6: constructed on the bootstrap reactor; first-touch defers)"
    );

    let writer = rt.debug_wal().expect("home writer wired").clone();
    assert_eq!(writer.syncs(), 0, "no barriers before any commit");

    // Three connections, settled one at a time (cap 1 per worker): the
    // first lands on a NUMA 0 worker — off the home NUMA.
    let url = |u: &str| format!("postgres://{u}:pw@127.0.0.1:{port}/dev");
    let mut c1 = postgres::Client::connect(&url("u1"), postgres::NoTls).expect("first connection");
    wait_until(|| rt.workers()[0].active_connections() == 1, "c1 on worker 0");
    let mut c2 = postgres::Client::connect(&url("u2"), postgres::NoTls).expect("second connection");
    wait_until(|| rt.workers()[1].active_connections() == 1, "c2 on worker 1");
    let mut c3 = postgres::Client::connect(&url("u3"), postgres::NoTls)
        .expect("third connection lands on the home NUMA's worker");
    wait_until(|| rt.workers()[2].active_connections() == 1, "c3 on the NUMA 1 worker");

    // A commit from the off-home reactor is durable through the home
    // writer — one writer, wherever the session runs.
    c1.simple_query("BEGIN").expect("begin");
    c1.simple_query("INSERT INTO debug_table VALUES (41, 'offhome')")
        .expect("insert");
    c1.simple_query("COMMIT").expect("commit ack");
    assert!(
        writer.syncs() >= 1,
        "the off-home commit synced through the home writer"
    );
    let _ = (c2.simple_query("SELECT 1"), c3.simple_query("SELECT 1"));
}

/// The A14 contract, end to end: first boot *derives* the cross home
/// (golden: over the designated set `[0, 1]`, the frozen hash picks
/// NUMA 1); a later boot honors whatever the catalog file says — even
/// when re-derivation would pick differently. Each boot below is a fresh
/// `Runtime`; the file is the only channel between them.
#[test]
fn cross_home_is_derived_then_the_persisted_choice_survives_restarts() {
    let dir = temp_wal_dir("cross-persist");
    let (cfg, hk, w0a, w0b, w1, spare) = cfg_for(NumaAffinity::Cross, &dir, false, false);
    let t = topo(hk, w0a, w0b, w1, spare);

    // Boot #1 (creation): the derivation picks NUMA 1 — *not* NUMA 0
    // (the head of the designated set an index-0 pick would take).
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt1 = Runtime::boot(&cfg, &t).expect("first boot");
    let dev = DatabaseId(0);
    assert_eq!(
        rt1.wal_homes().home(dev).map(|h| h.numa),
        Some(1),
        "first creation derives the cross home from the frozen hash"
    );
    assert_eq!(
        walhome::read_homes(&dir),
        std::collections::HashMap::from([(0u32, 1u32)]),
        "the derived choice is persisted"
    );

    // Simulate a home chosen under an older layout: NUMA 0.
    walhome::write_homes(&dir, &std::collections::HashMap::from([(0u32, 0u32)]));

    // Boot #2 (restart, same layout): the persisted choice wins — the
    // derivation would say NUMA 1 again, and A14 forbids re-picking.
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt2 = Runtime::boot(&cfg, &t).expect("restart boot");
    assert_eq!(
        rt2.wal_homes().home(dev).map(|h| h.numa),
        Some(0),
        "a restart re-reads the persisted home instead of re-deriving"
    );

    // Boot #3 (restart, *changed* pool layout): the extra pool shifts
    // nothing — M4 derives over the designated numas only, and those
    // did not change — so re-derivation would still say NUMA 1, and
    // the persisted NUMA 0 still holds.
    let (cfg3, hk3, w0a3, w0b3, w13, spare3) = cfg_for(NumaAffinity::Cross, &dir, true, false);
    let t3 = topo(hk3, w0a3, w0b3, w13, spare3);
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt3 = Runtime::boot(&cfg3, &t3).expect("changed-layout boot");
    assert_eq!(
        rt3.wal_homes().home(dev).map(|h| h.numa),
        Some(0),
        "a changed pool layout must not migrate a database's WAL home"
    );
    // And the file still says so (the rewrite preserved the row).
    assert_eq!(
        walhome::read_homes(&dir),
        std::collections::HashMap::from([(0u32, 0u32)]),
        "the persisted row survives every restart"
    );
}

/// A persisted home naming a NUMA that is no longer DESIGNATED — no
/// worker on it, so no reactor to flush the tail or stamp the marker —
/// is unusable: boot warns and re-derives rather than refusing the clean
/// marker forever (M4's honest boundary). It rewrites the file so the
/// next boot is clean.
#[test]
fn persisted_home_absent_from_designated_numas_degrades_to_derivation() {
    let dir = temp_wal_dir("cross-stale");
    let (cfg, hk, w0a, w0b, w1, spare) = cfg_for(NumaAffinity::Cross, &dir, false, false);
    let t = topo(hk, w0a, w0b, w1, spare);
    walhome::write_homes(&dir, &std::collections::HashMap::from([(0u32, 7u32)]));

    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot(&cfg, &t).expect("stale home degrades, does not crash");
    let dev = DatabaseId(0);
    assert_eq!(
        rt.wal_homes().home(dev).map(|h| h.numa),
        Some(1),
        "the unusable row falls back to the derivation over the designated set"
    );
    assert_eq!(
        walhome::read_homes(&dir),
        std::collections::HashMap::from([(0u32, 1u32)]),
        "boot rewrote the file with the usable home"
    );
}

/// M4 (post-M5 review), the real discriminator: a pool on a NUMA with NO
/// workers must never receive a derived home — derivation runs over the
/// designated numas only. Pre-fix, the pool-order derivation below picks
/// NUMA 7 (`db_hash(0)` is odd; pools `[0, 7]` → index 1), no drain
/// sender is flagged there, and the clean marker is refused at every
/// shutdown — every restart replaying as a crash. Post-fix the home is
/// the designated NUMA 0 and the marker stamps.
#[test]
fn workerless_pool_numa_never_homes_the_writer() {
    let dir = temp_wal_dir("cross-workerless");
    let (hk, w0, _w0b, _w1, spare) = layout();
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: vec![w0],
        }],
        numa_pools: vec![
            NumaPool { numa: 0, size_mb: 1 },
            NumaPool { numa: 7, size_mb: 1 },
        ],
        default_db_numa_affinity: NumaAffinity::Cross,
        wal_dir: Some(dir.clone()),
        nic_numa: None,
        ..RuntimeConfig::default()
    };
    let t = SyntheticTopology::new([hk, w0, spare])
        .with_numa(hk, 0)
        .with_numa(w0, 0)
        .with_numa(spare, 7);
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &t, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");

    // The derivation could only reach NUMA 7 by reading the pool numas —
    // the honest candidate set is the designated one, so the home is 0.
    let dev = DatabaseId(0);
    assert_eq!(
        rt.wal_homes().home(dev).map(|h| h.numa),
        Some(0),
        "a worker-less pool numa is never a derived home"
    );

    // And the shutdown runs the home's final flush (M6: the marker died
    // with the file path — the log store owns its tail at open): the home
    // has a designated reactor to run it. The pre-fix boot refused even
    // the flush.
    let report = rt.shutdown();
    assert!(
        report.phases.contains(&"final-wal-flush"),
        "the home reactor runs the final flush: {:?}",
        report.phases
    );
}
