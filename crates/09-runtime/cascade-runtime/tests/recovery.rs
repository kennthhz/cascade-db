//! T3.3: boot recovery — the plan [`recovery::prepare`] produces for every
//! crash/clean shape in the matrix, and what boot does with it. The matrix:
//!
//! | log shape                          | plan                          |
//! |------------------------------------|-------------------------------|
//! | no log                             | fresh: first boot, resume 0   |
//! | committed + uncommitted txns       | committed replayed, rest drop |
//! | torn tail (crash mid-record)       | truncated, boot continues     |
//! | mid-log CRC failure                | refuse — never serve over it  |
//! | clean marker at log end            | clean — replay still runs*    |
//! | bytes past the marker              | crash after clean: unclean    |
//!
//! \* the debug slice's WAL is the only durable state home (no page
//! checkpoints until M4), so replay is not optional even after a clean
//! stop; the marker decides the boot's *character*, not whether to walk.
//!
//! The end-to-end test runs the real loop: crash-shaped log → prepare →
//! boot → serve → more commits → restart → everything still there, and the
//! writer provably continued the log instead of overwriting it.

use std::{
    path::{Path, PathBuf},
    sync::{atomic::{AtomicU32, Ordering}, Arc},
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    recovery::{self, RecoveryError, RecoveryPlan},
    tenant,
    AcceptLoop, Runtime, RuntimeConfig,
};
use cascade_types::{DatabaseId, Lsn, SegmentId};
use cascade_wal::{DebugWalReader, WalHandle, WalReader, WalRecord, WalWriter};

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_wal_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cascade-recovery-test-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn db() -> DatabaseId {
    tenant::dev_context().db
}

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("recovery-test")
        .make()
        .expect("executor builds")
}

/// Appends one committed transaction (`begin, insert, commit`) to the log,
/// resuming at the file's current end — the same watermarks a recovery
/// boot hands its writer. Returns the commit record's end boundary. The
/// protocol is `session::durable_commit`'s, built inline so crash shapes
/// can be staged around it.
fn write_txn(dir: &Path, id: i32, val: &str) -> Lsn {
    let start = cascade_wal::log_len(dir, db()).expect("log length");
    let w = Arc::new(WalWriter::new_at(Lsn(start)));
    reactor().run(async {
        let h = WalHandle::open(dir, db()).await.expect("handle opens");
        let begin = w.append_wal(&WalRecord::BeginTxn { xid: id as u64 });
        let insert = w.append_wal(&WalRecord::Insert {
            xid: id as u64,
            table: SegmentId(1),
            id,
            val: val.to_owned(),
        });
        let commit = w.append_wal(&WalRecord::CommitTxn { xid: id as u64 });
        let end = commit.end();
        let _ = begin.fill();
        let _ = insert.fill();
        let _ = commit.fill();
        w.flush_wal(&h, end).await.expect("flush");
        end
    })
}

/// Appends `begin` + `insert` and flushes them — durable but never
/// committed. The strongest crash shape: the replay walker *can* read
/// these records and must still drop the transaction, because its commit
/// never landed.
fn write_open_txn(dir: &Path, id: i32, val: &str) {
    let start = cascade_wal::log_len(dir, db()).expect("log length");
    let w = Arc::new(WalWriter::new_at(Lsn(start)));
    reactor().run(async {
        let h = WalHandle::open(dir, db()).await.expect("handle opens");
        let begin = w.append_wal(&WalRecord::BeginTxn { xid: id as u64 });
        let insert = w.append_wal(&WalRecord::Insert {
            xid: id as u64,
            table: SegmentId(1),
            id,
            val: val.to_owned(),
        });
        let _ = begin.fill();
        let _ = insert.fill();
        let filled = w.filled();
        w.flush_wal(&h, filled).await.expect("flush");
    });
}

/// Every `Insert` record in the log, in log order — the read path a
/// recovery walk uses, never raw file bytes.
fn wal_rows(dir: &Path) -> Vec<(i32, String)> {
    reactor().run(async {
        let r = DebugWalReader::open(dir, db()).await.expect("reader opens");
        let mut rows = Vec::new();
        let mut cursor = Lsn(0);
        while let Some(entry) = r.read_record(cursor).await.expect("walks") {
            if let WalRecord::Insert { id, val, .. } = entry.record {
                rows.push((id, val));
            }
            cursor = entry.next;
        }
        rows
    })
}

fn set_len(dir: &Path, len: u64) {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(format!("db_{}.log", db().0)))
        .expect("raw log opens");
    f.set_len(len).expect("truncate to the crash point");
}

// ---- the matrix ----

#[test]
fn first_boot_plans_fresh() {
    let dir = temp_wal_dir();
    let plan = recovery::prepare(&dir, db()).expect("prepares");
    assert_eq!(plan, RecoveryPlan::fresh());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn committed_replays_and_uncommitted_drops() {
    let dir = temp_wal_dir();
    write_txn(&dir, 1, "a");
    write_open_txn(&dir, 2, "lost");

    let plan = recovery::prepare(&dir, db()).expect("prepares");
    assert_eq!(plan.seed_rows, vec![(1, "a".to_owned())], "only the commit survives");
    assert!(plan.unclean, "no marker: this boot is a crash recovery");
    assert_eq!(plan.torn_tail, None, "nothing torn — the drop is a semantic choice, not a truncation");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn torn_tail_truncates_and_boot_continues() {
    let dir = temp_wal_dir();
    let end_a = write_txn(&dir, 1, "a");
    let end_b = write_txn(&dir, 2, "bb");

    // Crash mid-record inside B: the file ends one byte into B's begin.
    set_len(&dir, end_a.0 + 1);

    let plan = recovery::prepare(&dir, db()).expect("prepares");
    assert_eq!(plan.seed_rows, vec![(1, "a".to_owned())], "A replayed; B was torn");
    assert_eq!(plan.torn_tail, Some(Lsn(end_a.0)));
    assert_eq!(plan.resume_offset, end_a.0);
    assert_eq!(
        cascade_wal::log_len(&dir, db()).expect("length"),
        end_a.0,
        "the torn suffix is gone before any worker opens a handle"
    );
    assert_eq!(end_b.0, end_a.0 + cascade_wal::format::encode(&WalRecord::BeginTxn { xid: 2 }).len() as u64 + cascade_wal::format::encode(&WalRecord::Insert { xid: 2, table: SegmentId(1), id: 2, val: "bb".into() }).len() as u64 + cascade_wal::format::encode(&WalRecord::CommitTxn { xid: 2 }).len() as u64, "test sanity: B was fully written before the cut");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mid_log_crc_failure_refuses_boot() {
    let dir = temp_wal_dir();
    write_txn(&dir, 1, "a");
    let end_a = write_txn(&dir, 2, "bb");

    // Bit rot inside A's payload — mid-log, complete length. The CRC is
    // the only defense; recovery refuses rather than serve over it.
    let path = dir.join(format!("db_{}.log", db().0));
    let mut raw = std::fs::read(&path).expect("log reads");
    let header = cascade_wal::format::HEADER_LEN;
    assert!(header + 2 < (end_a.0 as usize) - cascade_wal::format::CRC_LEN, "flip lands inside A's payload");
    raw[header + 2] ^= 0xFF;
    std::fs::write(&path, raw).expect("log writes back");

    match recovery::prepare(&dir, db()) {
        Err(RecoveryError::Corrupt { lsn }) => assert_eq!(lsn, Lsn(0)),
        other => panic!("mid-log CRC failure must refuse boot, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clean_marker_at_log_end_is_clean() {
    let dir = temp_wal_dir();
    let end_a = write_txn(&dir, 1, "a");
    cascade_wal::write_clean_marker(&dir, db(), end_a).expect("marker");

    let plan = recovery::prepare(&dir, db()).expect("prepares");
    assert!(!plan.unclean, "marker at log end: a clean stop");
    assert_eq!(plan.clean_lsn, Some(end_a));
    // The WAL is this slice's only durable state home: replay runs even
    // after a clean stop, or the table would boot empty.
    assert_eq!(plan.seed_rows, vec![(1, "a".to_owned())]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bytes_past_the_marker_are_a_crash() {
    let dir = temp_wal_dir();
    let end_a = write_txn(&dir, 1, "a");
    cascade_wal::write_clean_marker(&dir, db(), end_a).expect("marker");
    // The shape FR-7 exists for: bytes landed after the last graceful
    // sync — a crash after clean, not a clean stop.
    write_txn(&dir, 2, "bb");

    let plan = recovery::prepare(&dir, db()).expect("prepares");
    assert!(plan.unclean, "log ran past the marker: crash recovery");
    assert_eq!(plan.seed_rows, vec![(1, "a".to_owned()), (2, "bb".to_owned())]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- end-to-end: prepare → boot → serve → restart ----

struct Fixture {
    url: String,
    rt: Runtime,
}

/// Boots the runtime over `dir`, feeding it the recovery plan a real boot
/// would have computed first (FR-7 ordering lives in the server; here the
/// plan's *consumption* is under test).
fn boot_with_recovery(wal_dir: &Path) -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = cascade_runtime::SyntheticTopology::new(allowed.iter().copied());
    let plan = recovery::prepare(wal_dir, db()).expect("prepares");
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
    let rt = Runtime::boot_routed(
        &cfg,
        &topo,
        Some(listener),
        AuthConfig::default(),
        Some(&plan),
    )
    .expect("boot routed");
    Fixture {
        url: format!("postgres://alice:pw@127.0.0.1:{port}/dev"),
        rt,
    }
}

fn table_rows(url: &str) -> Vec<(i32, String)> {
    let mut c = postgres::Client::connect(url, postgres::NoTls).expect("client connects");
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

#[test]
fn boot_replays_then_restarts_preserve_everything() {
    let dir = temp_wal_dir();
    write_txn(&dir, 1, "pre-crash");

    // Boot 1 over the crash-shaped log: the replayed row is served, and
    // the writer's watermarks continue the log instead of restarting.
    let fx = boot_with_recovery(&dir);
    assert_eq!(table_rows(&fx.url), vec![(1, "pre-crash".to_owned())]);
    let w = fx.rt.debug_wal().expect("wal writer").clone();
    assert_eq!(
        w.reserved().0,
        cascade_wal::log_len(&dir, db()).expect("length"),
        "the writer resumes at the replayed end"
    );

    // This boot commits through the resumed writer — appending, never
    // overwriting, the replayed prefix.
    let mut c = postgres::Client::connect(&fx.url, postgres::NoTls).expect("client");
    c.simple_query("INSERT INTO debug_table VALUES (2, 'post-crash')")
        .expect("insert");
    drop(c);
    assert_eq!(wal_rows(&dir).len(), 2, "both txns decode in one log");
    drop(fx.rt);

    // Boot 2: the restart replays everything the previous boot acked.
    let fx = boot_with_recovery(&dir);
    assert_eq!(
        table_rows(&fx.url),
        vec![(1, "pre-crash".to_owned()), (2, "post-crash".to_owned())]
    );
    drop(fx.rt);
    let _ = std::fs::remove_dir_all(&dir);
}
