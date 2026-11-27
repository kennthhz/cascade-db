//! T4.6 (FR-33): the statistics collector — per-core counters, summed on
//! read.
//!
//! The plan's test-review question is answered structurally: the
//! double-count test drives REAL transactions through TWO DIFFERENT
//! workers and asserts EXACT totals. A transaction cannot span workers
//! — connections are pinned for life (invariant 2) — so "the same txn
//! through two workers" means one txn per worker with the sums checked
//! exactly. The split is SEQUENCED, not assumed: session A's gauge
//! increment is awaited before B connects. (The RFB that completes a
//! connect is sent by the housekeeper's handshake, BEFORE the fd
//! handoff — an immediate second connect can race the router's
//! `choose()` into reading 0/0 and parking both connections on one
//! reactor, the router's own admitted count race.) Any worker counting
//! another's work, any boot-time double registration, breaks the
//! equality.
//!
//! The read path is the staged stand-in for the T5.1 `submit_to` fan-in
//! (the counters' Arc-atomic substrate makes the direct sum exact; see
//! `crate::stats`' module doc) — same staging discipline as the
//! background signal families' channels (the checkpointer's control
//! channel that set the pattern retired with storage M8).

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};


use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

/// Pin-heavy fixtures must not overlap (deployment.rs discipline).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn boot(workers: usize) -> (String, Runtime) {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() > workers, "needs {} CPUs", workers + 1);
    allowed.truncate(workers + 1);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![allowed[0]],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: allowed[1..=workers].to_vec(),
        }],
        numa_pools: vec![NumaPool {
            numa: 0,
            size_mb: 1,
        }],
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
    (format!("127.0.0.1:{port}"), rt)
}

fn client(addr: &str) -> postgres::Client {
    let url = format!("postgres://alice:pw@{addr}/dev");
    postgres::Client::connect(&url, postgres::NoTls).expect("client connects")
}

/// Polls until the fleet's live-connection gauge drains to zero — the
/// FR-15 cleanup observable (every exit path decremented).
fn wait_connections_drained(rt: &Runtime, secs: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let n = rt.stats().connections;
        if n == 0 || Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Polls until the gauge reaches `want`: the housekeeper's `handshake()`
/// completes the whole auth exchange BEFORE the fd is handed to a
/// worker, so a freshly-connected client can race its session task into
/// existence (the client is "connected" while the gauge still reads 0).
fn wait_connections_reached(rt: &Runtime, want: u64, secs: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let n = rt.stats().connections;
        if n >= want || Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The double-count test (the plan's test review, answered with real
/// traffic): two transactions, one per worker, exact sums. The split is
/// sequenced (A's gauge increment awaited before B routes — see the
/// module doc) and then observed: while both connections are open, each
/// reactor's gauge reads exactly 1. Totals afterwards: 6 statements, 1
/// commit, 1 rollback, 0 connections — any double count anywhere in the
/// plumbing (boot registration, worker vs router sharing, summing one
/// set twice) breaks one of these.
#[test]
fn one_txn_per_worker_sums_exactly() {
    let _serial = serial();
    let (addr, rt) = boot(2);

    // Fresh boot: nothing counted yet.
    let s = rt.stats();
    assert_eq!(
        s,
        cascade_runtime::StatsSnapshot::default(),
        "a fresh boot's aggregate must be all zeros"
    );

    let mut a = client(&addr);
    // Sequence the split into existence: session A's gauge increment
    // must land BEFORE B is routed, or choose() can read 0/0 (ties go
    // to the first route) and park both connections on one worker.
    assert_eq!(
        wait_connections_reached(&rt, 1, 5),
        1,
        "session A must be serving before B is routed"
    );
    let mut b = client(&addr);

    // Both open (the handoff may still be in flight right after
    // connect — wait for the gauge, don't race it).
    assert_eq!(
        wait_connections_reached(&rt, 2, 5),
        2,
        "two live connections must both be serving"
    );
    // The sequenced split, observed: each worker holds exactly one
    // connection (attribution, not assumed).
    let loads: Vec<u64> = rt.workers().iter().map(|w| w.active_connections()).collect();
    assert_eq!(
        loads.iter().filter(|&&l| l == 1).count(),
        2,
        "two live connections must sit on two different reactors: {loads:?}"
    );
    assert_eq!(rt.stats().statements, 0, "no statements yet, only connections");

    // Worker A's txn: BEGIN, INSERT, COMMIT — 3 statements, 1 commit.
    a.simple_query("BEGIN").expect("begin");
    a.simple_query("INSERT INTO debug_table VALUES (1, 'a')")
        .expect("insert a");
    a.simple_query("COMMIT").expect("commit");
    // Worker B's txn: BEGIN, INSERT, ROLLBACK — 3 statements, 1 rollback.
    b.simple_query("BEGIN").expect("begin b");
    b.simple_query("INSERT INTO debug_table VALUES (2, 'b')")
        .expect("insert b");
    b.simple_query("ROLLBACK").expect("rollback");

    drop(a);
    drop(b);
    assert_eq!(
        wait_connections_drained(&rt, 5),
        0,
        "both sessions must clean up their gauges"
    );

    let s = rt.stats();
    assert_eq!(s.statements, 6, "exactly the six statements, no double count");
    assert_eq!(s.commits, 1, "exactly A's commit");
    assert_eq!(s.rollbacks, 1, "exactly B's rollback");
    assert_eq!(s.connections, 0);
}

/// Benchmark-sized sanity (the plan's test): ~150 statements through one
/// connection — 100 autocommit INSERTs (each its own committed
/// transaction) plus 50 autocommit SELECTs — and the aggregate survives
/// with EXACT totals, then drains to a clean baseline.
#[test]
fn benchmark_sized_workload_totals_are_exact() {
    let _serial = serial();
    let (addr, rt) = boot(2);
    let mut c = client(&addr);

    for i in 0..100 {
        c.simple_query(&format!("INSERT INTO debug_table VALUES ({i}, 'row{i}')"))
            .unwrap_or_else(|e| panic!("insert {i} failed: {e}"));
    }
    for _ in 0..50 {
        c.simple_query("SELECT * FROM debug_table").expect("select");
    }

    let s = rt.stats();
    assert_eq!(s.statements, 150, "every statement counted exactly once");
    assert_eq!(s.commits, 100, "each autocommit INSERT is its own txn");
    assert_eq!(s.rollbacks, 0);
    drop(c);
    assert_eq!(
        wait_connections_drained(&rt, 5),
        0,
        "the gauge drains after the workload"
    );
}

/// FR-15's cleanup is a transaction rollback in the stats too: a client
/// that abandons a mid-flight transaction (disconnect without ROLLBACK)
/// increments `rollbacks` exactly once, through the session-tail path
/// the reaper (T4.5) also exits through.
#[test]
fn abandoned_midtxn_disconnect_counts_as_rollback() {
    let _serial = serial();
    let (addr, rt) = boot(2);
    let mut c = client(&addr);
    c.simple_query("BEGIN").expect("begin");
    c.simple_query("INSERT INTO debug_table VALUES (1, 'lost')")
        .expect("insert");
    // Abrupt abandonment: the socket dies with the transaction open
    // (dropping the sync client closes it).
    drop(c);

    assert_eq!(
        wait_connections_drained(&rt, 5),
        0,
        "the abandoned session must clean up"
    );
    let s = rt.stats();
    assert_eq!(s.rollbacks, 1, "the abandoned txn is counted as rolled back");
    assert_eq!(s.commits, 0);
    assert_eq!(s.statements, 2);
}
