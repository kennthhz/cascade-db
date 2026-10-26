//! T1.10: the WAL durability core — reserve/fill watermarks (FR-25), leader
//! CAS group flush (FR-26), durable-LSN waiters, and torn-record detection
//! both in memory (a dropped reservation) and on disk (CRC on reopen).
//! Every test runs one reactor on the test thread over a temp dir.
//!
//! Boundary convention (the watermarks are byte offsets): a record's LSN is
//! its *first* byte; the value every flush/wait threshold uses is the
//! record's *end boundary* — `reservation.end()` — which doubles as the
//! next record's LSN.

use std::{
    io::{Seek, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use cascade_types::{DatabaseId, Lsn, SegmentId};
use cascade_wal::{DebugWalReader, WalError, WalHandle, WalReader, WalRecord, WalWriter};

const DB: DatabaseId = DatabaseId(5);

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("wal-test")
        .make()
        .expect("executor builds")
}

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "cascade-wal-test-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn insert(xid: u64, id: i32, val: &str) -> WalRecord {
    WalRecord::Insert {
        xid,
        table: SegmentId(11),
        id,
        val: val.to_owned(),
    }
}

/// Reserves `rec`, captures its end boundary, and fills. Returns the end
/// boundary — one past the record's last byte — the threshold that makes
/// THIS record durable when passed to `flush_wal` / `wait_durable`.
fn fill(rec: &WalRecord, w: &WalWriter) -> Lsn {
    let r = w.append_wal(rec);
    let end = r.end();
    let _ = r.fill();
    end
}

/// Waits `dur` for `fut`; `Ok(value)` if it resolved, `Err(())` on timeout.
/// The loser future is dropped on timeout — safe for `wait_durable` (it
/// parks with leadership released) and for a wedged `flush_wal` for the
/// same reason (the LeaderGuard only lives while leadership is held).
async fn bounded<T>(fut: impl std::future::Future<Output = T>, dur: Duration) -> Result<T, ()> {
    futures_lite::future::or(async { Ok(fut.await) }, async {
        glommio::timer::sleep(dur).await;
        Err(())
    })
    .await
}

/// FR-25: flush eligibility stops at the filled watermark. A reserved but
/// unfilled record bounds every flush — the leader must never fsync a torn
/// record, and a flush asked for more must wait, not lie.
#[test]
fn flush_stops_at_the_filled_watermark() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let b1 = fill(&insert(1, 10, "a"), &w);
        let r2 = w.append_wal(&WalRecord::CommitTxn { xid: 1 });
        assert_eq!(
            r2.start(),
            b1,
            "records are contiguous: b1 is also rec 2's LSN"
        );
        let b2 = r2.end();

        // Flushing to record 1's boundary makes exactly record 1 durable.
        let durable = w.flush_wal(&h, b1).await.expect("flush resolves");
        assert_eq!(durable, b1);
        assert_eq!(w.durable(), b1);

        // Asking past the reserved-but-unfilled record must NOT resolve:
        // the flush waits until fill() rather than reporting durability
        // over bytes that do not exist.
        let wedged = bounded(w.flush_wal(&h, b2), Duration::from_millis(50)).await;
        assert!(
            wedged.is_err(),
            "flush past an unfilled reservation must wait, never return"
        );
        assert_eq!(w.durable(), b1, "nothing past the fill boundary landed");

        // Fill completes the record; the same flush now resolves.
        let _ = r2.fill();
        let durable = w.flush_wal(&h, b2).await.expect("flush resolves");
        assert_eq!(durable, b2);
        assert_eq!(w.durable(), b2);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The torn-record test, planted deliberately: a dropped reservation tears
/// the log; the filled watermark stops before the gap forever and nothing
/// behind the gap — including later, fully written records — can become
/// durable. The wait for a covered-LSN wedges rather than lying.
#[test]
fn dropped_reservation_tears_the_log_and_flush_stops() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        let _h = WalHandle::open(&dir, DB).await.expect("handle opens");

        // Reserve, then "crash" before filling.
        let torn = w.append_wal(&insert(1, 1, "never lands"));
        drop(torn);

        // A later record fills fine — but sits behind the tear.
        let r2 = w.append_wal(&insert(2, 2, "after the tear"));
        let b2 = r2.end();
        let _ = r2.fill();

        assert_eq!(w.filled().0, 0, "the filled watermark stops at the gap");
        assert_eq!(w.durable().0, 0);
        let wedged = bounded(w.wait_durable(b2), Duration::from_millis(50)).await;
        assert!(wedged.is_err(), "a torn log must never report durability");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The plan's "panic between reserve and fill": the unwind drops the
/// reservation — the same tear as an explicit drop — and a flush attempt
/// behind it wedges instead of reporting the later record durable.
#[test]
fn panic_between_reserve_and_fill_tears_the_log() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let w_ref = &w;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _reserved = w_ref.append_wal(&WalRecord::BeginTxn { xid: 7 });
            panic!("crash mid-append");
        }));
        assert!(outcome.is_err());

        let r2 = w_ref.append_wal(&insert(7, 5, "orphan row"));
        let b2 = r2.end();
        let _ = r2.fill();
        assert_eq!(w_ref.filled().0, 0, "the tear stops the filled watermark");
        let attempted = bounded(w_ref.flush_wal(&h, b2), Duration::from_millis(50)).await;
        assert!(
            attempted.is_err(),
            "a flush behind the panic's tear must wedge, not lie"
        );
        assert_eq!(w_ref.durable().0, 0);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// FR-26: concurrent flushers elect one leader; the group is written
/// exactly once (total bytes on the wire == total record bytes, no
/// duplicated batches), and every caller sees the durable LSN cover it.
#[test]
fn concurrent_flushers_elect_one_leader_and_write_exactly_once() {
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h = std::rc::Rc::new(WalHandle::open(&dir, DB).await.expect("handle opens"));

        // Four records filled back-to-back: one contiguous prefix.
        let mut b4 = Lsn(0);
        for i in 0..4 {
            b4 = fill(&insert(i, i as i32, "row"), &w);
        }

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let (w, h) = (Arc::clone(&w), std::rc::Rc::clone(&h));
            tasks.push(glommio::spawn_local(async move {
                w.flush_wal(&h, b4).await.expect("flush succeeds")
            }));
        }
        for t in tasks {
            assert_eq!(t.await, b4, "every flusher sees durability cover it");
        }
        assert_eq!(w.durable(), b4);

        // Exactly-once: every byte written once, in however many batches
        // the racing leaders-and-fills produced.
        let (ops, bytes) = h.write_stats();
        assert_eq!(
            bytes, b4.0,
            "total bytes written == total record bytes (no duplication)"
        );
        assert!((1..=4).contains(&ops), "one leader per batch, got {ops}");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// FR-26 notification coverage: waiters for covered LSNs resolve when the
/// flush publishes durability; waiters past the filled watermark stay
/// pending.
#[test]
fn durable_notification_reaches_only_covered_waiters() {
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let b1 = fill(&insert(1, 1, "covered"), &w);
        let r2 = w.append_wal(&insert(2, 2, "not yet filled"));
        let b2 = r2.end();

        // Both waiters register BEFORE the flush. They capture only the
        // Arc and a copied Lsn — the reservation itself stays with this
        // task, unfilled.
        let waiter1 = glommio::spawn_local({
            let w = Arc::clone(&w);
            async move { w.wait_durable(b1).await }
        });
        let waiter2 = glommio::spawn_local({
            let w = Arc::clone(&w);
            async move { w.wait_durable(b2).await }
        });

        w.flush_wal(&h, b1).await.expect("flush");
        let covered = bounded(waiter1, Duration::from_secs(2))
            .await
            .expect("waiter covered by the flush resolves");
        covered.expect("the covered waiter sees no error");
        let still_pending = bounded(waiter2, Duration::from_millis(50)).await;
        assert!(
            still_pending.is_err(),
            "a waiter beyond the filled watermark must stay pending"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Torn record ON DISK (wal invariant 4): flush two records, flip a byte
/// mid-record, reopen through the recovery path — the CRC catches it at
/// that record's LSN.
#[test]
fn crc_catches_a_torn_record_on_reopen() {
    let dir = temp_dir();
    let w = WalWriter::new();
    let (b1, b2, on_disk) = reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let b1 = fill(&WalRecord::BeginTxn { xid: 9 }, &w);
        let b2 = fill(&insert(9, 1, "victim"), &w);
        w.flush_wal(&h, b2).await.expect("flush");
        (b1, b2, dir.join(format!("db_{}.log", DB.0)))
    });

    // Flip one byte mid-second-record, flushed to disk.
    let flip_at = b1.0 + (b2.0 - b1.0) / 2;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(&on_disk)
        .expect("log file exists");
    f.seek(std::io::SeekFrom::Start(flip_at)).expect("seek");
    f.write_all(&[0xFF]).expect("corrupt");
    f.sync_all().expect("flush corruption to disk");
    drop(f);

    reactor().run(async {
        let reader = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        // First record is intact.
        let entry = reader
            .read_record(Lsn(0))
            .await
            .expect("first read succeeds")
            .expect("first record exists");
        assert_eq!(entry.lsn, Lsn(0));
        assert!(matches!(entry.record, WalRecord::BeginTxn { xid: 9 }));
        // Second record carries the flipped byte.
        match reader.read_record(b1).await {
            Err(WalError::Corruption(at)) => assert_eq!(at, b1),
            other => panic!("expected Corruption at {b1:?}, got {other:?}"),
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The full debug-txn sequence survives the disk roundtrip and reads back
/// through the recovery path (the same path T1.11's replay test uses).
#[test]
fn txn_sequence_roundtrips_through_the_reader() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let b1 = fill(&WalRecord::BeginTxn { xid: 3 }, &w);
        let b2 = fill(&insert(3, 1, "alice"), &w);
        let b3 = fill(&WalRecord::CommitTxn { xid: 3 }, &w);
        let durable = w.flush_wal(&h, b3).await.expect("flush");
        assert_eq!(durable, b3);

        let reader = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        let e1 = reader
            .read_record(Lsn(0))
            .await
            .expect("read 1 succeeds")
            .expect("rec 1 exists");
        assert_eq!(e1.lsn, Lsn(0));
        assert!(matches!(e1.record, WalRecord::BeginTxn { xid: 3 }));
        let e2 = reader
            .read_record(b1)
            .await
            .expect("read 2 succeeds")
            .expect("rec 2 exists");
        assert_eq!(e2.lsn, b1);
        assert_eq!(
            e2.record,
            insert(3, 1, "alice"),
            "full row image survives the roundtrip"
        );
        let e3 = reader
            .read_record(b2)
            .await
            .expect("read 3 succeeds")
            .expect("rec 3 exists");
        assert_eq!(e3.lsn, b2);
        assert!(matches!(e3.record, WalRecord::CommitTxn { xid: 3 }));
        // The end boundaries chain: e.next is exactly where the next record
        // was written — how recovery steps without knowing lengths.
        assert_eq!(e1.next, b1);
        assert_eq!(e2.next, b2);
        let done = reader.read_record(b3).await.expect("eof check");
        assert!(done.is_none(), "end of log");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Group commit (the M2 opener fix): a flush leader drains the WHOLE filled
/// prefix, never just up to its own commit. Session A flushes its one
/// record while session B's record is already filled: A's barrier must
/// sweep B's bytes — one fdatasync covering both. The M1 shape capped the
/// drain at the leader's own boundary, so every barrier synced exactly one
/// commit and txn throughput sat pinned at the c1 level at any concurrency.
#[test]
fn leader_sweeps_the_whole_filled_prefix() {
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let ba = fill(&insert(1, 1, "a"), &w); // session A's commit record
        let bb = fill(&insert(2, 2, "b"), &w); // session B's, already filled

        let durable = w.flush_wal(&h, ba).await.expect("flush resolves");
        assert_eq!(durable, bb, "A's barrier covers B's filled record too");
        assert_eq!(w.durable(), bb);
        assert_eq!(w.syncs(), 1, "one fdatasync for both commits");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The group-commit window (the `commit_delay` analog): when the leader's
/// own commit IS the frontier, it holds the window open briefly, and a
/// record filled during the window joins the same barrier. Fills from
/// before the window skip it and drain immediately (previous test) — the
/// wait is paid only by a leader with nothing to batch yet.
#[test]
fn frontier_window_lets_a_latecomer_join_the_barrier() {
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let ba = fill(&insert(1, 1, "a"), &w);

        // Leader starts with its commit as the frontier: the window opens.
        let leader = glommio::spawn_local({
            let w = Arc::clone(&w);
            async move {
                let durable = w.flush_wal(&h, ba).await.expect("flush resolves");
                (h, durable)
            }
        });

        // A latecomer fills 50µs in — well inside the 300µs window.
        glommio::timer::sleep(Duration::from_micros(50)).await;
        let bb = fill(&insert(2, 2, "b"), &w);

        let (h, durable) = leader.await;
        drop(h);
        assert_eq!(durable, bb, "the latecomer rode the leader's barrier");
        assert_eq!(w.syncs(), 1, "one fdatasync total");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
