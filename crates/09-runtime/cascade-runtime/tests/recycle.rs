//! T4.3: the WAL recycler — segments below the per-DB truncatable LSN
//! recycle; needed ones never do (FR-32).
//!
//! The honest split, continuing the checkpointer's (T4.2): the boundary
//! math is real and pinned at the crate level (cascade-wal's recycle
//! tests, ±1 at exactly the frontier); this suite pins the system
//! properties. The live test is the strongest form available in the
//! debug engine — the frontier is permanently 0 (rows live only in the
//! log), so the recycler must run, reclaim exactly nothing, and leave
//! `db_<db>.log` byte-for-byte identical while real checkpoints tick;
//! the engine-level test proves a recycle can never cover an in-flight
//! checkpoint's bytes and can never advance the frontier itself.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use cascade_bpm::{Bpm, BpmConfig, PerDbAccounting};
use cascade_blockdev::GlommioBlockDevice;
use cascade_logstore::LogStore;
use cascade_runtime::{
    checkpointer::{self, TruncatableLsn},
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    recycler::RecycleSite,
    walgate::wal_gate,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};
use cascade_storage::{StorageConfig, StorageManager};
use cascade_types::{DatabaseId, Lsn, PageId, SegmentId};
use cascade_wal::WalRecord;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// Pin-heavy boots must not overlap (deployment.rs discipline).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cascade-recycle-test-{}-{}-{}",
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

/// The plan's live test: the recycler runs, reclaims nothing (frontier
/// 0 — the log IS the durable state), and the log file it must protect
/// is byte-for-byte identical across a full second of ticks. Every
/// committed insert is still recoverable afterwards.
/// M6: durability rides the sink — a MemLogStore-backed writer for tests
/// that need real flushes without a filesystem WAL.
fn sink_writer(
) -> (std::sync::Arc<cascade_storage_api::fake::MemLogStore>, std::sync::Arc<cascade_wal::WalWriter>) {
    use cascade_storage_api::fake::{test_ctx as sink_ctx, MemLogStore};
    use cascade_storage_api::LogScopes as _;
    futures_lite::future::block_on(async {
        let store = std::sync::Arc::new(MemLogStore::new());
        let span = cascade_telemetry::Span::noop();
        let ctx = sink_ctx(&span, 0);
        let _ = store
            .create_scope(&ctx, cascade_types::ScopeId(0), cascade_types::Epoch::FIRST,
                          cascade_storage_api::KeyRef { key_id: 1 })
            .await;
        let writer = cascade_wal::WalWriter::open_with_sink(
            store.clone(),
            &ctx,
            cascade_types::ScopeId(0),
            cascade_types::Epoch::FIRST,
        )
        .await
        .expect("writer opens");
        (store, std::sync::Arc::new(writer))
    })
}

#[test]
fn live_recycler_runs_but_never_touches_the_log() {
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
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");

    let recycle = rt.recycle_stats().expect("a serving WAL boot recycles");
    assert!(
        rt.checkpoint_stats().is_some(),
        "the checkpoint fanout rides along on the same tick loop"
    );
    assert_eq!(rt.truncatable_lsn(), Some(Lsn(0)));

    // Make the log real: rows through the wire protocol, so the file the
    // recycler must protect has content recovery depends on.
    let url = format!("postgres://alice:pw@127.0.0.1:{port}/dev");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("client connects");
    const ROWS: u32 = 50;
    for n in 0..ROWS {
        client
            .simple_query(&format!("INSERT INTO debug_table VALUES ({n}, 'recycle-{n}')"))
            .expect("insert");
    }

    let dev = cascade_runtime::tenant::dev_context().db;
    let scope = cascade_types::ScopeId(u64::from(dev.0));
    let store_log = |dir: &std::path::Path| -> Vec<u8> {
        use std::sync::Arc;
        glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("recycle-read")
            .make()
            .expect("executor")
            .run(async {
                let device = Arc::new(GlommioBlockDevice::new(dir.join("store")));
                let store = LogStore::open(device).await.expect("store opens");
                let span = cascade_telemetry::Span::noop();
                let ctx = cascade_storage_api::OpCtx::new(
                    cascade_storage_api::TenantContext::for_database(dev),
                    &span,
                );
                use cascade_storage_api::LogSource as _;
                let durable =
                    cascade_storage_api::LogSink::durable_lsn(&*store, scope).expect("durable");
                let mut out = Vec::new();
                let mut pos = Lsn(0);
                while pos < durable {
                    let chunk = store.read_log(&ctx, scope, pos, 1 << 20).await.expect("chunk");
                    if chunk.bytes.is_empty() {
                        break;
                    }
                    out.extend_from_slice(&chunk.bytes);
                    pos = chunk.end;
                }
                out
            })
    };
    let before = store_log(&dir);
    assert!(!before.is_empty(), "the log has content to protect");

    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        recycle.signals() >= 4,
        "~20 ticks nominal at 50ms; only {} recycle signals sent",
        recycle.signals()
    );
    assert_eq!(recycle.refused(), 0, "an idle control channel never refuses");
    assert!(
        recycle.passes() >= 4,
        "the home reactor must run what it is signaled: {}",
        recycle.passes()
    );
    assert_eq!(
        recycle.reclaimed_bytes(),
        0,
        "frontier 0: nothing is reclaimable — ever"
    );

    let after = store_log(&dir);
    assert_eq!(
        before, after,
        "needed-segment protection: a second of recycles left the log byte-identical"
    );
    assert_eq!(
        rt.truncatable_lsn(),
        Some(Lsn(0)),
        "the recycler is a frontier consumer, never an advancer"
    );
    // And the protected log still replays everything committed above
    // (M6: through the log store's LogSource walk).
    let plan = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("recycle-replay")
        .make()
        .expect("executor")
        .run(async {
            let device = Arc::new(GlommioBlockDevice::new(dir.join("store")));
            let store = LogStore::open(device).await.expect("store opens");
            let span = cascade_telemetry::Span::noop();
            let _ctx = cascade_storage_api::OpCtx::new(
                cascade_storage_api::TenantContext::for_database(dev),
                &span,
            );
            
            let durable = cascade_storage_api::LogSink::durable_lsn(&*store, scope).expect("durable");
            cascade_runtime::recovery::prepare_from_source(&*store, scope, durable).await
        })
        .expect("wal walk");
    assert_eq!(
        plan.seed_rows.len(),
        ROWS as usize,
        "every insert survived a second of recycling"
    );
}

/// The engine-level concurrent-checkpoint protection: with the frontier
/// at l3 (real flushed pages, real store) and a page stamped l4 dirty but
/// undurable (a checkpoint in flight), the recycle boundary stays at or
/// below l3 — the in-flight checkpoint's bytes are never inside a
/// reclaimable window — recycles during the parked pass reclaim nothing,
/// and only the CHECKPOINTER's eventual advance moves the frontier.
#[test]
fn engine_recycle_protects_a_concurrent_checkpoint() {
    let dir = temp_dir("engine");
    let cleanup_dir = dir.clone();
    let db = DatabaseId(7);
    let (_store, writer) = sink_writer();
    let recycle_passes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reclaimed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let site = RecycleSite::new(Arc::clone(&recycle_passes), Arc::clone(&reclaimed));

    let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("recycle-engine-test")
        .make()
        .expect("executor builds");
    ex.run(async move {
        let manager = StorageManager::mount(StorageConfig {
            data_dir: dir.join("pool"),
            wal_dir: dir.join("wal"),
            io_uring_entries: 64,
        })
        .expect("store mounts");
        let store = manager.local_worker(0);
        // The pool is the §8.1 shared case (Arc — T6.2): it holds no
        // store, every call passes this reactor's own. Rc stays for the
        // parked-pass task's trunc (spawn_local is 'static; Rc never
        // crosses a thread).
        let bpm = Bpm::arced(
            BpmConfig {
                max_frames: 8,
                cores_on_numa: 1,
                numa: None,
            },
            PerDbAccounting::default(),
        );
        let trunc = std::rc::Rc::new(TruncatableLsn::new());
        bpm.set_gate(wal_gate(&writer));

        // Records l1..l3, durability published through l3.
        let mut lsns = Vec::new();
        for xid in 1..=3u64 {
            let r = writer.append_wal(&WalRecord::BeginTxn { xid });
            let l = r.end();
            let _ = r.fill();
            lsns.push(l);
        }
        let (l1, l2, l3) = (lsns[0], lsns[1], lsns[2]);
        writer.flush_wal_sink(l3).await.expect("flush to l3");

        // Checkpoint passes (the real T4.2 path) walk the frontier to l3:
        // pass 1 flushes all three (the floor is the min stamp, l1); the
        // re-stamped pass 2 lifts it to l3.
        for (page_no, lsn) in [(1u32, l1), (2, l2), (3, l3)] {
            let mut g = bpm
                .page_exclusive(&store, page(db, page_no))
                .await
                .expect("page loads");
            g.write_logged(lsn, |buf| buf[0] = 0xAA);
        }
        let report = checkpointer::flush_pass(&bpm, &store, db, &trunc)
            .await
            .expect("pass 1");
        assert_eq!((report.flushed, report.refused), (3, 0));
        assert_eq!(trunc.get(), l1);
        let mut pinned = bpm
            .page_exclusive(&store, page(db, 3))
            .await
            .expect("p3 pins");
        pinned.write_logged(l3, |buf| buf[0] = 0xAA);
        checkpointer::flush_pass(&bpm, &store, db, &trunc)
            .await
            .expect("pass 2");
        assert_eq!(trunc.get(), l3, "the checkpointer owns the frontier");
        drop(pinned);

        // The concurrent checkpoint: p4 stamped l4, dirty, undurable.
        let r4 = writer.append_wal(&WalRecord::BeginTxn { xid: 4 });
        let l4 = r4.end();
        let _ = r4.fill();
        {
            let mut g = bpm
                .page_exclusive(&store, page(db, 4))
                .await
                .expect("p4 loads");
            g.write_logged(l4, |buf| buf[0] = 0xBB);
        }
        assert!(writer.durable() < l4, "a checkpoint is genuinely in flight");

        // The recycle at the frontier snapshot: at or below l3, never
        // covering l4. Byte-scale frontiers against the production
        // 16 MB window reclaim exactly zero — the live truth, and the
        // reason the log survives until the segmented WAL lands.
        let decision = site.run_recycle(db, trunc.get());
        assert!(
            decision.reclaimable_bytes <= l3.0,
            "recycle boundary {:?} above frontier l3",
            decision
        );
        assert!(
            l4.0 >= decision.protected_at.0,
            "the in-flight checkpoint's stamp sits inside the protected region"
        );
        assert_eq!(decision.reclaimable_bytes, 0, "byte-scale frontier, 16 MB window");
        assert_eq!(
            trunc.get(),
            l3,
            "a recycle must never advance the frontier"
        );

        // The parked pass (T3.2 gate through the checkpointer's path),
        // with recycles running underneath it the whole time — issued
        // from the main task while the pass is parked on p4's undurable
        // stamp (inside the pass task they would run after it completed,
        // i.e. after the frontier already moved).
        let done = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = std::rc::Rc::clone(&done);
        let passer = glommio::spawn_local({
            let bpm = bpm.clone();
            let trunc = std::rc::Rc::clone(&trunc);
            let passer_store = manager.local_worker(0);
            async move {
                checkpointer::flush_pass(&bpm, &passer_store, db, &trunc)
                    .await
                    .expect("pass 3");
                flag.set(true);
            }
        });
        glommio::timer::sleep(Duration::from_millis(5)).await;
        assert!(!done.get(), "the undurable stamp parks the pass");
        // Recycles DURING the parked pass: snapshots of a frontier that
        // has not moved — same answer, still no reclaim, still no advance.
        for _ in 0..3 {
            let d = site.run_recycle(db, trunc.get());
            assert_eq!(d.reclaimable_bytes, 0);
            assert_eq!(trunc.get(), l3);
        }
        writer.flush_wal_sink(l4).await.expect("flush to l4");
        passer.await;
        assert!(done.get());
        assert_eq!(
            trunc.get(),
            l4,
            "only the checkpointer's advance moved the frontier"
        );
        assert_eq!(
            recycle_passes.load(Ordering::Relaxed),
            4,
            "every recycle ran on the site"
        );
        assert_eq!(
            reclaimed.load(Ordering::Relaxed),
            0,
            "no byte was ever reclaimable in this engine state"
        );
    });
    let _ = std::fs::remove_dir_all(cleanup_dir);
}

/// No listener (nothing orchestrates, D9) or no WAL (nothing to recycle)
/// means no recycler stats — absence, not zeros.
#[test]
fn boots_without_serving_or_wal_expose_no_recycle_stats() {
    let _serial = serial();
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);
    let topo = SyntheticTopology::new(allowed.iter().copied());

    // Compute-only boot, WITH a WAL: the frontier exists (T4.3's recycler
    // reads it), but nothing orchestrates.
    let dir = temp_dir("listenerless");
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
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot(&cfg, &topo).expect("compute boot");
    assert!(rt.recycle_stats().is_none());
    assert_eq!(rt.truncatable_lsn(), Some(Lsn(0)));
}
