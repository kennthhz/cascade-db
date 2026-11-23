//! T3.3: the crash-consistency matrix, executed against the real server
//! binary. Each case drives the server into a deterministic protocol
//! point via the env-gated crash barriers (`CASCADE_TEST_BARRIER_DIR` —
//! the session writes a marker file at the phase and parks), `kill -9`s
//! it the moment the marker appears, restarts, and asserts what a client
//! sees on its *first* connection. That assertion doubles as the FR-7
//! ordering test: a client cannot connect before the socket binds, and
//! the socket binds only after replay finished.
//!
//! The matrix (row 0 of every case: one commit acked before the crash —
//! it must survive every shape):
//!
//! | crash point   | WAL bytes for the in-flight txn | replay verdict  |
//! |---------------|---------------------------------|-----------------|
//! | `reserved`    | none (reservations only)        | dropped         |
//! | `midfill`     | begin+inserts, no commit record | dropped (abort) |
//! | `synced`      | everything, `fdatasync`ed       | applied (in-doubt commit — all-or-nothing) |
//! | post-ack      | everything, acked               | applied         |
//! | SIGTERM       | everything, final flush + marker| applied; next boot is *clean* |
//!
//! Barrier protocol: the first commit of every case must *pass* through
//! all three barriers (the harness answers each marker with `<phase>.go`)
//! — that preamble is itself a live check that the barriers sit exactly
//! where `durable_commit` claims.

use std::{
    io::Read,
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use cascade_runtime::pin;

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Every case pins its servers to the same cores (`pin::current_affinity`
/// head) — two cases at once means two reactors fighting over one core,
/// where boot deadlines trip for contention, not for bugs. Serial
/// execution, same discipline as deployment.rs's sink lock.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Case {
    data_dir: PathBuf,
    wal_dir: PathBuf,
    barrier_dir: PathBuf,
    port: u16,
}

fn fresh_case() -> Case {
    let data_dir = std::env::temp_dir().join(format!(
        "cascade-crash-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let wal_dir = data_dir.join("wal");
    let barrier_dir = data_dir.join("barriers");
    std::fs::create_dir_all(&barrier_dir).expect("barrier dir");
    let port = free_port();
    Case {
        data_dir,
        wal_dir,
        barrier_dir,
        port,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("ephemeral bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn cores() -> (u32, Vec<u32>) {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);
    (allowed[0], vec![allowed[1], allowed[2]])
}

/// Starts cascade-server over the case's WAL dir. `arm_barriers` decides
/// whether the crash points are armed (first boot) or disarmed (restarts
/// — the OnceLock reads the env once, so a restart without the var never
/// parks).
fn spawn_server(case: &Case, arm_barriers: bool) -> Child {
    let (hk, workers) = cores();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cascade-server"));
    cmd.arg("--port")
        .arg(case.port.to_string())
        .arg("--hk-cores")
        .arg(hk.to_string())
        .arg("--worker-cores")
        .arg(
            workers
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .arg("--wal-dir")
        .arg(&case.wal_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if arm_barriers {
        cmd.env("CASCADE_TEST_BARRIER_DIR", &case.barrier_dir);
    }
    cmd.spawn().expect("server spawns")
}

/// First successful TCP connect: readiness AND the FR-7 property in one —
/// the socket only exists after replay finished and boot completed.
fn wait_ready(case: &Case) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if Instant::now() > deadline {
            panic!("server never became ready on 127.0.0.1:{}", case.port);
        }
        if TcpStream::connect(("127.0.0.1", case.port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn url(case: &Case) -> String {
    format!("postgres://alice:pw@127.0.0.1:{}/dev", case.port)
}

fn client(case: &Case) -> postgres::Client {
    postgres::Client::connect(&url(case), postgres::NoTls).expect("client connects")
}

fn table_rows(case: &Case) -> Vec<(i32, String)> {
    let mut c = client(case);
    let rows = c.simple_query("SELECT * FROM debug_table").expect("select");
    rows.iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some((
                r.get("id").unwrap().parse().expect("int id"),
                r.get("val").unwrap().to_owned(),
            )),
            _ => None,
        })
        .collect()
}

fn wait_marker(dir: &Path, phase: &str) {
    let marker = dir.join(phase);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "session never reached barrier {phase}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn let_pass(dir: &Path, phase: &str) {
    std::fs::write(dir.join(format!("{phase}.go")), b"1").expect("go file");
}

/// Removes every marker/answer file — the next commit must reach its
/// barrier *fresh*, or a stale marker would fire the wait instantly.
fn clear_barriers(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("barrier dir") {
        std::fs::remove_file(entry.expect("entry").path()).expect("stale barrier file removed");
    }
}

/// The row-0 preamble: one full commit cycled through every barrier —
/// proof the barriers sit exactly where `durable_commit` claims, and the
/// baseline row every crash shape must preserve.
fn commit_baseline(case: &Case) {
    let url = url(case);
    let barriers = case.barrier_dir.clone();
    let t = std::thread::spawn(move || {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("baseline connects");
        c.simple_query("INSERT INTO debug_table VALUES (1, 'committed-1')")
            .expect("baseline insert acked");
    });
    for phase in ["reserved", "midfill", "synced"] {
        wait_marker(&barriers, phase);
        let_pass(&barriers, phase);
    }
    t.join().expect("baseline commits");
}

/// The crash-shape driver: a second connection inserts and parks at the
/// chosen barrier. Returns the thread (its insert never acks when the
/// kill lands before the commit).
fn drive_to(case: &Case, phases_passed: &[&str], stop_at: &'static str) -> JoinHandle<()> {
    clear_barriers(&case.barrier_dir);
    let url = url(case);
    let barriers = case.barrier_dir.clone();
    let t = std::thread::spawn(move || {
        // The failure path is the point: the connection dies mid-commit.
        let Ok(mut c) = postgres::Client::connect(&url, postgres::NoTls) else {
            return;
        };
        let _ = c.simple_query("INSERT INTO debug_table VALUES (99, 'inflight')");
    });
    for phase in phases_passed {
        wait_marker(&barriers, phase);
        let_pass(&barriers, phase);
    }
    wait_marker(&barriers, stop_at);
    t
}

fn kill9(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let status = child.wait().expect("reaps");
    assert!(!status.success(), "kill -9 is never a clean exit");
}

fn crash_recover_stderr(child: &mut Child) -> String {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr piped")
        .read_to_string(&mut stderr)
        .expect("stderr reads");
    stderr
}

fn cleanup(case: &Case) {
    let _ = std::fs::remove_dir_all(&case.data_dir);
}

/// Kills at the named barrier; the in-flight txn's expected fate decides
/// what the restart serves. Every case also re-asserts the baseline row.
fn crash_case(phases_passed: &[&str], stop_at: &'static str, inflight_survives: bool) {
    let _serial = serial();
    let case = fresh_case();

    let mut child = spawn_server(&case, true);
    wait_ready(&case);
    commit_baseline(&case);

    let t = drive_to(&case, phases_passed, stop_at);
    kill9(&mut child);
    let _ = t.join(); // the killed insert's connection error is expected

    // Restart with barriers disarmed. The first connection a client gets
    // is post-replay (FR-7): whatever it sees *is* the recovery verdict.
    let mut child = spawn_server(&case, false);
    wait_ready(&case);
    let mut expected = vec![(1, "committed-1".to_owned())];
    if inflight_survives {
        expected.push((99, "inflight".to_owned()));
    }
    assert_eq!(
        table_rows(&case),
        expected,
        "crash at {stop_at}: replay verdict"
    );

    // M6 (storage §9): no clean marker — the log store handles its own
    // tail at open (§5.7), and the replay of the durable log is the
    // restart contract. Kill BEFORE reading stderr — read_to_string ends
    // only at the child's EOF; on a live child it blocks forever.
    kill9(&mut child);
    let stderr = crash_recover_stderr(&mut child);
    assert!(
        stderr.contains("recovered: replaying from byte"),
        "a restart must announce its replay: stderr was {stderr:?}"
    );
    cleanup(&case);
}

#[test]
fn crash_at_reserved_replays_as_nothing() {
    crash_case(&[], "reserved", false);
}

#[test]
fn crash_at_midfill_replays_as_abort() {
    crash_case(&["reserved"], "midfill", false);
}

#[test]
fn crash_at_synced_replays_the_indoubt_commit() {
    crash_case(&["reserved", "midfill"], "synced", true);
}

/// Post-ack kill: let the COMMIT reply reach the client, then kill. The
/// restart serves both rows, and the *next* commit appends to the
/// recovered log (no overwrite), surviving one more restart.
#[test]
fn crash_after_ack_preserves_and_continues() {
    let _serial = serial();
    let case = fresh_case();

    let mut child = spawn_server(&case, true);
    wait_ready(&case);
    commit_baseline(&case);

    let t = drive_to(&case, &["reserved", "midfill"], "synced");
    let_pass(&case.barrier_dir, "synced");
    t.join().expect("the insert acked before the kill");
    kill9(&mut child);

    let mut child = spawn_server(&case, false);
    wait_ready(&case);
    assert_eq!(
        table_rows(&case),
        vec![(1, "committed-1".to_owned()), (99, "inflight".to_owned())]
    );

    // Post-recovery writes must work — the writer resumed at the replayed
    // end — and survive yet another restart.
    let mut c = client(&case);
    c.simple_query("INSERT INTO debug_table VALUES (2, 'post-recovery')")
        .expect("post-recovery insert");
    drop(c);
    kill9(&mut child);

    let mut child = spawn_server(&case, false);
    wait_ready(&case);
    assert_eq!(
        table_rows(&case),
        vec![
            (1, "committed-1".to_owned()),
            (99, "inflight".to_owned()),
            (2, "post-recovery".to_owned()),
        ],
        "the post-recovery commit survives its own restart"
    );
    kill9(&mut child);
    cleanup(&case);
}

/// Graceful shutdown: SIGTERM → exit 0 → restart serves every served row
/// (M6: no marker — the log store owns the tail; replay is the contract).
#[test]
fn sigterm_exits_clean_and_restart_serves_everything() {
    let _serial = serial();
    let case = fresh_case();

    let mut child = spawn_server(&case, false);
    wait_ready(&case);
    {
        let mut c = client(&case);
        c.simple_query("INSERT INTO debug_table VALUES (7, 'graceful')")
            .expect("insert");
    }

    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().expect("reaps");
    assert!(status.success(), "SIGTERM exits 0");

    let mut child = spawn_server(&case, false);
    wait_ready(&case);
    assert_eq!(table_rows(&case), vec![(7, "graceful".to_owned())]);
    // Reap the server so the test leaves no zombie behind.
    let _ = child.kill();
    let _ = child.wait();
    cleanup(&case);
}
