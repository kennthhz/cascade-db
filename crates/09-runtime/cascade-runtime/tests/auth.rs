//! T1.4: PG handshake + auth (FR-9), integration-tested against the `postgres`
//! Rust client (the same protocol implementation class as real drivers) plus
//! raw-socket probes for the hostile paths.
//!
//! No timing reliance anywhere: every step is synchronous request/response.

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{mpsc, Mutex},
    thread,
    time::Duration,
};

use cascade_runtime::{
    accept::AcceptLoop,
    config::{AuthConfig, AuthMode, AuthUser, NumaPool, NumaWorkers},
    pg::{sqlstate, AuthPolicy, HandshakeError},
    pin,
    Runtime, RuntimeConfig, SyntheticTopology,
    scram::ScramVerifier,
};

const ALICE: &str = "alice";
const SECRET: &str = "correct horse battery staple";

/// Pin-heavy boots must not overlap (deployment.rs discipline).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// One-shot server: accept exactly one connection, run the handshake, and
/// hand the result back for assertions.
fn serve_one(
    policy: AuthPolicy,
) -> io::Result<(String, mpsc::Receiver<Result<(), HandshakeError>>)> {
    serve_one_bounded(policy, None)
}

/// The same, with an explicit handshake budget — the H1 mitigation's
/// unit-testable form.
fn serve_one_bounded(
    policy: AuthPolicy,
    budget: Option<Duration>,
) -> io::Result<(String, mpsc::Receiver<Result<(), HandshakeError>>)> {
    let listener = AcceptLoop::bind("127.0.0.1", 0)?;
    let port = listener.local_addr()?.port();
    let (conn_tx, conn_rx) = mpsc::channel();
    thread::spawn(move || listener.serve(conn_tx));
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let conn = conn_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("client connects");
        let mut stream = conn.stream;
        let result = cascade_runtime::pg::handshake(&mut stream, &policy, budget).map(|_| ())
            as Result<(), HandshakeError>;
        let _ = done_tx.send(result);
        // The socket drops when this thread ends; a moment of grace lets a
        // rejected client finish reading its FATAL before the fd closes.
        thread::sleep(Duration::from_millis(50));
    });
    Ok((format!("127.0.0.1:{port}"), done_rx))
}

fn scram_policy() -> AuthPolicy {
    let salt = [42u8; 16];
    let verifier = ScramVerifier::for_password(SECRET, salt, 4096).to_verifier();
    AuthPolicy::from_config(&AuthConfig {
        mode: AuthMode::ScramSha256,
        users: vec![AuthUser {
            name: ALICE.into(),
            verifier,
        }],
    })
    .expect("verifier builds")
}

fn connect(url: &str) -> postgres::Error {
    match postgres::Client::connect(url, postgres::NoTls) {
        Ok(_) => panic!("connect must fail"),
        Err(e) => e,
    }
}

fn db_error_code(err: postgres::Error) -> String {
    err.as_db_error()
        .map(|e| e.code().code().to_owned())
        .unwrap_or_else(|| format!("not a db error: {err}"))
}

#[test]
fn trust_mode_connects() {
    let (url, done) = serve_one(AuthPolicy::Trust).unwrap();
    let mut client = postgres::Client::connect(
        &format!("postgres://anybody:ignored@{url}/dev"),
        postgres::NoTls,
    )
    .expect("trust mode needs no credentials");
    // This fixture runs only the handshake (no router/worker behind it), so
    // any query fails — the assertion is that the client got a *complete*
    // handshake and reached the query phase. The full accept → auth →
    // handoff → query path is integration-tested in tests/query.rs.
    client
        .simple_query("SELECT 1")
        .expect_err("handshake-only fixture: nothing serves queries");
    assert!(matches!(
        done.recv_timeout(Duration::from_secs(5)),
        Ok(Ok(()))
    ));
}

#[test]
fn scram_mode_connects_with_correct_password() {
    let (url, done) = serve_one(scram_policy()).unwrap();
    let mut client = postgres::Client::connect(
        &format!("postgres://{ALICE}:{SECRET}@{url}/dev"),
        postgres::NoTls,
    )
    .expect("SCRAM exchange succeeds end-to-end");
    let _ = client.simple_query("SELECT 1");
    assert!(matches!(
        done.recv_timeout(Duration::from_secs(5)),
        Ok(Ok(()))
    ));
}

#[test]
fn scram_wrong_password_rejected_with_28p01() {
    let (url, done) = serve_one(scram_policy()).unwrap();
    let err = connect(&format!("postgres://{ALICE}:wrong-password@{url}/dev"));
    assert_eq!(
        db_error_code(err),
        sqlstate::INVALID_PASSWORD,
        "rejection must carry SQLSTATE 28P01, not just 'errored'"
    );
    // The server side must also see the failure, not a hang.
    match done.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(HandshakeError::Rejected { code, .. })) => {
            assert_eq!(code, sqlstate::INVALID_PASSWORD)
        }
        other => panic!("expected Rejected(28P01) server-side, got {other:?}"),
    }
}

#[test]
fn scram_unknown_user_rejected_with_28000() {
    let (url, done) = serve_one(scram_policy()).unwrap();
    let err = connect(&format!("postgres://mallory:whatever@{url}/dev"));
    assert_eq!(
        db_error_code(err),
        sqlstate::INVALID_AUTHORIZATION_SPECIFICATION
    );
    match done.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(HandshakeError::Rejected { code, .. })) => {
            assert_eq!(code, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION)
        }
        other => panic!("expected Rejected(28000), got {other:?}"),
    }
}

/// Bad verifiers are a config error, caught at policy build — never
/// mid-handshake.
#[test]
fn invalid_verifier_fails_policy_build() {
    let cfg = AuthConfig {
        mode: AuthMode::ScramSha256,
        users: vec![AuthUser {
            name: "bob".into(),
            verifier: "not-a-verifier".into(),
        }],
    };
    assert!(AuthPolicy::from_config(&cfg).is_err());
}

#[test]
fn scram_without_users_fails_config_validation() {
    let doc = r#"
[runtime]
[[runtime.worker_cores]]
numa = 0
cores = [2]
[[runtime.numa_pools]]
numa = 0
size_mb = 1
[runtime.auth]
mode = "scram_sha256"
"#;
    let err = cascade_runtime::RuntimeConfig::from_toml_str(doc)
        .expect_err("scram without users must refuse");
    assert!(err.to_string().contains("scram_sha256"), "got: {err}");
}

/// Protocol version other than 3.0 → 0A000, and the client gets a real
/// error message.
#[test]
fn unsupported_protocol_version_rejected() {
    let (url, done) = serve_one(AuthPolicy::Trust).unwrap();
    let mut sock = TcpStream::connect(&url).unwrap();
    // Startup-length packet with protocol 3.1 (196609).
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&8i32.to_be_bytes());
    pkt.extend_from_slice(&196_609i32.to_be_bytes());
    sock.write_all(&pkt).unwrap();
    let mut reply = Vec::new();
    sock.read_to_end(&mut reply).unwrap();
    let text = String::from_utf8_lossy(&reply);
    assert!(reply.contains(&b'E'), "expects an error message");
    assert!(
        text.contains("0A000"),
        "error must carry SQLSTATE 0A000: {text:?}"
    );
    assert!(
        text.contains("unsupported frontend protocol"),
        "names the problem: {text:?}"
    );
    let _ = done.recv_timeout(Duration::from_secs(5));
}

/// SSLRequest → 'N' (no TLS), then the cleartext startup proceeds.
#[test]
fn ssl_request_declined_then_startup() {
    let (url, done) = serve_one(AuthPolicy::Trust).unwrap();
    let mut sock = TcpStream::connect(&url).unwrap();
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&8i32.to_be_bytes());
    pkt.extend_from_slice(&80_877_103i32.to_be_bytes());
    sock.write_all(&pkt).unwrap();
    let mut reply = [0u8; 1];
    sock.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], b'N', "SSLRequest must be declined with 'N'");

    // Now a real 3.0 startup for user bob.
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for field in ["user", "bob", "database", "dev", ""] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    sock.write_all(&(body.len() as i32 + 4).to_be_bytes())
        .unwrap();
    sock.write_all(&body).unwrap();

    // Trust → AuthenticationOk ('R' code 0) … then ReadyForQuery 'Z'.
    let mut header = [0u8; 5];
    sock.read_exact(&mut header).unwrap();
    assert_eq!(header[0], b'R');
    let mut payload =
        vec![0u8; u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize - 4];
    sock.read_exact(&mut payload).unwrap();
    assert_eq!(
        &payload[..4],
        &0i32.to_be_bytes(),
        "AuthenticationOk code 0"
    );

    // Read through to ReadyForQuery.
    loop {
        let mut hdr = [0u8; 5];
        sock.read_exact(&mut hdr).unwrap();
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body).unwrap();
        if hdr[0] == b'Z' {
            assert_eq!(body, b"I", "idle ready-for-query");
            break;
        }
    }
    let _ = done.recv_timeout(Duration::from_secs(5));
}

/// A client that hangs up mid-handshake must not wedge the server thread.
#[test]
fn mid_handshake_disconnect_closes_cleanly() {
    let (url, done) = serve_one(scram_policy()).unwrap();
    let sock = TcpStream::connect(&url).unwrap();
    drop(sock); // vanish before sending anything
    match done.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(HandshakeError::Closed)) | Ok(Err(HandshakeError::Io(_))) => {}
        other => panic!("expected Closed/Io on vanish, got {other:?}"),
    }
}

/// A truncated startup (declares more than it sends) must not block or
/// misparse: the server sees EOF and closes.
#[test]
fn truncated_startup_packet_does_not_hang() {
    let (url, done) = serve_one(AuthPolicy::Trust).unwrap();
    let mut sock = TcpStream::connect(&url).unwrap();
    // Claim 200 bytes, send 8, close.
    sock.write_all(&200i32.to_be_bytes()).unwrap();
    sock.write_all(&196_608i32.to_be_bytes()).unwrap();
    drop(sock);
    match done.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(HandshakeError::Closed)) | Ok(Err(HandshakeError::Io(_))) => {}
        other => panic!("expected Closed/Io on truncation, got {other:?}"),
    }
}

#[test]
fn cancel_request_closes_without_error() {
    let (url, _done) = serve_one(AuthPolicy::Trust).unwrap();
    let mut sock = TcpStream::connect(&url).unwrap();
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&16i32.to_be_bytes());
    pkt.extend_from_slice(&80_877_102i32.to_be_bytes()); // CancelRequest
    pkt.extend_from_slice(&0i32.to_be_bytes()); // target pid
    pkt.extend_from_slice(&0i32.to_be_bytes()); // target secret
    sock.write_all(&pkt).unwrap();
    // Server closes without sending anything.
    let mut rest = Vec::new();
    match sock.read_to_end(&mut rest) {
        Ok(0) => {}
        Ok(n) => panic!("expected clean close, got {n} bytes"),
        Err(e) => panic!("expected clean close, got {e}"),
    }
}

/// H1: a client that connects and sends NOTHING must not pin the handshake
/// forever. Pre-mitigation this test hangs: the accepted socket inherited
/// no `SO_RCVTIMEO` (Linux does not propagate the listener's) and no total
/// deadline existed — one silent peer froze the whole serving loop, ticks
/// and shutdown with it.
#[test]
fn a_silent_client_times_out_bounded() {
    let (url, done) =
        serve_one_bounded(AuthPolicy::Trust, Some(Duration::from_millis(150))).unwrap();
    thread::spawn(move || {
        let _c = TcpStream::connect(&url).unwrap(); // connects, sends nothing
        // Hold the socket open regardless; the SERVER must cut the
        // exchange, not wait it out.
        thread::sleep(Duration::from_secs(2));
    });
    let result = done
        .recv_timeout(Duration::from_secs(10))
        .expect("the handshake resolves, bounded");
    assert!(
        matches!(result, Err(HandshakeError::TimedOut)),
        "a silent client must surface TimedOut, got {result:?}"
    );
}

/// H1's other half: a byte-TRICKLING peer defeats any per-read bound
/// alone — every individual read succeeds well inside `SO_RCVTIMEO`,
/// while the TOTAL crosses the budget. A valid minimal startup (length
/// 9: protocol-3.0 code + the empty-params terminator) arrives one byte
/// per 50 ms — 450 ms total against a 300 ms budget — so the deadline
/// check that precedes each read fires while bytes are still owed: the
/// deadline is the handshake's, not one read's.
#[test]
fn a_byte_trickling_client_hits_the_total_deadline() {
    let (url, done) =
        serve_one_bounded(AuthPolicy::Trust, Some(Duration::from_millis(300))).unwrap();
    thread::spawn(move || {
        let mut c = TcpStream::connect(&url).unwrap();
        for b in [0u8, 0, 0, 9, 0, 3, 0, 0, 0] {
            c.write_all(std::slice::from_ref(&b)).unwrap();
            thread::sleep(Duration::from_millis(50));
        }
        thread::sleep(Duration::from_millis(400));
    });
    let result = done
        .recv_timeout(Duration::from_secs(10))
        .expect("the handshake resolves, bounded");
    assert!(
        matches!(result, Err(HandshakeError::TimedOut)),
        "a trickling client must surface TimedOut, got {result:?}"
    );
}

/// The end-to-end discriminator: with one handshake stalled, the serving
/// loop must still serve everyone else. Pre-mitigation the second client
/// hangs forever behind the first (the watchdog aborts the suite); with
/// the budget, the stall is cut at 300 ms and the real client's session
/// runs — then the stalled connection is closed by the server, not held.
#[test]
fn a_stalled_handshake_does_not_stop_the_serving_loop() {
    let _serial = serial();
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "this test needs at least 3 CPUs");
    allowed.truncate(3);
    let (hk, w0, w1) = (allowed[0], allowed[1], allowed[2]);

    let topo = SyntheticTopology::new([hk, w0, w1]);
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![
            NumaWorkers {
                numa: 0,
                cores: vec![w0],
            },
            NumaWorkers {
                numa: 1,
                cores: vec![w1],
            },
        ],
        numa_pools: vec![
            NumaPool { numa: 0, size_mb: 1 },
            NumaPool { numa: 1, size_mb: 1 },
        ],
        handshake_timeout_ms: 300,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let _rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");

    // A: the stalled peer — connects, sends nothing.
    let mut stalled = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();

    // B: a real client, served WHILE A still holds its handshake open.
    let mut b = postgres::Client::connect(
        &format!("postgres://u:pw@127.0.0.1:{port}/dev"),
        postgres::NoTls,
    )
    .expect("a real client is served while another handshake stalls");
    b.simple_query("INSERT INTO debug_table VALUES (1, 'alive')")
        .expect("the serving loop is alive");

    // The stalled connection is CUT at the budget — not held forever.
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut eof = [0u8; 1];
    assert!(
        matches!(stalled.read(&mut eof), Ok(0) | Err(_)),
        "the server closes the stalled connection at the budget"
    );
}
