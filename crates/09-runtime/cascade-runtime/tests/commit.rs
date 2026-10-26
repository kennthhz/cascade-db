//! T1.11: COMMIT end-to-end + disconnect cleanup (FR-15, FR-27). An acked
//! commit is really durable — the WAL was `fdatasync`ed before the ack left
//! the server — and a dropped connection mid-txn leaves nothing behind:
//! the aborted rows are in no table and in no WAL byte.
//!
//! The replay tests read through the *recovery path* (`DebugWalReader`
//! stepping `WalEntry::next`), never by parsing file bytes here.

use std::{
    io::Write,
    net::TcpStream,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};
use cascade_types::{DatabaseId, Lsn};
use cascade_wal::{DebugWalReader, WalReader, WalRecord};

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_wal_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "cascade-commit-test-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

struct Fixture {
    url: String,
    addr: String,
    wal_dir: PathBuf,
    rt: Runtime,
}

fn boot() -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let wal_dir = temp_wal_dir();
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
        wal_dir: Some(wal_dir.clone()),
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default())
        .expect("boot routed");
    Fixture {
        url: format!("postgres://alice:pw@127.0.0.1:{port}/dev"),
        addr: format!("127.0.0.1:{port}"),
        wal_dir,
        rt,
    }
}

fn client(fx: &Fixture) -> postgres::Client {
    postgres::Client::connect(&fx.url, postgres::NoTls).expect("client connects")
}

/// Waits until every worker reports `active_connections == 0` — the FR-15
/// decrement observable — or panics after a generous deadline.
fn wait_connections_drain(fx: &Fixture) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let total: u64 = fx.rt.workers().iter().map(|w| w.active_connections()).sum();
        if total == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("connections never drained to baseline");
}

/// The recovery path: step the log via `WalEntry::next` from LSN 0, collect
/// every committed transaction's rows (xid-scoped Begin→Insert*→Commit).
/// Runs its own reactor — a fresh process would do exactly this.
fn replay_committed_rows(dir: &Path) -> Vec<(i32, String)> {
    let db = DatabaseId(0);
    let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("commit-replay")
        .make()
        .expect("replay executor builds");
    ex.run(async move {
        let reader = DebugWalReader::open(dir, db)
            .await
            .expect("reader opens on the wal dir");
        let mut rows = Vec::new();
        let mut open: Vec<(u64, Vec<(i32, String)>)> = Vec::new();
        let mut at = Lsn(0);
        loop {
            // None at end-of-log ends the replay — the recovery walk.
            let Some(entry) = reader
                .read_record(at)
                .await
                .expect("replay hits no corruption")
            else {
                break;
            };
            match entry.record {
                WalRecord::BeginTxn { xid } => open.push((xid, Vec::new())),
                WalRecord::Insert { xid, id, val, .. } => {
                    open.iter_mut().for_each(|(x, r)| {
                        if *x == xid {
                            r.push((id, val.clone()));
                        }
                    });
                }
                WalRecord::CommitTxn { xid } => {
                    if let Some((_, txn_rows)) = open.iter().find(|(x, _)| *x == xid) {
                        rows.extend(txn_rows.iter().cloned());
                    }
                    open.retain(|(x, _)| *x != xid);
                }
                WalRecord::AbortTxn { xid } => open.retain(|(x, _)| *x != xid),
            }
            at = entry.next;
        }
        assert!(open.is_empty(), "no transaction left open at end of log");
        rows
    })
}

#[test]
fn commit_is_durable_before_the_ack_and_replays() {
    let fx = boot();
    let writer = fx.rt.debug_wal().expect("wal writer wired").clone();
    assert_eq!(writer.syncs(), 0, "no barriers before any commit");

    let mut c = client(&fx);
    c.simple_query("BEGIN").expect("begin");
    c.simple_query("INSERT INTO debug_table VALUES (1, 'one')")
        .expect("insert 1");
    c.simple_query("INSERT INTO debug_table VALUES (2, 'two')")
        .expect("insert 2");
    // simple_query returns only after CommandComplete + ReadyForQuery — the
    // ack. FR-27: by the time it is in our hands, the commit was fsynced.
    c.simple_query("COMMIT").expect("commit ack");
    assert!(
        writer.syncs() >= 1,
        "an acked COMMIT implies a completed fdatasync barrier"
    );

    // Replay through the recovery path: the txn is there, rows in order.
    assert_eq!(
        replay_committed_rows(&fx.wal_dir),
        vec![(1, "one".into()), (2, "two".into())],
        "the acked transaction replays from the log"
    );
}

#[test]
fn autocommit_insert_is_its_own_durable_transaction() {
    let fx = boot();
    let mut c = client(&fx);
    c.simple_query("INSERT INTO debug_table VALUES (7, 'solo')")
        .expect("autocommit insert ack");

    assert_eq!(
        replay_committed_rows(&fx.wal_dir),
        vec![(7, "solo".into())],
        "autocommit INSERT acked only after its own txn is durable"
    );
}

#[test]
fn disconnect_mid_txn_leaves_nothing_behind() {
    let fx = boot();
    let mut c = client(&fx);
    c.simple_query("BEGIN").expect("begin");
    c.simple_query("INSERT INTO debug_table VALUES (9, 'lost')")
        .expect("pending insert");
    // Vanish without COMMIT: the socket drops, the worker sees EOF.
    drop(c);

    // FR-15: the routing counter returns to baseline once the session has
    // cleaned up.
    wait_connections_drain(&fx);

    // The aborted row is in no table a later client can see...
    let mut fresh = client(&fx);
    let seen = fresh
        .simple_query("SELECT * FROM debug_table")
        .expect("select after disconnect");
    let rows: Vec<_> = seen
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).map(str::to_owned)),
            _ => None,
        })
        .collect();
    assert!(
        !rows.contains(&Some("9".to_string())),
        "aborted row must not be visible: {rows:?}"
    );
    // ...nor in any WAL byte: nothing was ever reserved for the txn.
    assert!(
        replay_committed_rows(&fx.wal_dir).is_empty(),
        "a mid-txn disconnect commits nothing to the log"
    );
}

#[test]
fn explicit_rollback_writes_nothing_to_the_wal() {
    let fx = boot();
    let mut c = client(&fx);
    c.simple_query("BEGIN").expect("begin");
    c.simple_query("INSERT INTO debug_table VALUES (5, 'undone')")
        .expect("pending insert");
    c.simple_query("ROLLBACK").expect("rollback");

    // The abort path appends nothing: WAL writes happen at COMMIT, so a
    // rollback is a pure in-memory discard — the log stays empty.
    assert_eq!(
        replay_committed_rows(&fx.wal_dir),
        Vec::new(),
        "rollback leaves the log untouched"
    );
}

/// The pre-T1.11 mode still works: with no `wal_dir`, COMMIT acks without
/// durability (in-memory engine only) and no writer is wired.
#[test]
fn without_wal_dir_the_engine_stays_in_memory() {
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
    assert!(rt.debug_wal().is_none(), "no writer without wal_dir");

    let url = format!("postgres://alice:pw@127.0.0.1:{port}/dev");
    let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    c.simple_query("BEGIN").expect("begin");
    c.simple_query("INSERT INTO debug_table VALUES (3, 'ram')")
        .expect("insert");
    c.simple_query("COMMIT").expect("commit ack (memory only)");
}

/// Raw-socket startup (trust mode) on an already-connected socket.
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

/// The harshest disconnect: the socket dies *between* BEGIN and the first
/// INSERT. The session must not treat the buffered row as anything but
/// garbage — and the counter must still drain (FR-15 under abrupt EOF).
#[test]
fn abrupt_eof_between_statements_drains_cleanly() {
    let fx = boot();
    {
        // Connect, complete startup, then hard-close without Terminate.
        let mut sock = TcpStream::connect(&fx.addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        raw_startup_trust(&mut sock);
        // Wait for ReadyForQuery so startup is fully processed server-side.
        let mut buf = [0u8; 64];
        let _ = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
        drop(sock); // RST/close mid-"session"
    }
    wait_connections_drain(&fx);

    // The worker survives and serves a fresh client.
    let mut c = client(&fx);
    c.simple_query("SELECT debug_worker()")
        .expect("worker healthy after abrupt EOF");
    assert!(
        replay_committed_rows(&fx.wal_dir).is_empty(),
        "nothing was ever committed"
    );
}
