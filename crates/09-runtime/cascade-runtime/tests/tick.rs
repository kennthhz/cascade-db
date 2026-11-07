//! T4.1: the orchestrator tick loop, live. The unit tests pin the
//! scheduler's arithmetic; these pin the *system* property the plan asks
//! for: "timers tick, housekeeper sleeps — provably" (FR-5, D9).
//!
//! The no-busy-poll assertion is the review's tolerance question: an idle
//! housekeeper at a 50 ms tick wakes ~once per tick (each timeout return
//! discovers at most one tick), so wakeups ≈ ticks. A `loop {}`
//! regression would show ~10⁶ wakeups per second — any CI-safe upper
//! bound (here: a few multiples of the tick budget) still catches it by
//! four orders of magnitude.

use std::{
    io::Read,
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
    time::Duration,
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// Pin-heavy boots (1 housekeeper + 2 workers per runtime) must not
/// overlap: two runtimes fighting over the same cores trip boot and tick
/// deadlines for contention, not bugs. Same discipline as crash.rs /
/// deployment.rs.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_wal_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cascade-tick-test-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

struct Fixture {
    url: String,
    rt: Runtime,
}

fn config(wal_dir: Option<&Path>, tick_ms: u64) -> (SyntheticTopology, RuntimeConfig) {
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
        wal_dir: wal_dir.map(|p| p.to_path_buf()),
        // A tight stall bound (default is 30 s): this test is where the
        // lost-completion containment is exercised under load — a
        // statement wedged on an io_uring CQE that never arrives must be
        // killed and its client must recover, not hang the suite for 14
        // minutes (twice observed before the rule existed).
        stall_reap_ms: 3_000,
        tick_ms,
        ..RuntimeConfig::default()
    };
    (topo, cfg)
}

/// A serving runtime on 3 cores with the given tick interval.
fn boot_ticking(tick_ms: u64, wal_dir: &Path) -> Fixture {
    let (topo, cfg) = config(Some(wal_dir), tick_ms);
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
    // Hang diagnostics, read at fire time (Arc-backed clones): ticks frozen
    // ⇒ dead housekeeper ring; ticks advancing + reaped == 0 while a client
    // hangs ⇒ dead worker ring.
    let orch = rt.orchestrator().cloned();
    let reap = rt.reap_stats().cloned();
    cascade_runtime::testutil::on_hang(
        format!("tick-fixture runtime on 127.0.0.1:{port}"),
        move || {
            format!(
                "ticks={:?} reap={:?}",
                orch.as_ref().map(|o| (o.ticks(), o.wakeups())),
                reap.as_ref().map(|r| r.reaped())
            )
        },
    );
    Fixture {
        url: format!("postgres://alice:pw@127.0.0.1:{port}/dev"),
        rt,
    }
}

/// The plan's idle test: ticks fire on schedule while the housekeeper
/// provably sleeps between them (FR-5).
#[test]
fn idle_server_ticks_without_busy_polling() {
    let _serial = serial();
    let dir = temp_wal_dir();
    let fx = boot_ticking(50, &dir);
    let stats = fx
        .rt
        .orchestrator()
        .expect("a serving boot exposes tick stats")
        .clone();
    assert_eq!(stats.interval(), Duration::from_millis(50));

    let (t0, w0) = (stats.ticks(), stats.wakeups());
    std::thread::sleep(Duration::from_millis(600));
    let ticks = stats.ticks() - t0;
    let wakeups = stats.wakeups() - w0;

    // 12 slots nominal; [4, 30] absorbs CI scheduler slop both ways.
    assert!(
        (4..=30).contains(&ticks),
        "idle 600ms at a 50ms interval fired {ticks} ticks"
    );
    // Every tick must be discovered by a timeout wakeup — and nothing
    // else wakes an idle housekeeper, so wakeups ≥ ticks.
    assert!(
        wakeups >= ticks,
        "{wakeups} wakeups cannot have discovered {ticks} ticks"
    );
    // The busy-wait catcher: a `loop {}` shows ~10⁶/s; 140 in 600ms is
    // already 4× the tick budget and still four orders of magnitude short
    // of a spin.
    assert!(
        wakeups <= 140,
        "the idle housekeeper woke {wakeups} times in 600ms — busy-wait?"
    );
}

/// The plan's skew-under-load test: with all workers busy committing,
/// ticks keep firing and their worst lateness stays bounded — the skip-late
/// grid does not let load bury the tick loop.
#[test]
fn ticks_continue_under_client_load() {
    let _serial = serial();
    let dir = temp_wal_dir();
    let fx = boot_ticking(50, &dir);
    let stats = fx.rt.orchestrator().expect("tick stats").clone();

    let (t0, w0) = (stats.ticks(), stats.wakeups());
    let url = fx.url.clone();
    // Time-bounded load: each client hammers inserts for a fixed window,
    // so the tick-loop window spans ~10 slots regardless of how fast a
    // single insert is (a fixed count would finish inside one tick).
    const LOAD_MS: u64 = 500;
    // The completion channel bounds the WAIT (a wedged client converts
    // into a loud failure with evidence instead of an eternal join — the
    // ~14-minute full-suite wedge, twice observed); the handles still
    // distinguish a panicked client (finished with Err) from a wedged
    // one (unfinished) at the timeout. The clients RETRY a dead
    // connection — with the stall reaper armed, the expected shape of a
    // lost-CQE wedge is "connection killed at ~3 s", and a PG client
    // reconnects; ids keep advancing across reconnects because the
    // killed statement's fate (logged or not) is unknowable client-side.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Vec<i32>>();
    let load: Vec<_> = (0..4)
        .map(|w| {
            let url = url.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_millis(LOAD_MS);
                let mut acked: Vec<i32> = Vec::new();
                let mut n = 0u32;
                let mut c = postgres::Client::connect(&url, postgres::NoTls)
                    .expect("load client connects");
                while std::time::Instant::now() < deadline {
                    let id = (1000 + w * 10_000 + n) as i32;
                    match c.simple_query(&format!(
                        "INSERT INTO debug_table VALUES ({id}, 'load-{w}-{n}')"
                    )) {
                        Ok(_) => {
                            acked.push(id);
                            n += 1;
                        }
                        Err(_) => {
                            if std::time::Instant::now() >= deadline {
                                break;
                            }
                            c = postgres::Client::connect(&url, postgres::NoTls)
                                .expect("load client reconnects after kill");
                        }
                    }
                }
                let _ = done_tx.send(acked);
            })
        })
        .collect();
    drop(done_tx);
    let mut acked: Vec<i32> = Vec::new();
    const JOIN_BUDGET: Duration = Duration::from_secs(30);
    let joined_at = std::time::Instant::now();
    for _ in 0..load.len() {
        let remaining = JOIN_BUDGET.saturating_sub(joined_at.elapsed());
        match done_rx.recv_timeout(remaining) {
            Ok(ids) => acked.extend(ids),
            Err(_) => {
                let finished = load.iter().filter(|h| h.is_finished()).count();
                // Root-cause evidence, not just the failure: the live
                // per-worker connection gauge and the reaper's kill count.
                // A FULL gauge means the sessions run and the wedge is in
                // a socket op; reaped > 0 means the stall rule fired and
                // a retry should have followed — reaped == 0 with a full
                // gauge means the containment itself failed.
                let gauges: Vec<u64> = fx
                    .rt
                    .workers()
                    .iter()
                    .map(|w| w.active_connections())
                    .collect();
                let reaped = fx.rt.reap_stats().map(|s| s.reaped()).unwrap_or(0);
                panic!(
                    "load clients did not all finish within {JOIN_BUDGET:?} \
                     (the load window itself is {LOAD_MS} ms): {finished}/{} \
                     threads finished (a finished thread panicked; an \
                     unfinished one is wedged on a lost response); \
                     per-worker live connection gauges {gauges:?}; \
                     stall reaps so far {reaped}",
                    load.len()
                );
            }
        }
    }
    for h in load {
        // All senders reported; the join is the thread-exit formality.
        h.join().expect("load client thread joins");
    }

    let ticks = stats.ticks() - t0;
    assert!(
        ticks >= 4,
        "500ms of load at a 50ms interval fired only {ticks} ticks"
    );
    assert!(
        stats.wakeups() - w0 >= ticks,
        "wakeups ({}) fell behind ticks ({ticks})",
        stats.wakeups() - w0
    );
    // The skew bound: no single tick may fire later than 10 intervals
    // after its deadline, no matter how busy the workers are (the
    // housekeeper's per-event work is one handshake + route — small by
    // construction, so lateness is bounded by scheduling, not backlog).
    assert!(
        stats.max_late() <= Duration::from_millis(500),
        "worst tick lateness under load was {:?} — unbounded skew",
        stats.max_late()
    );

    // The load was real, and every committed insert is durable. Read the
    // truth from the WAL, not a connection: the debug table is per-worker
    // (`Rc<Table>`), so a SELECT sees only the rows routed to its worker —
    // the log holds what all of them acked. With the stall reaper armed,
    // a killed client's last statement may have committed without its ack
    // ever arriving (the wedged op WAS the ack write), so strict equality
    // is not the invariant any more; durability is:
    //   every acked id is in the log, and the log's surplus over the
    //   acked set is bounded by the kills (at most one in-flight
    //   statement per kill can be in either state).
    assert!(
        acked.len() >= 100,
        "load clients only managed {} inserts",
        acked.len()
    );
    let plan = cascade_runtime::recovery::prepare(
        &dir,
        cascade_runtime::tenant::dev_context().db,
    )
    .expect("wal walk");
    let wal_ids: std::collections::HashSet<i32> =
        plan.seed_rows.iter().map(|(id, _)| *id).collect();
    let kills = fx.rt.reap_stats().map(|s| s.reaped()).unwrap_or(0) as usize;
    assert!(
        acked.iter().all(|id| wal_ids.contains(id)),
        "an acked insert is missing from the log"
    );
    assert!(
        wal_ids.len() <= acked.len() + kills,
        "{} log rows vs {} acked inserts and {kills} stall kills — \
         the log holds rows the clients never could have acked",
        wal_ids.len(),
        acked.len()
    );
}

/// M1 (post-M5 review): sustained connection churn — an accept backlog
/// that never drains — must not suspend the tick loop. Pre-fix,
/// `poll_due` ran ONLY in the accept-timeout arm, and churn means no
/// pass ever times out: ticks froze precisely while the server was
/// busiest (the fix moves the poll to the top of every pass).
///
/// The churn's pacing knob is H1's handshake budget: each accepted
/// stall socket parks the handshake for exactly this long, which is
/// what spaces the passes the tick poll now rides — at the 60 s
/// default one stalled socket would bury the loop past the whole
/// window, ticks or no ticks.
#[test]
fn ticks_survive_connection_churn() {
    let _serial = serial();
    let dir = temp_wal_dir();
    let (topo, mut cfg) = config(Some(&dir), 50);
    cfg.handshake_timeout_ms = 200;
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
    let stats = rt.orchestrator().expect("tick stats").clone();

    // Pre-arm the backlog and HOLD it: a connected-but-silent socket
    // only stalls the handshake while the client keeps its fd open (a
    // dropped fd is a FIN, and the handshake would end at once). Each
    // socket buys one ~200 ms pass; 24 of them bury the accept loop
    // for ~4.8 s — comfortably past the 2.5 s window.
    let mut first_stall = TcpStream::connect(format!("127.0.0.1:{port}")).expect("stall socket");
    // `_stalls`: held until scope end — that hold is the churn.
    let _stalls: Vec<TcpStream> = (0..23)
        .map(|_| TcpStream::connect(format!("127.0.0.1:{port}")).expect("stall socket"))
        .collect();

    let (t0, w0) = (stats.ticks(), stats.wakeups());
    const CHURN_MS: u64 = 2500;
    std::thread::sleep(Duration::from_millis(CHURN_MS));

    // The churn was real, not vacuous: the first stall socket was
    // accepted, parked for its budget, and CUT — the loop genuinely
    // never idled into a timeout return during the window, which is
    // exactly the condition that used to starve the ticks.
    first_stall
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut eof = [0u8; 1];
    assert!(
        matches!(first_stall.read(&mut eof), Ok(0) | Err(_)),
        "the server never processed the stall socket — churn not engaged"
    );

    let ticks = stats.ticks() - t0;
    let wakeups = stats.wakeups() - w0;
    // Passes under churn are handshake-paced (~one per 200 ms stalled
    // socket — wakeups ≈ CHURN_MS/200), and at most one tick fires per
    // pass, so 4 ticks demands ≥ 4 passes: 2.5 s buys ~12 nominal,
    // still ≥ 4 at 2× CI slop.
    assert!(
        ticks >= 4,
        "{CHURN_MS}ms of churn at a 50ms interval fired only {ticks} ticks — \
         the tick poll is not on every pass"
    );
    assert!(
        wakeups >= ticks,
        "{wakeups} wakeups cannot have discovered {ticks} ticks"
    );
    // The stall sockets live until scope end — that hold is the churn.
}

/// Compute-only boot (no listener): nothing serves, nothing ticks (D9) —
/// the orchestrator stats are absent rather than silently zero.
#[test]
fn compute_only_boot_has_no_tick_stats() {
    let _serial = serial();
    let (topo, cfg) = config(None, 50);
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot(&cfg, &topo).expect("compute boot");
    assert!(
        rt.orchestrator().is_none(),
        "a listenerless boot must not pretend to tick"
    );
}
