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
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use cascade_types::{DatabaseId, Lsn, SegmentId};
use cascade_wal::{
    log_len, read_clean_marker, truncate_log, write_clean_marker, DebugWalReader, WalError,
    WalHandle, WalReader, WalRecord, WalWriter,
};

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

// --- T3.1: group commit under concurrency -----------------------------------
//
// The plan's stress asks three things the unit shapes above don't cover:
// N writers, one barrier (fsync count ≪ commit count), the watermark
// invariants holding *while* writers run (probe), and the §8.2 per-inode
// argument — records staged through one reactor's handle covered by the
// leader's barrier through a *different* handle.

/// N-writer stress, deterministic shape: 16 sessions fill their commit
/// records first, then all flush concurrently. The first elected leader
/// drains the whole filled prefix in ONE fdatasync; every other flusher
/// resolves off the published durable LSN without writing. 16 commits,
/// 1 barrier, every waiter released exactly once (each future resolves
/// with durability covering it — no hangs, no partial acks).
#[test]
fn n_writer_stress_sixteen_commits_one_barrier() {
    const N: u64 = 16;
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h = std::rc::Rc::new(WalHandle::open(&dir, DB).await.expect("handle opens"));

        // All 16 records filled before anyone flushes: one contiguous
        // prefix, so the first leader takes it whole.
        let mut end = Lsn(0);
        for i in 0..N {
            end = fill(&insert(i, i as i32, "row"), &w);
        }

        let mut tasks = Vec::new();
        for _ in 0..N {
            let (w, h) = (Arc::clone(&w), std::rc::Rc::clone(&h));
            tasks.push(glommio::spawn_local(async move {
                w.flush_wal(&h, end).await.expect("flush resolves")
            }));
        }
        for t in tasks {
            assert_eq!(t.await, end, "every waiter released with durability");
        }
        assert_eq!(w.durable(), end);
        assert_eq!(w.syncs(), 1, "16 commits, one fdatasync");
        let (ops, bytes) = h.write_stats();
        assert_eq!(bytes, end.0, "every byte written exactly once");
        assert_eq!(ops, 1, "one batch write for the whole group");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Filled-watermark invariant probe (the plan's "probe thread"): while
/// rounds of commits drain, a concurrent task continuously samples the
/// watermarks and asserts `durable <= filled <= reserved`, plus
/// monotonicity of `durable`, `filled`, and the barrier count. One
/// fdatasync per round of 4 commits — fsync count ≪ commit count — with
/// the probe never observing a violation.
#[test]
fn watermark_invariants_hold_while_writers_drain() {
    const ROUNDS: usize = 6;
    const PER_ROUND: u64 = 4;
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");

        // The probe: samples until told to stop; any violated invariant
        // fails the test through the shared slot.
        static VIOLATION: AtomicU32 = AtomicU32::new(0);
        let probe_stop = Arc::new(AtomicU32::new(0));
        let probe = {
            let (w, stop) = (Arc::clone(&w), Arc::clone(&probe_stop));
            glommio::spawn_local(async move {
                let (mut last_d, mut last_f, mut last_s) =
                    (w.durable().0, w.filled().0, w.syncs());
                loop {
                    if stop.load(Ordering::Relaxed) == 1 {
                        return;
                    }
                    let (d, f, r, s) =
                        (w.durable().0, w.filled().0, w.reserved().0, w.syncs());
                    if d > f || f > r {
                        VIOLATION.fetch_or(1, Ordering::Relaxed);
                    }
                    if d < last_d || f < last_f || s < last_s {
                        VIOLATION.fetch_or(2, Ordering::Relaxed);
                    }
                    (last_d, last_f, last_s) = (d, f, s);
                    glommio::timer::sleep(Duration::from_micros(100)).await;
                }
            })
        };

        // Commit rounds: 4 records per round, one flush per round — the
        // leader sweeps the round's whole prefix (filled > up_to, no
        // window, no timing dependence).
        for round in 0..ROUNDS {
            let mut end = Lsn(0);
            for i in 0..PER_ROUND {
                end = fill(&insert((round as u64) * PER_ROUND + i, i as i32, "row"), &w);
            }
            let durable = w.flush_wal(&h, end).await.expect("flush resolves");
            assert_eq!(durable, end, "round {round} drained fully");
        }

        probe_stop.store(1, Ordering::Relaxed);
        probe.await;
        assert_eq!(VIOLATION.load(Ordering::Relaxed), 0, "watermark invariant violation");
        assert_eq!(
            w.syncs(),
            ROUNDS as u64,
            "{} commits drained in {} barriers",
            ROUNDS * PER_ROUND as usize,
            ROUNDS
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// §8.2 per-inode argument, in its real shape for this design: session B
/// stages its record through reactor B's handle *ownership* (B's handle is
/// the only one B would ever touch), and reactor A's leader barrier makes
/// it durable. B's handle never issues a single byte of I/O — its commit
/// landed through A's write and A's `fdatasync`, which is per-inode and
/// therefore covers everything staged by every reactor. Two handles here
/// stand in for two reactors: the property under test is handle ownership
/// (per-reactor handles), not reactor scheduling.
#[test]
fn another_reactors_commit_lands_through_this_reactors_barrier() {
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h_a = WalHandle::open(&dir, DB).await.expect("handle A opens");
        let h_b = WalHandle::open(&dir, DB).await.expect("handle B opens");

        // Both sessions fill their records (staged through the shared
        // writer; no bytes on disk yet).
        let ba = fill(&insert(1, 1, "a"), &w);
        let bb = fill(&insert(2, 2, "b"), &w);

        // B flushes through ITS handle; A flushes through its own. Whoever
        // wins election drains the whole prefix — both commits — through
        // ONE barrier; the loser resolves off the published LSN. Each task
        // reports its handle's write stats after its flush (the handles
        // are owned by their tasks, so the stats come back with them).
        let (wa, wb) = (Arc::clone(&w), Arc::clone(&w));
        let ta = glommio::spawn_local(async move {
            let durable = wa.flush_wal(&h_a, ba).await;
            (durable, h_a.write_stats())
        });
        let tb = glommio::spawn_local(async move {
            let durable = wb.flush_wal(&h_b, bb).await;
            (durable, h_b.write_stats())
        });
        let ((da, stats_a), (db, stats_b)) = futures_lite::future::zip(ta, tb).await;
        assert_eq!(da.expect("A flush"), bb, "A's barrier swept B's commit");
        assert_eq!(db.expect("B flush"), bb, "B resolved covered by A's barrier");

        assert_eq!(w.syncs(), 1, "one fdatasync for both reactors' commits");
        let (ops_a, bytes_a) = stats_a;
        let (ops_b, bytes_b) = stats_b;
        let total = bytes_a + bytes_b;
        assert_eq!(total, bb.0, "every byte written exactly once");
        assert_eq!(ops_a + ops_b, 1, "exactly one handle issued the batch write");
        assert!(
            (ops_a == 0) != (ops_b == 0),
            "the loser's handle issued no I/O — its commit rode the leader's \
             barrier: ops_a={ops_a} ops_b={ops_b}"
        );

        // And the on-disk log carries both records in order.
        let reader = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        let e1 = reader
            .read_record(Lsn(0))
            .await
            .expect("read 1 succeeds")
            .expect("rec 1 exists");
        assert_eq!(e1.record, insert(1, 1, "a"));
        let e2 = reader
            .read_record(ba)
            .await
            .expect("read 2 succeeds")
            .expect("rec 2 exists");
        assert_eq!(e2.record, insert(2, 2, "b"));
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// A flush leader abandoned mid-window (its future dropped while it holds
/// leadership in the group-commit sleep) must not wedge the log: the guard
/// releases leadership on the drop path, the next flusher is elected, and
/// the commit completes. The plan's "leader dying mid-sync — who
/// re-leads?" for the drop/unwind flavor; the bytes are idempotent by
/// offset, so a leader dropped *after* write is equally safe.
#[test]
fn abandoned_leader_releases_leadership_and_the_next_flush_completes() {
    let dir = temp_dir();
    let w = Arc::new(WalWriter::new());
    reactor().run(async {
        let h = std::rc::Rc::new(WalHandle::open(&dir, DB).await.expect("handle opens"));
        let ba = fill(&insert(1, 1, "a"), &w);

        // Leader enters the window (filled == up_to) and is dropped 50µs
        // in — nominally mid-window-sleep, mid-leadership. Glommio timers
        // on WSL2 have been observed to fire late, so the cut can land at
        // any await: mid-window, mid-write, or mid-sync. The contract under
        // test holds at every one of those points.
        let first = bounded(
            {
                let w = Arc::clone(&w);
                let h = std::rc::Rc::clone(&h);
                async move { w.flush_wal(&h, ba).await }
            },
            Duration::from_micros(50),
        )
        .await;
        match first {
            Err(()) => {} // abandoned mid-flight, as intended
            Ok(Ok(_)) => {} // completed before the cut — contract still holds
            Ok(Err(e)) => panic!("the first flush failed outright: {e}"),
        }

        // The next flusher is elected and completes the commit — the batch
        // the abandoned leader took out of staging came back, and the
        // re-written bytes (if it got that far) are idempotent by offset.
        let durable = w.flush_wal(&h, ba).await.expect("flush resolves — log not wedged");
        assert_eq!(durable, ba);
        assert_eq!(
            w.syncs(),
            1,
            "exactly one barrier across both attempts (an abandoned leader never published)"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// T3.3: boot-recovery semantics — torn tail vs mid-log corruption on the
// read side, writer resume past a replayed prefix, and the clean-marker /
// length helpers boot recovery drives.
// ---------------------------------------------------------------------------

/// Sets the log file's length to `cut` — the deterministic stand-in for a
/// crash mid-pwrite (the real thing leaves a short write at the tail; a
/// truncation is byte-for-byte what the kernel ends up with).
fn saw_off(dir: &Path, cut: u64) {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(format!("db_{}.log", DB.0)))
        .expect("raw log opens");
    f.set_len(cut).expect("truncate to the crash point");
}

/// Torn tail (wal invariant 4's read half): a record whose bytes run past
/// EOF is the crash artifact — reported as [`WalError::TornTail`], both
/// when EOF lands inside the header and when only the payload is short.
/// Complete-length records before it still decode.
#[test]
fn short_tail_reads_as_torn_not_corrupt() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let e1 = fill(&insert(1, 1, "a"), &w);
        let e2 = fill(&insert(1, 2, "bb"), &w);
        let e3 = fill(&insert(1, 3, "ccc"), &w);
        w.flush_wal(&h, e3).await.expect("flush");

        use cascade_wal::format::{CRC_LEN, HEADER_LEN};

        // Cut 1 — EOF one byte into record 3's header.
        saw_off(&dir, e2.0 + 1);
        let r = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        assert_eq!(r.len(), e2.0 + 1);
        let first = r.read_record(Lsn(0)).await.expect("decodes").expect("present");
        assert_eq!(first.next, e1);
        let second = r.read_record(e1).await.expect("decodes").expect("present");
        assert_eq!(second.next, e2);
        match r.read_record(e2).await {
            Err(WalError::TornTail(lsn)) => assert_eq!(lsn, e2),
            other => panic!("EOF mid-header must read TornTail at {e2:?}, got {other:?}"),
        }

        // Cut 2 — header and CRC complete, declared payload runs past EOF.
        saw_off(&dir, e2.0 + (HEADER_LEN + CRC_LEN) as u64);
        let r = DebugWalReader::open(&dir, DB).await.expect("reader reopens");
        match r.read_record(e2).await {
            Err(WalError::TornTail(lsn)) => assert_eq!(lsn, e2),
            other => panic!("payload past EOF must read TornTail at {e2:?}, got {other:?}"),
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the distinction: a record of *complete* length whose
/// CRC fails is bit rot or a mid-log stomp — [`WalError::Corruption`],
/// never forgiven as a tail. This is the error recovery refuses boot on.
#[test]
fn complete_length_bad_crc_is_corruption_not_torn() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let e1 = fill(&insert(1, 1, "a"), &w);
        let e2 = fill(&insert(1, 2, "bb"), &w);
        w.flush_wal(&h, e2).await.expect("flush");

        // Flip the last payload byte of record 1 — mid-log, full length,
        // version and type untouched, so the only defense is the CRC.
        let path = dir.join(format!("db_{}.log", DB.0));
        let mut raw = std::fs::read(&path).expect("log reads");
        let flip = e1.0 as usize - (cascade_wal::format::CRC_LEN + 1);
        raw[flip] ^= 0xFF;
        std::fs::write(&path, raw).expect("log writes back");

        let r = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        match r.read_record(Lsn(0)).await {
            Err(WalError::Corruption(lsn)) => assert_eq!(lsn, Lsn(0)),
            other => panic!("CRC failure must read Corruption, got {other:?}"),
        }
        // Record 2 is untouched but unreachable behind the refuse point —
        // the walker stops at the first Corruption regardless.
        match r.read_record(e1).await {
            Ok(Some(_)) => {}
            other => panic!("a later intact record still decodes: {other:?}"),
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Post-recovery resume: a writer constructed at the replayed log end
/// continues the same byte stream instead of restarting at 0 and
/// overwriting durable history. This is what makes a restarted server's
/// first commit append-safe.
#[test]
fn resume_writer_appends_after_the_replayed_prefix() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let e1 = fill(&insert(1, 1, "a"), &w);
        let e2 = fill(&insert(1, 2, "bb"), &w);
        w.flush_wal(&h, e2).await.expect("flush");
        assert_eq!(e2, w.reserved(), "test sanity: the log ends at the fill point");

        // A crash restart: fresh writer, seeded at the replayed end.
        let w = WalWriter::new_at(e2);
        assert_eq!(w.reserved(), e2);
        assert_eq!(w.filled(), e2);
        assert_eq!(w.durable(), e2);

        let h = WalHandle::open(&dir, DB).await.expect("handle reopens");
        let e3 = fill(&insert(2, 3, "cc"), &w);
        assert_eq!(e3.0, e2.0 + cascade_wal::format::encode(&insert(2, 3, "cc")).len() as u64);
        w.flush_wal(&h, e3).await.expect("flush");

        // All three records decode, each at its own LSN — nothing clobbered.
        let r = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        let first = r.read_record(Lsn(0)).await.expect("decodes").expect("present");
        assert_eq!(first.next, e1);
        let second = r.read_record(e1).await.expect("decodes").expect("present");
        assert_eq!(second.next, e2);
        let third = r.read_record(e2).await.expect("decodes").expect("present");
        assert_eq!(third.next, e3);
        assert!(r.read_record(e3).await.expect("eof").is_none());
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The file-level recovery helpers: marker round-trip (absent = `None` —
/// the unclean-shutdown signal), `log_len` on missing files, and
/// `truncate_log` dropping exactly the torn suffix.
#[test]
fn clean_marker_and_length_helpers_round_trip() {
    let dir = temp_dir();
    let w = WalWriter::new();
    reactor().run(async {
        // Fresh directory: no log, no marker — boot reads "clean slate".
        assert_eq!(log_len(&dir, DB).expect("len"), 0);
        assert_eq!(read_clean_marker(&dir, DB).expect("marker"), None);

        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let e1 = fill(&insert(1, 1, "a"), &w);
        let e2 = fill(&insert(1, 2, "bb"), &w);
        w.flush_wal(&h, e2).await.expect("flush");
        assert_eq!(log_len(&dir, DB).expect("len"), e2.0);

        // Graceful shutdown stamps the marker; boot reads it back.
        write_clean_marker(&dir, DB, e2).expect("marker writes");
        assert_eq!(read_clean_marker(&dir, DB).expect("marker"), Some(e2));

        // A torn tail gets truncated back to the last record boundary:
        // log length moves, the intact prefix still decodes, EOF ends it.
        saw_off(&dir, e2.0 + 3);
        truncate_log(&dir, DB, e2.0).expect("truncates");
        assert_eq!(log_len(&dir, DB).expect("len"), e2.0);
        let r = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        assert!(r.read_record(Lsn(0)).await.expect("decodes").is_some());
        assert!(r.read_record(e1).await.expect("decodes").is_some());
        assert!(r.read_record(e2).await.expect("eof").is_none());
    });
    let _ = std::fs::remove_dir_all(&dir);
}
