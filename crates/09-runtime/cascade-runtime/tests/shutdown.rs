//! T4.4: the §10.2 graceful shutdown, asserted as a *sequence*.
//!
//! The plan's test review is explicit: the drain order is asserted as a
//! SEQUENCE, not just an end-state — every phase boundary lands in a
//! [`ShutdownTrace`] under one mutex in completion order, and the exact
//! vector is compared. The flushes are proven to have RAN rather than
//! been skipped as "nothing dirty" by counting the workers' pass
//! counter: with an idle tick loop (10 s interval, one test second), the
//! only pass a worker can have run is the final flush's.
//!
//! One test drives the session layer directly (draining preset, raw
//! socket) so the 57P01 refusal is deterministic rather than a race
//! against the drain window.

use std::{
    io::{Read, Write},
    net::TcpStream,
    os::unix::io::IntoRawFd,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    handoff::Handoff,
    pin,
    session::{SessionEnv, Table},
    tenant::TenantCounters,
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
        "cascade-shutdown-test-{}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed),
        tag
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

struct Fixture {
    addr: String,
    wal_dir: PathBuf,
    /// The checkpointer's counters, cloned out before `shutdown` consumes
    /// the runtime — the Arcs stay live, so `passes()` read afterwards is
    /// the workers' real total (proves the final flush RAN).
    checkpoint: Option<cascade_runtime::CheckpointStats>,
    rt: Runtime,
}

fn boot(wal: bool, tick_ms: u64, drain_deadline_s: u64) -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let wal_dir = wal.then(temp_dir_lazy);
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
        drain_deadline_s,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
    let checkpoint = rt.checkpoint_stats().cloned();
    Fixture {
        addr: format!("127.0.0.1:{port}"),
        wal_dir: wal_dir.expect("fixture asked for a wal dir"),
        checkpoint,
        rt,
    }
}

fn temp_dir_lazy() -> PathBuf {
    temp_dir("boot")
}

fn client(fx: &Fixture) -> postgres::Client {
    let url = format!("postgres://alice:pw@{}/dev", fx.addr);
    postgres::Client::connect(&url, postgres::NoTls).expect("client connects")
}

/// Raw-socket startup (trust mode) on an already-connected socket — the
/// handshake completes but the session stays *idle*: no statement ever
/// arrives, which is exactly the load the drain must not wait on.
fn raw_startup_trust(sock: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for field in ["user", "alice", "database", "dev", ""] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    sock.write_all(&(body.len() as i32 + 4).to_be_bytes())
        .unwrap();
    sock.write_all(&body).unwrap();
}

/// The whole §10.2 sequence, in order — not just the end state. The load:
/// one client, one transaction of three committed rows, cleanly
/// disconnected before shutdown. The tick loop is parked at 10 s (a test
/// second never ticks), so the pass counter proves the final flush ran
/// and was not skipped.
#[test]
fn drain_order_is_the_102_sequence() {
    let _serial = serial();
    let fx = boot(true, 10_000, 5);
    {
        let mut c = client(&fx);
        c.simple_query("BEGIN").expect("begin");
        c.simple_query("INSERT INTO debug_table VALUES (1, 'one')")
            .expect("insert 1");
        c.simple_query("INSERT INTO debug_table VALUES (2, 'two')")
            .expect("insert 2");
        c.simple_query("INSERT INTO debug_table VALUES (3, 'three')")
            .expect("insert 3");
        c.simple_query("COMMIT").expect("commit");
        // Clean Terminate: the connection drops, the gauge drains — the
        // shutdown below waits on an idle fleet.
    }
    // Baseline before shutdown: no ticks fired (10 s grid), so no pass
    // signal ever reached a worker.
    assert_eq!(
        fx.checkpoint.as_ref().expect("serving boot").passes(),
        0,
        "an idle 10s tick grid must not have flushed anything yet"
    );

    let wal = fx.rt.debug_wal().cloned().expect("wal booted");
    let started = Instant::now();
    let report = fx.rt.shutdown();
    let elapsed = started.elapsed();

    // The SEQUENCE — §10.2 verbatim, in order. Anything else (a missing
    // phase, a reordering, a "final-flush-incomplete") fails here.
    assert_eq!(
        report.phases,
        [
            "accept-stopped",
            "refuse-new",
            "sessions-drained",
            "final-wal-flush",
            "final-bpm-flush",
            "clean-marker",
            "reactors-dropped",
        ],
        "the drain order is the §10.2 sequence"
    );
    assert_eq!(report.housekeepers_stopped, 1, "the housekeeper joined");
    assert_eq!(report.workers_stopped, 2, "both reactors joined");
    assert!(
        elapsed < Duration::from_secs(4),
        "an idle fleet drains in well under the 5s deadline: {elapsed:?}"
    );
    // The final pass RAN on the designated reactor — unconditionally, not
    // gated on "nothing dirty" (the live pool scans nothing; the trace's
    // `final-bpm-flush` plus this counter is the proof it was not skipped).
    assert_eq!(
        fx.checkpoint.as_ref().expect("serving boot").passes(),
        1,
        "exactly one pass — the final flush's"
    );
    // The WAL tail is fully durable: what was written is what was synced.
    assert_eq!(
        wal.durable(),
        wal.filled(),
        "final flush synced the whole tail"
    );
    // The clean marker exists (fsynced by the runtime).
    let db = DatabaseId(0);
    let marker = fx.wal_dir.join(format!("db_{}.clean", db.0));
    assert!(marker.exists(), "the clean marker was written");

    // Restart semantics: recovery reads an unclean-free log with the
    // clean LSN stamped and replays exactly the committed rows.
    let plan = cascade_runtime::recovery::prepare(&fx.wal_dir, db).expect("recovery reads");
    assert!(!plan.unclean, "a clean shutdown never replays a crash");
    assert!(plan.clean_lsn.is_some(), "the marker is visible to recovery");
    assert!(plan.torn_tail.is_none(), "no torn tail after a clean flush");
    assert_eq!(
        plan.seed_rows.len(),
        3,
        "the three committed rows are all the WAL holds"
    );
}

/// An idle client — connected, authenticated, silent — must not turn the
/// drain into the deadline path: the wait counts *in-flight statements*,
/// not connections (an idle txn-free backend blocks nothing in PG
/// either). The drain finishes near-instantly with `sessions-drained`,
/// never `force-abort`.
#[test]
fn idle_connections_do_not_block_the_drain() {
    let _serial = serial();
    let fx = boot(true, 10_000, 30);
    // Idle for the whole shutdown: startup completes, then silence. The
    // socket is never closed by us — the reactor drop must close it.
    let mut sock = TcpStream::connect(&fx.addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    raw_startup_trust(&mut sock);
    // Consume startup responses so the server-side writes never block.
    let mut buf = [0u8; 512];
    let _ = Read::read(&mut sock, &mut buf).unwrap_or(0);

    let started = Instant::now();
    let report = fx.rt.shutdown();
    let elapsed = started.elapsed();

    assert!(
        report.phases.contains(&"sessions-drained"),
        "the idle connection must not force the abort path: {:?}",
        report.phases
    );
    assert!(
        !report.phases.contains(&"force-abort"),
        "30s deadline must never be reached for an idle fleet"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "idle connections block nothing: drained in {elapsed:?}, \
         not the 30s deadline"
    );
    // The reactor drop closed the idle socket. Leftover startup bytes may
    // still sit in the client's receive buffer — consume until the read
    // says EOF (a lingering open socket would never reach it).
    let mut drained = 0usize;
    loop {
        let mut chunk = [0u8; 4096];
        match Read::read(&mut sock, &mut chunk) {
            Ok(0) => break,                          // EOF — closed
            Err(_) => break,                         // reset — closed
            Ok(n) => {
                drained += n;
                assert!(
                    drained < 1 << 20,
                    "socket kept producing: never closed after shutdown"
                );
            }
        }
    }
}

/// H2 (post-M5 review): a DDL in flight at SIGTERM must DRAIN, not die.
/// Pre-fix, `run_drain` blocked the serving ring in `wait_inflight`'s
/// `std::thread::sleep` loop — but the in-flight CREATE ROLE completes
/// only when the ring polls `writer_loop`, so the gauge never dropped,
/// the whole deadline burned (force-abort), the flush acks inherited a
/// zero budget, and a HEALTHY system exited shutdown with the clean
/// marker refused. The cooperative drain keeps the ring alive: the
/// bounce completes mid-drain, the gauge drops, the acks get their
/// budget, the marker stamps.
///
/// Deterministic park: the test holds the writer BEFORE the DDL is even
/// sent (testutil's hold — the env-gated WAL crash barriers are unusable
/// here: they sit in the ordinary INSERT path, and arming them
/// in-process would park every later INSERT in this binary), proves the
/// statement is in flight via the statements counter (counted at
/// `execute()` entry; it cannot COMPLETE — the writer that would apply
/// it is held — so its gauge is up), then shuts down, waits out the
/// drain's entry, and releases.
#[test]
fn ddl_in_flight_at_shutdown_drains_cleanly() {
    let _serial = serial();
    let fx = boot(true, 10_000, 5);
    cascade_runtime::testutil::hold_writer_before_apply(true);
    let addr = fx.addr.clone();

    // One CREATE ROLE from a helper thread; its ack is the first
    // discriminator (the bounce must be carried, not waited out).
    let (ddl_tx, ddl_rx) = std::sync::mpsc::channel();
    let ddl_thread = std::thread::spawn(move || {
        let mut c = postgres::Client::connect(
            &format!("postgres://alice:pw@{addr}/dev"),
            postgres::NoTls,
        )
        .expect("client connects");
        c.simple_query("CREATE ROLE h2_inflight")
            .expect("the in-flight DDL completes");
        let _ = ddl_tx.send(());
    });

    // The statement has ENTERED (counted at execute entry) and provably
    // cannot complete: the writer that would apply it is held.
    let entered = Instant::now();
    while fx.rt.stats().statements == 0 {
        assert!(
            entered.elapsed() < Duration::from_secs(10),
            "the DDL never reached the session"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // Shutdown while the DDL is mid-bounce — INLINE, because `Runtime`
    // is deliberately !Send (ring-local state); the release runs on a
    // helper thread instead (a bare bool store, no runtime needed). The
    // grace before release is LOAD-BEARING: by ~100 ms the accept loop
    // has broken and the drain entered its wait (shutdown(2) wakes a
    // blocked accept immediately) — releasing earlier would let the
    // still-running accept loop's yields complete the bounce before the
    // drain ever looked, gutting the discrimination.
    let releaser = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        cascade_runtime::testutil::hold_writer_before_apply(false);
    });
    let report = fx.rt.shutdown();
    releaser.join().expect("releaser joins");
    // The phase vector first: a regression fails HERE, loudly naming the
    // burned-deadline shape, instead of after the ack wait below.
    assert_eq!(
        report.phases,
        [
            "accept-stopped",
            "refuse-new",
            "sessions-drained",
            "final-wal-flush",
            "final-bpm-flush",
            "clean-marker",
            "reactors-dropped",
        ],
        "the drain carried the bounce to a fully clean shutdown: {:?}",
        report.phases
    );
    ddl_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the client's CREATE ROLE was acked, not abandoned");
    // Joining the client thread too: a panic inside it (connect failed,
    // the server answered with an error) must fail this test loudly, not
    // vanish into a detached thread.
    ddl_thread.join().expect("the DDL client thread joins cleanly");
    // The WAL is clean for the next boot — the marker the pre-fix drain
    // refused is exactly what a healthy system must stamp.
    let db = DatabaseId(0);
    let marker = fx.wal_dir.join(format!("db_{}.clean", db.0));
    assert!(marker.exists(), "the clean marker was written");
    let plan = cascade_runtime::recovery::prepare(&fx.wal_dir, db).expect("recovery reads");
    assert!(!plan.unclean, "a clean shutdown never replays a crash");
}

/// A listenerless boot orchestrates nothing (D9): shutdown degenerates to
/// the joins, and the trace records exactly that — no drain phases, no
/// marker, no flushes.
#[test]
fn shutdown_without_a_listener_is_quiet() {
    let _serial = serial();
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
        ..RuntimeConfig::default()
    };
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, None, AuthConfig::default(), None)
        .expect("compute-only boot");
    let report = rt.shutdown();
    assert_eq!(
        report.phases,
        ["reactors-dropped"],
        "nothing served, nothing drained — the trace says exactly that"
    );
    assert_eq!(report.housekeepers_stopped, 1);
    assert_eq!(report.workers_stopped, 2);
}

/// §10.2 step 2b, deterministically: a session whose env is already
/// draining refuses its NEXT statement with fatal PG 57P01 and closes —
/// the current statement always finishes, and the refusal costs no
/// in-flight increment (the gauge never ticks). Driven at the session
/// layer so the drain-window race is not what's under test.
#[test]
fn draining_session_refuses_the_next_statement_with_57p01() {
    // A local socket pair: the "server" end becomes the Handoff fd, the
    // client end speaks the simple protocol. The session's executor runs
    // on a HELPER thread — `session::run` only completes once the client
    // drives it to the refusal, so the test thread must stay free to do
    // the driving (running the executor inline deadlocked the first draft
    // of this test: the reads sat after `ex.run`, which never returned).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let client_sock = TcpStream::connect(addr).expect("client connects");
    let (server_sock, _peer) = listener.accept().expect("server accepts");
    let fd = server_sock.into_raw_fd();

    let env = SessionEnv {
        draining: Arc::new(AtomicBool::new(true)),
        inflight: Arc::new(AtomicU64::new(0)),
    };
    let gauge = Arc::clone(&env.inflight);
    let session_thread = std::thread::Builder::new()
        .name("shutdown-session-io".into())
        .spawn(move || {
            let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                .name("shutdown-session-ring")
                .make()
                .expect("executor builds");
            ex.run(async move {
                cascade_runtime::session::run(
                    Handoff {
                        fd,
                        peer: addr,
                        user: "alice".into(),
                        database: Some("dev".into()),
                    },
                    Rc::new(Table::default()),
                    Arc::new(cascade_runtime::WorkerCounters::new()),
                    Arc::new(TenantCounters::new()),
                    None,
                    None,
                    None, // no Tier C wiring: the drain drives this session, not DDL
                    64,
                    env,
                    None, // no reaper: the drain drives this session, not idle
                )
                .await;
            });
        })
        .expect("session thread spawns");

    let mut sock = client_sock;
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // No startup packet: in production the housekeeper's `handshake()`
    // consumes startup before the handoff, so `session::run` enters its
    // statement loop immediately — a startup message here would be a
    // protocol violation. The very first message is the refused 'Q'.
    let read_msg = |sock: &mut TcpStream| -> (u8, Vec<u8>) {
        let mut header = [0u8; 5];
        Read::read_exact(sock, &mut header).expect("message header");
        let len =
            i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        Read::read_exact(sock, &mut body).expect("message body");
        (header[0], body)
    };
    let mut q = Vec::new();
    q.push(b'Q');
    let query = "SELECT 1\0";
    q.extend_from_slice(&((query.len() + 4) as i32).to_be_bytes());
    q.extend_from_slice(query.as_bytes());
    sock.write_all(&q).expect("query sent");

    // ErrorResponse: 'E' + int32 len + fields. The SQLSTATE field ('C')
    // must carry 57P01 with the shutting-down message.
    let (tag, body) = read_msg(&mut sock);
    assert_eq!(tag, b'E', "a fatal ErrorResponse, got {tag}");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("57P01"),
        "the admin_shutdown SQLSTATE must name itself: {text}"
    );
    assert!(
        text.contains("shutting down"),
        "the refusal says why: {text}"
    );
    // Fatal: the session closed — the client reads EOF next.
    let mut byte = [0u8; 1];
    match Read::read(&mut sock, &mut byte) {
        Ok(0) => {}
        Err(_) => {}
        Ok(_) => panic!("fatal refusal must close the connection"),
    }
    // The refusal is not work: the gauge never ticked.
    assert_eq!(
        gauge.load(Ordering::Relaxed),
        0,
        "a refused statement never counted as in-flight"
    );
    // The session task ended with the fatal break — the helper joins.
    session_thread.join().expect("session thread joins cleanly");
}
