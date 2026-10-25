//! T1.5: router v0 + RawFd handoff (FR-11 count component, FR-13, FR-15
//! counter part). Proves: the fd crosses to a worker, is adopted on the
//! worker's ring (not the housekeeper's), and the connection counter moves
//! up on handoff and back to zero on disconnect.
//!
//! Since T1.7 the worker runs the Tier A query loop, so "which thread owns
//! this fd" is observed through `SELECT debug_worker()` — the debug engine
//! answers with the adopting reactor's thread name.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

/// Bounded poll: waits for `pred` without fixed sleeps — robust to scheduler
/// noise, fails on timeout instead of hanging.
fn wait_until(pred: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Fixture {
    addr: String,
    rt: Runtime,
}

fn boot_two_workers() -> Fixture {
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

    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default())
        .expect("boot routed");
    Fixture {
        addr: format!("127.0.0.1:{port}"),
        rt,
    }
}

/// A raw client that completes the startup handshake and returns the live
/// stream, ready for worker-side queries.
fn connect(url: &str, user: &str) -> TcpStream {
    let mut sock = TcpStream::connect(url).expect("connect");
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for field in ["user", user, "database", "dev", ""] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    sock.write_all(&(body.len() as i32 + 4).to_be_bytes())
        .unwrap();
    sock.write_all(&body).unwrap();
    // Read messages until ReadyForQuery — the handshake runs on the
    // housekeeper before any handoff (FR-9).
    loop {
        let mut hdr = [0u8; 5];
        sock.read_exact(&mut hdr).unwrap();
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).unwrap();
        if hdr[0] == b'Z' {
            break;
        }
        assert_ne!(
            hdr[0],
            b'E',
            "unexpected error: {}",
            String::from_utf8_lossy(&payload)
        );
    }
    sock
}

/// Simple-protocol Query; collects worker replies up to ReadyForQuery.
fn query(sock: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    let mut frame = vec![b'Q'];
    let mut payload = sql.as_bytes().to_vec();
    payload.push(0);
    frame.extend_from_slice(&((payload.len() as i32 + 4).to_be_bytes()));
    frame.extend_from_slice(&payload);
    sock.write_all(&frame).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut out = Vec::new();
    loop {
        let mut hdr = [0u8; 5];
        sock.read_exact(&mut hdr).expect("reply arrives");
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).unwrap();
        if hdr[0] == b'Z' {
            return out;
        }
        out.push((hdr[0], payload));
    }
}

/// First text field of the first DataRow in a reply set.
fn scalar_text(msgs: &[(u8, Vec<u8>)]) -> String {
    for (t, p) in msgs {
        if *t == b'D' {
            let flen = u32::from_be_bytes(p[2..6].try_into().unwrap()) as usize;
            return String::from_utf8_lossy(&p[6..6 + flen]).into_owned();
        }
    }
    panic!("no DataRow in reply: {msgs:?}");
}

#[test]
fn handoff_queries_on_a_worker_thread() {
    let fx = boot_two_workers();

    let mut sock = connect(&fx.addr, "alice");
    let name = scalar_text(&query(&mut sock, "SELECT debug_worker();"));

    // Mutation check (plan T1.5 test review): if the fd were adopted on the
    // housekeeper thread, this name would be cascade-hk-* and the test fails.
    assert!(
        name.starts_with("cascade-w-"),
        "adoption must happen on a worker reactor thread, got {name:?}"
    );
}

#[test]
fn least_loaded_routing_spreads_connections() {
    let fx = boot_two_workers();

    let mut c1 = connect(&fx.addr, "alice");
    let name1 = scalar_text(&query(&mut c1, "SELECT debug_worker();"));

    // c1 is live: its worker's counter is 1, so the next connection must be
    // routed to the *other* worker.
    let mut c2 = connect(&fx.addr, "bob");
    let name2 = scalar_text(&query(&mut c2, "SELECT debug_worker();"));

    assert_ne!(
        name1, name2,
        "least-loaded routing must pick the idle worker"
    );
    let loads: Vec<u64> = fx
        .rt
        .workers()
        .iter()
        .map(|w| w.active_connections())
        .collect();
    assert_eq!(
        loads.iter().filter(|&&l| l >= 1).count(),
        2,
        "both workers hold one connection, counter visible from the router side: {loads:?}"
    );

    // FR-15 (counter part): dropping the clients returns the counters to
    // zero — the worker decrements on disconnect.
    drop(c1);
    drop(c2);
    wait_until(
        || fx.rt.workers().iter().all(|w| w.active_connections() == 0),
        "counters to return to zero",
    );
}

/// Counter accounting through the real `postgres` client: connect raises the
/// count, disconnect lowers it.
#[test]
fn postgres_client_lifecycle_moves_counter() {
    let fx = boot_two_workers();
    let before: u64 = fx.rt.workers().iter().map(|w| w.active_connections()).sum();
    assert_eq!(before, 0);

    let client = postgres::Client::connect(
        &format!("postgres://alice:pw@{}/dev", fx.addr),
        postgres::NoTls,
    )
    .expect("client connects (auth on housekeeper, then handoff)");

    wait_until(
        || {
            fx.rt
                .workers()
                .iter()
                .map(|w| w.active_connections())
                .sum::<u64>()
                == 1
        },
        "counter to rise to 1",
    );

    drop(client);
    wait_until(
        || {
            fx.rt
                .workers()
                .iter()
                .map(|w| w.active_connections())
                .sum::<u64>()
                == 0
        },
        "counter to fall back to 0",
    );
}

#[test]
fn capacity_cap_refuses_instead_of_overloading() {
    // A tiny max_conns_per_worker: the first connection fills the only
    // worker's budget… with two workers both stay routable, so use one
    // worker and cap it at 1 — the second connection must be closed, not
    // queued forever.
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 2, "needs at least 2 CPUs");
    allowed.truncate(2);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![allowed[0]],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: vec![allowed[1]],
        }],
        numa_pools: vec![NumaPool {
            numa: 0,
            size_mb: 1,
        }],
        max_conns_per_worker: 1,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let rt =
        Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default()).expect("boot");

    let mut c1 = connect(&addr, "alice");
    let name = scalar_text(&query(&mut c1, "SELECT debug_worker();"));
    assert!(
        name.starts_with("cascade-w-"),
        "first conn adopted and serving"
    );
    wait_until(
        || rt.workers()[0].active_connections() == 1,
        "first conn counted",
    );

    // Second connection completes the handshake (ReadyForQuery is sent
    // housekeeper-side, before routing), but the router refuses it with PG
    // 53300 naming the bound NUMA (FR-12, since T2.4; before that it was a
    // bare close). The refusal surfaces on the client's next read; the
    // refused conn is never adopted.
    let url2 = format!("postgres://bob:pw@{addr}/dev");
    let mut c2 = postgres::Client::connect(&url2, postgres::NoTls)
        .expect("handshake completes before routing");
    let err = c2
        .simple_query("SELECT 1")
        .expect_err("over-cap connection is refused at first use");
    let db = err.as_db_error().expect("refusal is a PG error");
    assert_eq!(db.code().code(), "53300", "FR-12 refusal code");
    assert!(
        db.message().contains("NUMA 0"),
        "refusal names the bound NUMA, got: {}",
        db.message()
    );
    assert_eq!(
        rt.workers()[0].active_connections(),
        1,
        "refused conn never adopted"
    );
}
