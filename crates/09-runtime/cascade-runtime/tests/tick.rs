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
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
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
    let load: Vec<_> = (0..4)
        .map(|w| {
            let url = url.clone();
            std::thread::spawn(move || {
                let mut c = postgres::Client::connect(&url, postgres::NoTls)
                    .expect("load client connects");
                let deadline = std::time::Instant::now() + Duration::from_millis(LOAD_MS);
                let mut n = 0u32;
                while std::time::Instant::now() < deadline {
                    c.simple_query(&format!(
                        "INSERT INTO debug_table VALUES ({}, 'load-{}-{}')",
                        1000 + w * 10_000 + n,
                        w,
                        n
                    ))
                    .expect("load insert");
                    n += 1;
                }
                n
            })
        })
        .collect();
    let mut inserted = 0u32;
    for h in load {
        inserted += h.join().expect("load client finishes");
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
    // the log holds what all of them acked.
    assert!(inserted >= 100, "load clients only managed {inserted} inserts");
    let plan = cascade_runtime::recovery::prepare(
        &dir,
        cascade_runtime::tenant::dev_context().db,
    )
    .expect("wal walk");
    assert_eq!(
        plan.seed_rows.len(),
        inserted as usize,
        "every acked insert is in the log"
    );
}

/// Compute-only boot (no listener): nothing serves, nothing ticks (D9) —
/// the orchestrator stats are absent rather than silently zero.
#[test]
fn compute_only_boot_has_no_tick_stats() {
    let _serial = serial();
    let (topo, cfg) = config(None, 50);
    let rt = Runtime::boot(&cfg, &topo).expect("compute boot");
    assert!(
        rt.orchestrator().is_none(),
        "a listenerless boot must not pretend to tick"
    );
}
