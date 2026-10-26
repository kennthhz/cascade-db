//! Smoke: the real binary boots on explicit cores, serves the debug engine
//! over the wire with durable commits, and answers SIGTERM with its
//! observables JSON — the shape the bench harness collects.

use std::{
    io::BufRead,
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use bench::Client;

static SEQ: AtomicU32 = AtomicU32::new(0);

#[test]
fn server_serves_commits_and_reports_observables() {
    let port = free_port();
    let wal_dir = std::env::temp_dir().join(format!(
        "cascade-server-smoke-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    let mut child = Command::new(env!("CARGO_BIN_EXE_cascade-server"))
        .args([
            "--port",
            &port.to_string(),
            "--hk-cores",
            "0",
            "--worker-cores",
            "1,2",
            "--wal-dir",
        ])
        .arg(&wal_dir)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn cascade-server");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = std::io::BufReader::new(stdout).lines();

    // Readiness: "cascade-server: ready on 127.0.0.1:PORT".
    let addr = loop {
        match lines.next() {
            Some(Ok(l)) if l.contains("ready on") => {
                break l[l.find(" on ").expect("ready shape") + 4..]
                    .trim()
                    .to_owned()
            }
            Some(Ok(l)) => eprintln!("[server] {l}"),
            Some(Err(e)) => panic!("server stdout: {e}"),
            None => panic!("server exited before ready"),
        }
    };

    // Durable end-to-end: three txns through the real binary.
    {
        let mut c = Client::connect(&addr).expect("client connects");
        let ins = c
            .query("INSERT INTO debug_table VALUES (1, 'one')")
            .expect("q");
        c.assert_ok(&ins, "insert");
        let txn = c
            .query("BEGIN; INSERT INTO debug_table VALUES (2, 'two'); COMMIT")
            .expect("q");
        c.assert_ok(&txn, "txn");
        let sel = c.query("SELECT * FROM debug_table").expect("q");
        c.assert_ok(&sel, "select");
    }
    // Give the runtime a beat to observe the disconnect cleanup, then ask
    // it to stop.
    std::thread::sleep(Duration::from_millis(100));

    // SIGTERM → observables JSON on stdout, then exit.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let mut observables = None;
    for line in lines.by_ref().flatten() {
        if line.contains("\"event\":\"cascade_observables\"") {
            observables = Some(line);
        }
    }
    let status = child.wait().expect("wait");
    assert!(status.success(), "server exited cleanly: {status}");

    let observables = observables.expect("observables JSON line");
    // Two durable txns (the autocommit INSERT and the explicit one); the
    // SELECT is read-only and never touches the WAL.
    assert!(
        observables.contains("\"wal_syncs\":2"),
        "2 durable commits: {observables}"
    );
    assert!(
        observables.contains("\"stall_hard_total\":0"),
        "no hard stalls: {observables}"
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("probe bind")
        .local_addr()
        .expect("addr")
        .port()
}
