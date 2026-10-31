//! T4.2: the checkpointer — the housekeeper decides, a worker flushes
//! in-pool, the truncatable frontier advances, and the housekeeper thread
//! provably never issues storage I/O.
//!
//! The no-I/O assertion is kernel per-task accounting
//! (`/proc/<pid>/task/<tid>/io`): bytes hit the counters at the syscall,
//! network traffic never appears in them, and no abstraction change can
//! route around the kernel's bookkeeping — the review's "instrument the
//! syscall layer" point.
//!
//! Frontier expectations are split honestly: the engine-level tests run
//! real flushes against a real store and watch the frontier advance per
//! the conservative redo rule (module doc, checkpointer.rs); the live
//! boot test asserts the frontier *stays at zero* — live workers host no
//! BPM yet, the debug engine's only durable state is the WAL itself, so
//! nothing is recyclable and the T4.3 recycler must respect that.

use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

use cascade_bpm::{Bpm, BpmConfig, PerDbAccounting};
use cascade_runtime::{
    checkpointer::{
        self, task_io_bytes, tid_named, CheckpointFanout, CheckpointStats, TruncatableLsn,
    },
    config::{AuthConfig, NumaPool, NumaWorkers},
    handoff::ControlMsg,
    orchestrator,
    pin,
    walgate::wal_gate,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology, TickJob,
};
use cascade_storage::{StorageConfig, StorageManager};
use cascade_types::{DatabaseId, Lsn, PageId, SegmentId};
use cascade_wal::{WalHandle, WalRecord, WalWriter};

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// Pin-heavy fixtures must not overlap (deployment.rs discipline).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cascade-checkpoint-test-{}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed),
        tag
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn page(db: DatabaseId, page_no: u32) -> PageId {
    PageId {
        db,
        segment: SegmentId(3),
        page_no,
    }
}

fn reactor_named(name: &str) -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name(name)
        .make()
        .expect("executor builds")
}

/// The engine-level flush pass: real WAL gate, real page store, the T3.2
/// gate re-run through the *checkpointer's* path. Shape:
///
/// | pass | dirty at start                | outcome                       | frontier |
/// |------|-------------------------------|-------------------------------|----------|
/// | 1    | p1@l1 p2@l2 p3@l3 (p3 pinned) | 2 flushed, 1 refused          | min = l1 |
/// | 2    | p3 (guard dropped)            | 1 flushed                     | advances to l3 |
/// | 3    | nothing                       | no-op                         | stays l3 |
/// | 4    | p4@l4, durable < l4           | parks on the gate (T3.2 rerun) until the WAL flush publishes | l4 |
#[test]
fn flush_pass_flushes_gated_pages_and_advances_the_frontier() {
    let dir = temp_dir("engine");
    let cleanup_dir = dir.clone();
    let db = DatabaseId(7);
    let writer = Arc::new(WalWriter::new());
    let trunc = TruncatableLsn::new();

    let ex = reactor_named("checkpoint-engine-test");
    ex.run(async move {
        let manager = StorageManager::mount(StorageConfig {
            data_dir: dir.join("pool"),
            wal_dir: dir.join("wal"),
            io_uring_entries: 64,
        })
        .expect("store mounts");
        let store = manager.local_worker(0);
        // Rc so the parked-pass task below can own them ('static —
        // spawn_local takes no borrows); the reactor-locality invariant is
        // unchanged (Rc never crosses a thread).
        let bpm = Rc::new(Bpm::new(store, BpmConfig { max_frames: 8 }, PerDbAccounting::default()));
        let trunc = Rc::new(trunc);
        bpm.set_gate(wal_gate(&writer));

        // Three WAL records, durability published through l3 up front.
        let r1 = writer.append_wal(&WalRecord::BeginTxn { xid: 1 });
        let l1 = r1.end();
        let _ = r1.fill();
        let r2 = writer.append_wal(&WalRecord::BeginTxn { xid: 2 });
        let l2 = r2.end();
        let _ = r2.fill();
        let r3 = writer.append_wal(&WalRecord::BeginTxn { xid: 3 });
        let l3 = r3.end();
        let _ = r3.fill();
        let handle = WalHandle::open(&dir.join("wal"), db)
            .await
            .expect("wal handle opens");
        writer.flush_wal(&handle, l3).await.expect("flush to l3");

        // Three stamped dirty pages; p3 pinned.
        for (page_no, lsn) in [(1u32, l1), (2, l2), (3, l3)] {
            let mut g = bpm.page(page(db, page_no)).await.expect("page loads");
            g.write_logged(lsn, |buf| buf[0] = 0xAA);
        }
        let pinned = bpm.page(page(db, 3)).await.expect("p3 pins");

        // Pass 1: two flushes, one refusal — and the floor still includes
        // the refused page's stamp (its state lives only in the log).
        let report = checkpointer::flush_pass(&bpm, db, &trunc)
            .await
            .expect("pass 1");
        assert_eq!(
            (report.scanned, report.flushed, report.refused),
            (3, 2, 1),
            "pinned page is skipped, not forced"
        );
        assert_eq!(trunc.get(), l1, "frontier = min dirty-at-start stamp");
        assert_eq!(bpm.is_dirty(&page(db, 1)), Some(false), "p1 written back");
        assert_eq!(bpm.is_dirty(&page(db, 2)), Some(false), "p2 written back");
        assert_eq!(
            bpm.is_dirty(&page(db, 3)),
            Some(true),
            "pinned p3 stays dirty"
        );

        // Pass 2: p3's guard dropped, its flush lifts the frontier further.
        drop(pinned);
        let report = checkpointer::flush_pass(&bpm, db, &trunc)
            .await
            .expect("pass 2");
        assert_eq!((report.scanned, report.flushed, report.refused), (1, 1, 0));
        assert_eq!(trunc.get(), l3, "frontier advances monotonically");

        // Pass 3: nothing dirty — the frontier does not move.
        let report = checkpointer::flush_pass(&bpm, db, &trunc)
            .await
            .expect("pass 3");
        assert_eq!(report.scanned, 0);
        assert_eq!(report.floor, None);
        assert_eq!(trunc.get(), l3, "no dirty pages, no advance");

        // Pass 4 — the T3.2 gate, re-run through this path: a page stamped
        // beyond the durable watermark must park the pass until the WAL
        // flush publishes, never write ahead of it.
        let r4 = writer.append_wal(&WalRecord::BeginTxn { xid: 4 });
        let l4 = r4.end();
        let _ = r4.fill();
        {
            let mut g = bpm.page(page(db, 4)).await.expect("p4 loads");
            g.write_logged(l4, |buf| buf[0] = 0xBB);
        }
        assert!(writer.durable() < l4, "p4's stamp is undurable");
        let done = Rc::new(Cell::new(false));
        let flag = Rc::clone(&done);
        let passer = glommio::spawn_local({
            let bpm = Rc::clone(&bpm);
            let trunc = Rc::clone(&trunc);
            async move {
                checkpointer::flush_pass(&bpm, db, &trunc)
                    .await
                    .expect("pass 4");
                flag.set(true);
            }
        });
        glommio::timer::sleep(Duration::from_millis(5)).await;
        assert!(!done.get(), "undurable stamp must park the pass");
        assert_eq!(bpm.is_dirty(&page(db, 4)), Some(true), "p4 stays dirty");
        assert_eq!(trunc.get(), l3, "no advance while parked");
        writer.flush_wal(&handle, l4).await.expect("flush to l4");
        passer.await;
        assert!(done.get(), "the pass resolved once durability published");
        assert_eq!(bpm.is_dirty(&page(db, 4)), Some(false), "p4 written back");
        assert_eq!(trunc.get(), l4, "frontier advances to the flushed stamp");
    });
    let _ = std::fs::remove_dir_all(cleanup_dir);
}

/// The transport, end to end: the test's main thread plays the
/// housekeeper (its tid == pid), ticking the fanout job; the designated
/// worker's control loop receives, runs a real in-pool pass over a real
/// store (dirty pages planted in-task, the flush parked on the gate until
/// the housekeeper side publishes durability), and the frontier advances
/// — while the housekeeper thread's kernel-accounted storage I/O stays
/// exactly flat across the ticks.
#[test]
fn fanout_reaches_the_worker_and_the_housekeeper_touches_no_storage_io() {
    let _serial = serial();
    let dir = temp_dir("transport");
    let db = DatabaseId(9);

    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 2, "needs at least 2 CPUs");
    allowed.truncate(2);
    let worker_core = allowed[1];

    let (ready_tx, ready_rx) = mpsc::channel::<u64>();
    let (done_tx, done_rx) = mpsc::channel::<(u64, usize)>();
    let (ctrl_tx, ctrl_rx) = glommio::channels::shared_channel::new_bounded::<ControlMsg>(8);
    let writer = Arc::new(WalWriter::new());
    let worker_writer = Arc::clone(&writer);
    let trunc = Arc::new(TruncatableLsn::new());
    let worker_dir = dir.clone();

    // The designated reactor: plants its own dirty state in-task, then
    // serves control signals — the pass runs on ITS thread and ring.
    let worker = std::thread::Builder::new()
        .name("checkpoint-worker-test".into())
        .spawn(move || {
            let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(
                worker_core as usize,
            ))
            .name("checkpoint-worker-ring")
            .make()
            .expect("worker executor builds");
            ex.run(async move {
                let manager = StorageManager::mount(StorageConfig {
                    data_dir: worker_dir.join("pool"),
                    wal_dir: worker_dir.join("wal"),
                    io_uring_entries: 64,
                })
                .expect("store mounts");
                let store = manager.local_worker(0);
                let bpm = Bpm::new(store, BpmConfig { max_frames: 8 }, PerDbAccounting::default());
                bpm.set_gate(wal_gate(&worker_writer));

                // Four stamped dirty pages; durability stays at 0, so any
                // pass parks on the gate until the housekeeper publishes.
                // The highest stamp goes to the housekeeper — it needs the
                // flush frontier, it does not guess one.
                let mut max_stamp = Lsn(0);
                for xid in 1..=4u64 {
                    let r = worker_writer.append_wal(&WalRecord::BeginTxn { xid });
                    let l = r.end();
                    let _ = r.fill();
                    let mut g = bpm
                        .page(page(db, xid as u32))
                        .await
                        .expect("page loads");
                    g.write_logged(l, |buf| buf[0] = 0xCC);
                    max_stamp = max_stamp.max(l);
                }

                ready_tx.send(max_stamp.0).expect("ready signal");

                // The control loop: production's counts only (no live BPM
                // yet); this fixture's runs the real pass — the shape the
                // §8.1 concurrent BPM will plug in.
                let rx = ctrl_rx.connect().await;
                let mut flushed_total = 0usize;
                while let Some(msg) = rx.recv().await {
                    match msg {
                        ControlMsg::FlushPass => {
                            let report = checkpointer::flush_pass(&bpm, db, &trunc)
                                .await
                                .expect("in-pool pass");
                            flushed_total += report.flushed;
                        }
                    }
                    done_tx
                        .send((trunc.get().0, flushed_total))
                        .expect("done signal");
                }
            })
        })
        .expect("worker thread spawns");

    let max_stamp = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("worker plants its state");

    // Housekeeper side: this thread's tid == pid (main thread). Snapshot
    // kernel storage counters, tick the fanout, snapshot again.
    let hk_tid = std::process::id();
    let io_before = task_io_bytes(hk_tid).expect("housekeeper io readable");
    let passes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stats = CheckpointStats::new(Arc::clone(&passes));
    let ex = reactor_named("checkpoint-hk-test");
    ex.run(async move {
        let mut job = CheckpointFanout::new(vec![(0, ctrl_tx.connect().await)], stats);
        for ordinal in 1..=3u64 {
            TickJob::tick(
                &mut job,
                &orchestrator::Tick {
                    ordinal,
                    late: Duration::ZERO,
                },
            );
        }
        // The three signals sit in the channel; the job (and its sender)
        // drop here, so the worker's loop can end cleanly after them.
    });
    let io_after = task_io_bytes(hk_tid).expect("housekeeper io readable");
    assert_eq!(
        io_after.read_bytes, io_before.read_bytes,
        "the orchestrating thread must issue no storage reads (FR-31)"
    );
    assert_eq!(
        io_after.write_bytes, io_before.write_bytes,
        "the orchestrating thread must issue no storage writes (FR-31)"
    );

    // The housekeeper publishes durability (this DOES write — after the
    // snapshot, on its own ring and its own WAL handle, the way a
    // boot/shutdown path would). The parked pass resumes on the worker.
    let wal_log_dir = dir.join("wal");
    reactor_named("checkpoint-hk-flush").run(async move {
        let h = WalHandle::open(&wal_log_dir, db)
            .await
            .expect("hk wal handle opens");
        writer.flush_wal(&h, Lsn(max_stamp)).await.expect("hk flush");
    });

    // The three passes report.
    let mut trunc_val = 0;
    let mut flushed = 0;
    for _ in 0..3 {
        let (t, f) = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("passes complete");
        trunc_val = t;
        flushed = f;
    }
    assert!(
        trunc_val > 0,
        "the frontier advanced on the worker: {trunc_val}"
    );
    assert!(
        flushed >= 4,
        "every planted page was written back: {flushed}"
    );
    worker.join().expect("worker thread joins");
    let _ = std::fs::remove_dir_all(dir);
}

/// A serving runtime on 3 cores: ticks fan out, workers count passes, the
/// frontier stays at zero (nothing dirty — the debug engine's durable
/// truth is the whole log), and the serving housekeeper thread issues no
/// storage I/O across a full second of ticks.
#[test]
fn live_ticks_run_passes_and_the_frontier_stays_put() {
    let _serial = serial();
    let dir = temp_dir("live");
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
        wal_dir: Some(dir.clone()),
        tick_ms: 50,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");

    let stats = rt
        .checkpoint_stats()
        .expect("a serving boot orchestrates checkpoints")
        .clone();
    assert_eq!(
        rt.truncatable_lsn(),
        Some(Lsn(0)),
        "nothing flushed yet — nothing recyclable"
    );
    let hk_tid = tid_named(&format!("cascade-hk-{}", allowed[0]))
        .expect("the serving housekeeper thread is findable by name");
    let io_before = task_io_bytes(hk_tid).expect("housekeeper io readable");

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        stats.requested() >= 4,
        "~10 ticks nominal at 50ms; only {} pass signals sent",
        stats.requested()
    );
    assert_eq!(stats.refused(), 0, "an idle control channel never refuses");
    assert!(
        stats.passes() >= 4,
        "workers must run what they are signaled: {}",
        stats.passes()
    );

    std::thread::sleep(Duration::from_millis(600));
    let io_after = task_io_bytes(hk_tid).expect("housekeeper io readable");
    assert_eq!(
        io_after.read_bytes, io_before.read_bytes,
        "over a second of orchestration: no storage reads on the housekeeper"
    );
    assert_eq!(
        io_after.write_bytes, io_before.write_bytes,
        "one second of orchestration: no storage writes on the housekeeper"
    );
    assert_eq!(
        rt.truncatable_lsn(),
        Some(Lsn(0)),
        "live passes scan an empty pool — the frontier must stay at 0"
    );
}

/// A listenerless boot orchestrates nothing (D9) — but the frontier
/// exists as soon as a WAL does, for the recycler to read (T4.3).
#[test]
fn listenerless_boot_has_no_checkpoint_stats_but_a_frontier() {
    let _serial = serial();
    let dir = temp_dir("listenerless");
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
        wal_dir: Some(dir),
        ..RuntimeConfig::default()
    };
    let rt = Runtime::boot(&cfg, &topo).expect("compute boot");
    assert!(rt.checkpoint_stats().is_none());
    assert!(rt.orchestrator().is_none());
    assert_eq!(rt.truncatable_lsn(), Some(Lsn(0)));
}
