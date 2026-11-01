//! T4.5 (FR-34): the idle-connection reaper, end to end through the real
//! serving stack — tick job on the serving housekeeper, dedicated channel
//! per worker, registry bookkeeping in the session loop.
//!
//! The plan's test-review question is asserted *behaviorally*: idle is
//! measured from LAST STATEMENT COMPLETION, not connection age. The
//! discriminator test holds a connection whose AGE passes the timeout
//! while its idle-since-completion clock does not, proves it survives the
//! ticks that would condemn an age-based reaper, and only then lets the
//! clock run out.
//!
//! FR-15's disconnect cleanup is re-tested THROUGH the reaper: a
//! connection reaped mid-transaction must leave exactly the committed
//! rows in the WAL — the pending row never existed anywhere durable.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};
use cascade_types::DatabaseId;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// Pin-heavy fixtures must not overlap (deployment.rs discipline).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cascade-reaper-test-{}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed),
        tag
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

struct Fixture {
    addr: String,
    wal_dir: Option<PathBuf>,
    rt: Runtime,
}

fn boot(wal: bool, tick_ms: u64, reap_ms: u64, stall_ms: u64) -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let wal_dir = wal.then(|| temp_dir("boot"));
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
        wal_dir: wal_dir.clone(),
        tick_ms,
        idle_reap_ms: reap_ms,
        stall_reap_ms: stall_ms,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
    Fixture {
        addr: format!("127.0.0.1:{port}"),
        wal_dir,
        rt,
    }
}

fn client(fx: &Fixture) -> postgres::Client {
    let url = format!("postgres://alice:pw@{}/dev", fx.addr);
    postgres::Client::connect(&url, postgres::NoTls).expect("client connects")
}

/// Polls until the reaper has condemned `want` connections (the reap
/// needs a tick to fire after the timeout elapses — never a busy spin).
fn wait_reaped(fx: &Fixture, want: u64, secs: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let got = fx
            .rt
            .reap_stats()
            .map(|s| s.reaped())
            .unwrap_or_else(|| panic!("reaping must be enabled for this test"));
        if got >= want || Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A connection idle past the timeout is reaped — and if it was
/// mid-transaction, the reap exits through the ordinary FR-15 cleanup:
/// pending (uncommitted) rows are discarded, so the WAL replays exactly
/// the committed rows and nothing else. The pending row was never
/// WAL-appended (the WAL write happens at COMMIT), so "nothing else" is
/// asserted against the log itself, not a memory.
#[test]
fn reaped_midtxn_connection_leaves_only_committed_rows() {
    let _serial = serial();
    let fx = boot(true, 200, 600, 0);
    let mut c = client(&fx);
    // Committed — survives the reap and the restart.
    c.simple_query("BEGIN").expect("begin");
    c.simple_query("INSERT INTO debug_table VALUES (1, 'keep')")
        .expect("insert keep");
    c.simple_query("COMMIT").expect("commit");
    // Mid-txn when the reap lands — must be discarded by FR-15's path.
    c.simple_query("BEGIN").expect("begin 2");
    c.simple_query("INSERT INTO debug_table VALUES (2, 'dropped')")
        .expect("insert dropped");
    // Silence: the client parks in its next read, idle clock running.
    std::thread::sleep(Duration::from_millis(2_000));

    let stats = fx.rt.reap_stats().expect("reaping enabled");
    assert!(
        stats.signals() >= 1,
        "the tick job fanned reap signals: got {}",
        stats.signals()
    );
    let reaped = wait_reaped(&fx, 1, 3);
    assert_eq!(reaped, 1, "exactly the idle mid-txn connection was reaped");
    assert_eq!(stats.refused(), 0, "a 2s idle with a 200ms grid never fills the channel");
    drop(c);

    // FR-15 through the reaper: the committed row replays, the pending
    // one is gone — and no torn anything, the reap closed cleanly.
    let report = fx.rt.shutdown();
    assert_eq!(report.workers_stopped, 2);
    let db = DatabaseId(0);
    let plan = cascade_runtime::recovery::prepare(
        fx.wal_dir.as_ref().expect("wal fixture"),
        db,
    )
    .expect("recovery reads");
    assert_eq!(
        plan.seed_rows,
        vec![(1, "keep".to_string())],
        "the reaped txn's pending row must not exist anywhere durable"
    );
}

/// A connection that keeps completing statements is never reaped, no
/// matter how long it has been CONNECTED: `signals() > 0` proves the
/// reaper ran and chose not to condemn; every statement succeeds.
#[test]
fn active_connections_are_never_reaped() {
    let _serial = serial();
    // 300ms gaps vs a 1000ms timeout: ~700ms of margin per gap — nine
    // gaps must not give a loaded machine nine chances to overshoot into
    // a spurious reap (T4.5 review F3).
    let fx = boot(false, 200, 1000, 0);
    let mut c = client(&fx);
    // ~9 statements at ~300ms gaps: idle never exceeds ~350ms < 700ms,
    // while the reap machinery ticks (200ms) several times in between.
    for i in 0..9 {
        c.simple_query("SELECT * FROM debug_table")
            .unwrap_or_else(|e| panic!("statement {i} on an active connection failed: {e}"));
        std::thread::sleep(Duration::from_millis(300));
    }
    let stats = fx.rt.reap_stats().expect("reaping enabled");
    assert!(
        stats.signals() >= 2,
        "the reaper must have ticked during the ~2.7s window: {}",
        stats.signals()
    );
    assert_eq!(stats.reaped(), 0, "an active connection is never reaped");
    // Still fully usable at the end.
    c.simple_query("SELECT * FROM debug_table").expect("connection alive after 2.7s");
}

/// The plan's test-review demand, behavioral: idle is measured from LAST
/// STATEMENT COMPLETION, not connection age. A connection whose AGE
/// passes the timeout while its since-completion clock does not survives
/// the ticks that would condemn an age-based reaper — proven by a
/// statement issued AFTER such a tick — and is only reaped once its own
/// clock runs out.
#[test]
fn idle_is_measured_from_last_completion_not_connection_age() {
    let _serial = serial();
    // Margins sized for a loaded machine (T4.5 review F2): the
    // spurious-reap window — from first completion + timeout to the
    // second statement's completion — must survive a >100ms sleep
    // overshoot. timeout 1500ms with a 1100ms first gap leaves ~400ms.
    let fx = boot(false, 200, 1500, 0);
    let mut c = client(&fx);
    let t0 = Instant::now();
    // Completion at ~t0+30ms. The connection's age now runs while its
    // idle clock is reset at every statement below.
    c.simple_query("SELECT * FROM debug_table").expect("first statement");
    // ~1.1s: second statement — idle gap ~1.07s < 1500ms.
    std::thread::sleep(Duration::from_millis(1_100));
    c.simple_query("SELECT * FROM debug_table").expect("second statement");
    // ~1.63s total age: past the timeout as an AGE. The last completion
    // was ~500ms ago, so every tick in this window must survive — and
    // the 200ms grid guarantees several ticks fired while age > 1500
    // and idle < 1500 (the exact state an age-based reaper condemns).
    std::thread::sleep(Duration::from_millis(500));
    c.simple_query("SELECT * FROM debug_table")
        .expect("connection aged past the timeout but idle-since-completion \
                 under it must NOT be reaped");
    let age = t0.elapsed();
    assert!(
        age >= Duration::from_millis(1_550),
        "test precondition: the connection must genuinely be older than \
         the timeout here (got {age:?})"
    );
    // Now let the clock actually run out: reap lands on the first tick
    // ≥ completion+1500 (~3.1s), well inside the poll budget.
    let reaped = wait_reaped(&fx, 1, 5);
    assert_eq!(reaped, 1, "only the now-genuinely-idle connection reaped");
    // The client observes the reap as a dead connection (the session
    // exited through the read-wrapper's EOF path, fd closed by FR-15).
    assert!(
        c.simple_query("SELECT * FROM debug_table").is_err(),
        "a reaped connection must be closed from the client's side too"
    );
}

/// The IDLE rule is opt-in (`idle_reap_ms == 0`, the default — a default
/// that kills idle client connections must be asked for): with the stall
/// rule also explicitly off there is no reap plane at all — no job, no
/// stats — and a connection idle well past any timeout survives. (The
/// STALL rule is the opposite: on by default, as the wedged-backend
/// containment — `the_stall_rule_arms_the_reap_plane` pins that side.)
#[test]
fn idle_reaping_is_opt_in() {
    let _serial = serial();
    let fx = boot(false, 200, 0, 0);
    assert!(
        fx.rt.reap_stats().is_none(),
        "a fully disabled reaper must not report stats"
    );
    let mut c = client(&fx);
    c.simple_query("SELECT * FROM debug_table").expect("first statement");
    std::thread::sleep(Duration::from_millis(1_200));
    c.simple_query("SELECT * FROM debug_table")
        .expect("with reaping disabled, an idle connection is never touched");
}

/// A nonzero `stall_reap_ms` arms the reap plane even with idle reaping
/// off — the wedged-backend containment rides the same plane. The value
/// here is the default (30 000, pinned in config.rs); what this test pins
/// is the on-switch: a serving boot with only the stall rule reports
/// reap stats, and a short, healthy statement is untouched by it.
#[test]
fn the_stall_rule_arms_the_reap_plane() {
    let _serial = serial();
    let fx = boot(false, 200, 0, 30_000);
    let stats = fx
        .rt
        .reap_stats()
        .expect("a stall-armed boot reports reap stats");
    assert_eq!(stats.reaped(), 0, "nothing wedged, nothing reaped");
    let mut c = client(&fx);
    c.simple_query("SELECT * FROM debug_table")
        .expect("a statement far under the stall bound completes untouched");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(stats.reaped(), 0, "healthy statements are never condemned");
}
