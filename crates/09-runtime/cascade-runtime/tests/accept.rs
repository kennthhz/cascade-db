//! T1.3: housekeeper accept loop (FR-5) — one socket, no REUSEPORT, sleeps
//! between events, fds handed off without leaks.

use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::mpsc,
    time::Duration,
};

use cascade_runtime::{
    config::{NumaPool, NumaWorkers},
    pin,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

/// A short wait used only to *observe absence* of wakeups. A busy-wait loop
/// would rack up thousands of counts in this window; a correct blocking
/// accept cannot return at all without a client. Asserting exact equality
/// (not timing) keeps this robust to scheduler noise.
const OBSERVE_WINDOW: Duration = Duration::from_millis(200);

fn bind_ephemeral() -> AcceptLoop {
    AcceptLoop::bind("127.0.0.1", 0).expect("bind")
}

fn client_of(addr: std::net::SocketAddr) -> TcpStream {
    TcpStream::connect(addr).expect("client connects")
}

#[test]
fn accepts_and_hands_off_raw_fds() {
    let loop_ = bind_ephemeral();
    let addr = loop_.local_addr().unwrap();
    assert!(!loop_.reuses_port().unwrap(), "FR-5: SO_REUSEPORT must NOT be set");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop_.serve(tx));

    let mut c1 = client_of(addr);
    let mut c2 = client_of(addr);
    c1.write_all(b"one").unwrap();
    c2.write_all(b"two").unwrap();

    let a = rx.recv_timeout(Duration::from_secs(5)).expect("first conn");
    let b = rx.recv_timeout(Duration::from_secs(5)).expect("second conn");
    assert_ne!(a.peer.port(), 0);
    assert_ne!(b.peer.port(), 0);

    // The stream is the real connection: read what each client wrote.
    let mut s1 = a.stream;
    let mut s2 = b.stream;
    let mut buf = [0u8; 3];
    s1.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"one");
    s2.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"two");
    // s1/s2 drop → fds closed. No leak.
}

#[test]
fn idle_listener_never_wakes() {
    let loop_ = bind_ephemeral();
    let stats = loop_.stats().clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop_.serve(tx));

    std::thread::sleep(OBSERVE_WINDOW);
    assert_eq!(stats.accepted.load(std::sync::atomic::Ordering::Relaxed), 0,
        "no clients → accept(2) must not have returned even once (FR-5: no busy-wait)");
    assert!(rx.try_recv().is_err());
}

#[test]
fn wakeup_count_tracks_connections_only() {
    let loop_ = bind_ephemeral();
    let addr = loop_.local_addr().unwrap();
    let stats = loop_.stats().clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop_.serve(tx));

    let _c1 = client_of(addr);
    let _c2 = client_of(addr);
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();

    std::thread::sleep(OBSERVE_WINDOW);
    assert_eq!(stats.accepted.load(std::sync::atomic::Ordering::Relaxed), 2,
        "idle time between clients must not add wakeups");

    let _c3 = client_of(addr);
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(stats.accepted.load(std::sync::atomic::Ordering::Relaxed), 3);
}

/// When the router side is gone, accepted fds are closed, not leaked: after
/// the receiver drops, one more client connect must not wedge the loop, and
/// the loop exits (its thread finishes).
#[test]
fn receiver_gone_closes_fd_and_stops() {
    let loop_ = bind_ephemeral();
    let addr = loop_.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || loop_.serve(tx));
    drop(rx);

    // serve() exits on first send failure. The connect may or may not
    // succeed (listener may already be closed when the thread exits) —
    // either way the test must finish promptly and the thread must end.
    let _ = client_of(addr);
    let started = std::time::Instant::now();
    while !handle.is_finished() && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "serve() must stop when the router is gone");
}

/// Full boot integration: housekeeper core serves the listener.
#[test]
fn booted_housekeeper_serves_connections() {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![allowed[0]],
        worker_cores: vec![NumaWorkers { numa: 0, cores: vec![allowed[1], allowed[2]] }],
        numa_pools: vec![NumaPool { numa: 0, size_mb: 1 }],
        ..RuntimeConfig::default()
    };

    let listener = bind_ephemeral();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let _rt = Runtime::boot_with_listener(&cfg, &topo, Some((listener, tx)))
        .expect("boot with listener");

    let mut c = client_of(addr);
    c.write_all(b"ping").unwrap();
    let conn = rx.recv_timeout(Duration::from_secs(5)).expect("conn from booted housekeeper");
    let mut s = conn.stream;
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}
