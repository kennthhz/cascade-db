//! Parity: the two architectures must speak identical wire bytes. The same
//! statement batches go to cascade (in-process runtime) and to the pgmodel
//! twin (spawned postmaster+fork binary); every collected message vector
//! must be equal, and both WAL logs must hold the same record *shape*
//! sequence (xids differ by design — per-process counters).
//!
//! The one deliberate difference is `SELECT debug_worker()`: cascade names
//! its reactor thread, pgmodel names its backend process. That statement is
//! excluded here; it is the observable we *want* to differ.

use std::{
    io::BufRead,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
};

use bench::{Client, Msg};
use cascade_runtime::{
    config::{NumaPool, NumaWorkers},
    pin, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};
use cascade_types::{DatabaseId, Lsn};
use cascade_wal::{DebugWalReader, WalReader, WalRecord};

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_wal_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bench-parity-{tag}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("probe bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Spawn the pgmodel twin; its stdout line "pgmodel: ready on …" is the
/// readiness barrier. The child is killed on drop so a failing assert
/// cannot leak it (a leaked child also holds the test harness's stderr
/// pipe and wedges `cargo test`).
struct Pgmodel {
    addr: String,
    child: Child,
}

impl Drop for Pgmodel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_pgmodel(wal_dir: &Path) -> Pgmodel {
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bench-pgmodel"))
        .args(["--port", &port.to_string(), "--wal-dir"])
        .arg(wal_dir)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn bench-pgmodel");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = std::io::BufReader::new(stdout).lines();
    loop {
        match lines.next() {
            Some(Ok(l)) if l.contains("ready on") => break,
            Some(Ok(l)) => eprintln!("[pgmodel] {l}"),
            Some(Err(e)) => panic!("pgmodel stdout: {e}"),
            None => panic!("pgmodel exited before ready"),
        }
    }
    Pgmodel {
        addr: format!("127.0.0.1:{port}"),
        child,
    }
}

/// Boot the real cascade runtime in-process (the same shape the commit
/// tests use), with a durable WAL so both sides run the same mode.
fn boot_cascade(wal_dir: &Path) -> (String, Runtime) {
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
        wal_dir: Some(wal_dir.to_path_buf()),
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt =
        Runtime::boot_routed(&cfg, &topo, Some(listener), Default::default()).expect("boot routed");
    (format!("127.0.0.1:{port}"), rt)
}

/// The batches both servers must answer identically. They form one session
/// story: autocommit, explicit txn, rollback, errors outside and inside a
/// txn, protocol corners.
const BATCHES: &[&str] = &[
    "INSERT INTO debug_table VALUES (1, 'one')",
    "INSERT INTO debug_table VALUES (2, 'two')",
    "SELECT * FROM debug_table",
    "SELECT id FROM debug_table",
    "SELECT val FROM debug_table",
    "BEGIN; INSERT INTO debug_table VALUES (3, 'three'); COMMIT",
    "SELECT * FROM debug_table",
    "BEGIN",
    "INSERT INTO debug_table VALUES (4, 'four')",
    "ROLLBACK",
    "SELECT * FROM debug_table",
    "COMMIT",
    "ROLLBACK",
    "BEGIN; INSERT INTO debug_table VALUES (5, 'five'); SELEKT * FROM debug_table; COMMIT",
    "SELECT * FROM debug_table",
    "COMMIT",
    "SELECT nonexistent_column FROM debug_table",
    "INSERT INTO debug_table VALUES",
    ";",
    "",
];

/// Run every batch through one fresh client; collect the message vectors.
fn drive(addr: &str) -> Vec<Vec<Msg>> {
    let mut c = Client::connect(addr).expect("client connects");
    BATCHES
        .iter()
        .map(|sql| c.query(sql).expect("query round trip"))
        .collect()
}

/// Record-shape sequence of the log (xids deliberately erased: each
/// process numbers its own transactions).
fn wal_shapes(wal_dir: &Path) -> Vec<String> {
    let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("parity-wal")
        .make()
        .expect("executor");
    ex.run(async move {
        let reader = DebugWalReader::open(wal_dir, DatabaseId(0))
            .await
            .expect("open wal");
        let mut shapes = Vec::new();
        let mut lsn = Lsn(0);
        loop {
            let Some(entry) = reader.read_record(lsn).await.expect("read ok") else {
                break;
            };
            shapes.push(match &entry.record {
                WalRecord::BeginTxn { .. } => "begin".to_owned(),
                WalRecord::Insert { id, val, .. } => format!("insert:({id},{val})"),
                WalRecord::CommitTxn { .. } => "commit".to_owned(),
                other => format!("other:{other:?}"),
            });
            lsn = entry.next;
        }
        shapes
    })
}

#[test]
fn wire_and_wal_parity_between_architectures() {
    let cascade_wal_dir = temp_wal_dir("cascade");
    let pgmodel_wal_dir = temp_wal_dir("pgmodel");

    let (cascade_addr, _rt) = boot_cascade(&cascade_wal_dir);
    let pg = spawn_pgmodel(&pgmodel_wal_dir);
    let pg_addr = &pg.addr;

    let cascade_msgs = drive(&cascade_addr);
    let pg_msgs = drive(pg_addr);

    // 1. Wire parity, batch by batch for a diffable failure.
    for (i, (a, b)) in cascade_msgs.iter().zip(pg_msgs.iter()).enumerate() {
        assert_eq!(
            a, b,
            "wire mismatch on batch {:?}\n cascade: {a:?}\n pgmodel: {b:?}",
            BATCHES[i]
        );
    }
    assert_eq!(cascade_msgs.len(), pg_msgs.len());

    // 2. Anchor a few semantics to PostgreSQL's own answers, so "both
    // identically wrong" cannot slip through the comparison above.
    let last_tag = |i: usize| -> String {
        cascade_msgs[i]
            .iter()
            .rev()
            .find_map(|m| match m {
                Msg::Tag(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let err_code = |i: usize| -> String {
        cascade_msgs[i]
            .iter()
            .find_map(|m| match m {
                Msg::Error(c, _) => Some(c.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };
    assert_eq!(last_tag(0), "INSERT 0 1");
    assert_eq!(last_tag(5), "COMMIT");
    assert_eq!(last_tag(9), "ROLLBACK"); // explicit rollback …
    assert!(!format!("{:?}", cascade_msgs[10]).contains("four")); // … 'four' gone
    assert_eq!(err_code(11), "25P01"); // COMMIT outside a txn
    assert_eq!(err_code(12), "25P01"); // ROLLBACK outside a txn
    assert_eq!(err_code(13), "42601"); // parse error mid-batch aborts the txn
    assert_eq!(err_code(14), "25P02"); // … so batch 14's SELECT is refused
    assert_eq!(last_tag(15), "ROLLBACK"); // COMMIT in an aborted txn = ROLLBACK
    assert_eq!(err_code(16), "42703"); // unknown column
    assert_eq!(err_code(17), "42601"); // malformed insert
    assert_eq!(last_tag(18), ""); // empty query: EmptyQueryResponse, no tag

    // 3. WAL parity: same committed record shapes, same order, in both
    // files (the fairness contract's byte identity, modulo per-process
    // xids).
    let cascade_shapes = wal_shapes(&cascade_wal_dir);
    let pgmodel_shapes = wal_shapes(&pgmodel_wal_dir);
    assert_eq!(cascade_shapes, pgmodel_shapes);
    assert!(cascade_shapes.contains(&"insert:(3,three)".to_owned()));
    assert!(!cascade_shapes.iter().any(|s| s.contains("five")));
}
