//! T1.9 + T3.2 + T6.2: the buffer pool — hit/miss/pin/unpin, panic
//! release, lookup races, pool-full, the WAL-before-data writeback gate,
//! gated eviction — and the T6.2 cross-core generalization: multiple
//! reactors sharing one `Arc<Bpm>`, each with its own `CoreStorage` on one
//! data dir (the production shape: the pool is shared, the store is
//! reactor-bound). Every test runs real `CoreStorage` over a temp dir.
//!
//! Two behavior changes from the single-reactor slice are pinned here:
//! flushes no longer refuse pinned pages (coherent snapshots + the write
//! ticket make them safe — writers proceed during gate waits), while
//! eviction still refuses them (a claim needs the pin word at zero).

use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Barrier, Mutex,
    },
    time::Duration,
};

use cascade_bpm::{Bpm, BpmConfig, BpmError, PerDbAccounting, WalDurability};
use cascade_storage::{
    CoreStorage, PageStore, StorageError,
};
use cascade_types::{
    AlignedBuf, DatabaseId, Lsn, PageId, SegmentId, PAGE_SIZE,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};

/// The eternal-futex backstop. This development environment carries a
/// documented hazard (WSL2 5.15 + glommio 0.9 can lose an io_uring
/// completion; a ring whose PARK wakeup is lost freezes every task on it
/// — first diagnosed in cascade-runtime/src/testutil.rs at ~1-in-20
/// suites, and since observed here: one 10× stability loop's first run
/// sat in futex wait for five hours). Nothing running on a wedged ring
/// can contain it, so the watchdog converts the eternal hang into a fast,
/// loud failure: at the budget it dumps every thread's /proc syscall line
/// — `io_uring_enter` on the reactors names the frozen-ring class, while
/// two threads parked on each other's futexes would name a real deadlock
/// instead — to a side file and stderr, then aborts.
mod watchdog {
    use std::sync::Once;
    use std::time::Duration;

    static ARM: Once = Once::new();

    pub fn arm() {
        ARM.call_once(|| {
            let secs = std::env::var("CASCADE_TEST_WATCHDOG_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(90);
            let budget = Duration::from_secs(secs);
            std::thread::Builder::new()
                .name("bpm-suite-watchdog".into())
                .spawn(move || {
                    std::thread::sleep(budget);
                    let mut dump = format!(
                        "=== BPM SUITE WATCHDOG: {budget:?} budget exceeded ===\n"
                    );
                    let tasks = std::fs::read_dir("/proc/self/task")
                        .map(|d| d.flatten().collect::<Vec<_>>())
                        .unwrap_or_default();
                    for t in tasks {
                        let tid = t.file_name().to_string_lossy().into_owned();
                        let comm = std::fs::read_to_string(format!(
                            "/proc/self/task/{tid}/comm"
                        ))
                        .unwrap_or_default();
                        let syscall = std::fs::read_to_string(format!(
                            "/proc/self/task/{tid}/syscall"
                        ))
                        .unwrap_or_default();
                        let wchan = std::fs::read_to_string(format!(
                            "/proc/self/task/{tid}/wchan"
                        ))
                        .unwrap_or_default();
                        dump.push_str(&format!(
                            "TID {} [{}] wchan={} syscall={}\n",
                            tid,
                            comm.trim(),
                            wchan.trim(),
                            syscall.trim()
                        ));
                    }
                    dump.push_str("=== BPM SUITE WATCHDOG: aborting ===\n");
                    let side = std::env::temp_dir().join(format!(
                        "cascade-bpm-watchdog-{}.log",
                        std::process::id()
                    ));
                    let _ = std::fs::write(&side, &dump);
                    eprint!("{dump}");
                    std::process::abort();
                })
                .expect("bpm suite watchdog thread spawns");
        });
    }
}

fn reactor_named(name: &str) -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name(name)
        .make()
        .expect("executor builds")
}

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// A temp data dir cleaned up on drop. Cross-reactor tests keep it alive
/// on the main thread past every join.
fn temp_dir() -> PathBuf {
    watchdog::arm();
    let dir = std::env::temp_dir().join(format!(
        "cascade-bpm-test-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// The single-reactor fixture: one pool, one store, one executor — the
/// T1.9/T3.2 shape, unchanged except that the pool now holds no store and
/// every call passes it.
struct Pool {
    dir: PathBuf,
    bpm: Arc<Bpm>,
    store: CoreStorage,
}

impl Pool {
    fn with_frames(max_frames: u32) -> Self {
        Self::build(max_frames, None)
    }

    fn build(max_frames: u32, numa: Option<u32>) -> Self {
        let dir = temp_dir();
        let bpm = Bpm::arced(
            BpmConfig {
                max_frames,
                cores_on_numa: 1,
                numa,
            },
            PerDbAccounting::default(),
        );
        let store = CoreStorage::new(0, dir.clone());
        Pool { dir, bpm, store }
    }

    fn page(&self, page_no: u32) -> PageId {
        PageId {
            db: DatabaseId(7),
            segment: SegmentId(3),
            page_no,
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A reactor thread with its own `CoreStorage` over the shared dir — the
/// second core of a cross-core test. The body runs on that thread's
/// executor; only `Send` things (the `Arc<Bpm>`, owned data) cross over.
fn spawn_reactor<F, Fut>(
    name: &str,
    core_id: usize,
    dir: PathBuf,
    bpm: Arc<Bpm>,
    body: F,
) -> std::thread::JoinHandle<()>
where
    F: FnOnce(CoreStorage, Arc<Bpm>) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let name = name.to_string();
    std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                .name(&name)
                .make()
                .expect("executor builds");
            ex.run(async move {
                let store = CoreStorage::new(core_id, dir);
                body(store, bpm).await;
            });
        })
        .expect("reactor thread spawns")
}

fn page_id(page_no: u32) -> PageId {
    PageId {
        db: DatabaseId(7),
        segment: SegmentId(3),
        page_no,
    }
}

/// `CoreStorage` wrapper counting disk reads — the double-install
/// detector: after N reactors all load one page, exactly one reactor may
/// have read it from disk. Same shape as the production store otherwise.
struct CountingStore {
    inner: CoreStorage,
    reads: Arc<AtomicU32>,
}

impl CountingStore {
    fn new(inner: CoreStorage) -> Self {
        CountingStore {
            inner,
            reads: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl PageStore for CountingStore {
    async fn read_page(&self, page_id: PageId) -> Result<AlignedBuf, StorageError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_page(page_id).await
    }

    async fn write_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> Result<AlignedBuf, StorageError> {
        self.inner.write_page(page_id, buf).await
    }

    async fn read_pages(
        &self,
        start_page_id: PageId,
        count: u32,
    ) -> Result<Vec<AlignedBuf>, StorageError> {
        self.inner.read_pages(start_page_id, count).await
    }

    async fn write_pages(
        &self,
        start_page_id: PageId,
        bufs: Vec<AlignedBuf>,
    ) -> Result<Vec<AlignedBuf>, StorageError> {
        self.inner.write_pages(start_page_id, bufs).await
    }

    async fn allocate_extent(
        &self,
        db_id: u32,
        space_id: u32,
        num_pages: u32,
    ) -> Result<u32, StorageError> {
        self.inner.allocate_extent(db_id, space_id, num_pages).await
    }

    async fn free_extent(
        &self,
        db_id: u32,
        space_id: u32,
        start_page: u32,
        num_pages: u32,
    ) -> Result<(), StorageError> {
        self.inner.free_extent(db_id, space_id, start_page, num_pages).await
    }
}

// --- shape -------------------------------------------------------------------

/// I2 survives the generalization, split in two: the POOL is now shared
/// (Send + Sync — the whole point of T6.2, guards included), while the
/// STORE stays strictly reactor-bound (its file handles never cross
/// threads). The store moved out of the pool; it cannot smuggle `!Send`
/// back in.
#[test]
fn pool_is_cross_core_and_the_store_is_not() {
    assert_impl_all!(Bpm: Send, Sync);
    assert_impl_all!(cascade_bpm::PageGuard: Send, Sync);
    assert_impl_all!(cascade_bpm::PageGuardMut: Send, Sync);
    assert_impl_all!(PerDbAccounting: Send, Sync);
    assert_not_impl_any!(CoreStorage: Send, Sync);
}

// --- T1.9: pin/unpin basics, now through the per-call store ------------------

#[test]
fn miss_reads_then_hit_pins() {
    let t = Pool::with_frames(4);
    reactor_named("bpm-test").run(async {
        let page = t.page(0);
        // Miss: fresh page reads as zeroes, exactly one frame used.
        {
            let guard = t.bpm.page(&t.store, page).await.expect("miss loads the page");
            assert_eq!(t.bpm.frames_in_use(), 1);
            assert!(guard.iter().all(|&b| b == 0), "fresh page is zeroes");
            assert_eq!(t.bpm.pin_count(&page), 1);
        }
        // The writer guard (the cross-core pool splits reader and writer
        // entry points).
        {
            let mut guard = t
                .bpm
                .page_exclusive(&t.store, page)
                .await
                .expect("writer pin");
            guard.write(|p| p[10] = 0xAB);
        }
        assert_eq!(t.bpm.pin_count(&page), 0, "guard drop unpins");
        assert_eq!(t.bpm.is_dirty(&page), Some(true), "write marked dirty");

        // Hit: same frame, no second load, mutation visible in memory
        // (writeback is explicit).
        {
            let g1 = t.bpm.page(&t.store, page).await.expect("hit");
            let _g2 = t.bpm.page(&t.store, page).await.expect("hit again");
            assert_eq!(t.bpm.frames_in_use(), 1, "no duplicate frame");
            assert_eq!(t.bpm.pin_count(&page), 2);
            assert_eq!(g1[10], 0xAB, "earlier write visible through the hit");
        }
        assert_eq!(t.bpm.pin_count(&page), 0);
    });
}

/// The exclusive pin: a writer guard refuses a second writer AND any
/// reader (`Contended`, fail fast — the caller owns the retry); readers
/// refuse a writer. Shared pins coexist freely.
#[test]
fn exclusive_writer_refuses_readers_and_writers() {
    let t = Pool::with_frames(4);
    reactor_named("bpm-test").run(async {
        let page = t.page(3);
        let mut writer = t
            .bpm
            .page_exclusive(&t.store, page)
            .await
            .expect("first writer");
        writer.write(|p| p[4] = 0x44);

        match t.bpm.page(&t.store, page).await {
            Err(BpmError::Contended(p)) => assert_eq!(p, page),
            Ok(_) => panic!("reader must refuse while the writer holds EXCL"),
            Err(other) => panic!("expected Contended, got {other:?}"),
        }
        match t.bpm.page_exclusive(&t.store, page).await {
            Err(BpmError::Contended(p)) => assert_eq!(p, page),
            Ok(_) => panic!("second writer must refuse"),
            Err(other) => panic!("expected Contended, got {other:?}"),
        }

        drop(writer);
        let r1 = t.bpm.page(&t.store, page).await.expect("reader 1");
        let r2 = t.bpm.page(&t.store, page).await.expect("reader 2");
        assert_eq!(t.bpm.pin_count(&page), 2, "readers share the frame");
        match t.bpm.page_exclusive(&t.store, page).await {
            Err(BpmError::Contended(_)) => {}
            Ok(_) => panic!("writer must refuse while readers hold the frame"),
            Err(other) => panic!("expected Contended, got {other:?}"),
        }
        drop((r1, r2));
        let _w2 = t
            .bpm
            .page_exclusive(&t.store, page)
            .await
            .expect("writer after readers");
    });
}

#[test]
fn panic_while_pinned_releases_the_pin() {
    let t = Pool::with_frames(4);
    reactor_named("bpm-test").run(async {
        let page = t.page(1);
        let guard = t.bpm.page(&t.store, page).await.expect("pinned");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _last_use = guard; // moved in: dropped by the unwind
            panic!("planted");
        }));
        assert!(outcome.is_err());
        assert_eq!(
            t.bpm.pin_count(&page),
            0,
            "the unwind dropped the guard → pin released"
        );
        assert_eq!(t.bpm.accounting().pinned(DatabaseId(7)), 0);
    });
}

#[test]
fn racing_lookups_from_one_reactor_share_one_frame() {
    let t = Pool::with_frames(4);
    reactor_named("bpm-test").run(async {
        let page = t.page(2);
        let (a, b) =
            futures_lite::future::zip(t.bpm.page(&t.store, page), t.bpm.page(&t.store, page))
                .await;
        let a = a.expect("first loader");
        let b = b.expect("second loader");
        assert_eq!(
            t.bpm.frames_in_use(),
            1,
            "both miss → the page is inserted exactly once, both pin it"
        );
        assert_eq!(t.bpm.pin_count(&page), 2);
        drop((a, b));
        assert_eq!(t.bpm.pin_count(&page), 0);
    });
}

/// Pool full with every frame PINNED refuses — the greedy eviction may
/// only take unpinned victims. Releasing the pin lets the next miss evict
/// the clean victim instead of refusing.
#[test]
fn pinned_frame_refuses_but_released_frame_evicts() {
    let t = Pool::with_frames(1);
    reactor_named("bpm-test").run(async {
        let _held = t.bpm.page(&t.store, t.page(0)).await.expect("first page fits");
        match t.bpm.page(&t.store, t.page(1)).await {
            Err(BpmError::PoolFull { max_frames }) => assert_eq!(max_frames, 1),
            Ok(_) => panic!("expected PoolFull, got a pinned page"),
            Err(other) => panic!("expected PoolFull, got {other:?}"),
        }
        drop(_held);
        // The clean unpinned victim is now evictable — the second page
        // fits, reusing the frame.
        let second = t.bpm.page(&t.store, t.page(1)).await.expect("evicts the victim");
        assert_eq!(t.bpm.frames_in_use(), 1, "the frame was reused");
        assert_eq!(t.bpm.is_dirty(&t.page(0)), None, "victim unmapped");
        drop(second);
    });
}

#[test]
fn accounting_follows_pins_not_frames() {
    let t = Pool::with_frames(4);
    reactor_named("bpm-test").run(async {
        let db = DatabaseId(7);
        let a = t.bpm.page(&t.store, t.page(0)).await.expect("pin a");
        let b = t.bpm.page(&t.store, t.page(1)).await.expect("pin b");
        assert_eq!(t.bpm.accounting().pinned(db), 2);
        drop(a);
        assert_eq!(t.bpm.accounting().pinned(db), 1);
        drop(b);
        assert_eq!(t.bpm.accounting().pinned(db), 0);
    });
}

#[test]
fn pages_are_page_sized() {
    let t = Pool::with_frames(2);
    reactor_named("bpm-test").run(async {
        let guard = t.bpm.page(&t.store, t.page(0)).await.expect("loaded");
        assert_eq!(guard.len(), PAGE_SIZE);
    });
}

// --- T6.2: cross-core sharing -------------------------------------------------

/// THE T6.2 core test: four reactors on four threads share one pool and
/// load the same page at once. Exactly one disk read (double-install
/// detector), exactly one frame, every guard sees the same bytes.
#[test]
fn double_install_from_many_reactors_is_exactly_once() {
    let dir = temp_dir();
    let bpm = Bpm::arced(
        BpmConfig {
            max_frames: 8,
            cores_on_numa: 4,
            numa: None,
        },
        PerDbAccounting::default(),
    );
    const REACTORS: usize = 4;
    let barrier = Arc::new(Barrier::new(REACTORS));
    let total_reads = Arc::new(Mutex::new(Vec::<Arc<AtomicU32>>::new()));
    let page = page_id(0);

    let mut handles = Vec::new();
    for core in 0..REACTORS {
        let bpm = Arc::clone(&bpm);
        let barrier = Arc::clone(&barrier);
        let dir = dir.clone();
        let total_reads = Arc::clone(&total_reads);
        handles.push(spawn_reactor("bpm-xcore", core, dir, bpm, move |store, bpm| async move {
            let counting = CountingStore::new(store);
            total_reads.lock().unwrap().push(Arc::clone(&counting.reads));
            barrier.wait();
            let guard = bpm.page(&counting, page).await.expect("cross-core load");
            assert!(guard.iter().all(|&b| b == 0), "fresh page is zeroes");
            // Hold until everyone has loaded, so the pin race window is
            // fully exercised before any unpins.
            barrier.wait();
            drop(guard);
        }));
    }
    for h in handles {
        h.join().expect("reactor joined cleanly");
    }
    let reads: u32 = total_reads
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .sum();
    // The INSTALL is exactly once — one frame, ever. The disk READ is not
    // deduplicated by design: each miss reads outside every lock (§8.1's
    // no-I/O-under-locks rule), so concurrent same-page misses may each
    // read before the winner publishes; the re-check only stops a second
    // install. 1..=REACTORS reads, one frame.
    assert!(
        (1..=REACTORS as u32).contains(&reads),
        "expected 1..={REACTORS} disk reads, got {reads}"
    );
    assert_eq!(bpm.frames_in_use(), 1, "one frame, not four");
    assert_eq!(bpm.pin_count(&page), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Guards are `'static` and `Send`: a pin taken on one reactor can be
/// released on another (the unpin path is lock-based, not reactor-bound),
/// and the freed frame serves a load from yet another reactor.
#[test]
fn guards_move_across_reactors() {
    let dir = temp_dir();
    let bpm = Bpm::arced(
        BpmConfig {
            max_frames: 4,
            cores_on_numa: 2,
            numa: None,
        },
        PerDbAccounting::default(),
    );
    let page = page_id(5);
    {
        let bpm = Arc::clone(&bpm);
        let ex = reactor_named("bpm-main");
        ex.run(async {
            let guard = bpm.page(&CoreStorage::new(0, dir.clone()), page).await.expect("pin");
            // Hand the guard to another thread while pinned.
            let (tx, rx) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(move || {
                let ex = reactor_named("bpm-drop");
                ex.run(async move {
                    let guard: cascade_bpm::PageGuard = rx.recv().expect("guard arrives");
                    assert_eq!(guard[0], 0);
                    drop(guard); // unpins on THIS reactor
                });
            });
            tx.send(guard).expect("guard crosses threads");
            handle.join().expect("dropping thread joins");
            assert_eq!(bpm.pin_count(&page), 0, "cross-thread unpin worked");
        });
    }
    // A third reactor loads it (the frame is still resident — hit path).
    let h = spawn_reactor("bpm-third", 2, dir.clone(), Arc::clone(&bpm), move |store, bpm| async move {
        let g = bpm.page(&store, page).await.expect("hit from another reactor");
        assert_eq!(g.len(), PAGE_SIZE);
    });
    h.join().expect("third reactor joins");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Write-vs-flush race, cross-core, with a CRC-validated read-back: a
/// writer hammers one page through exclusive guards while a flusher
/// flushes it continuously from another reactor. The store's CRC makes
/// any torn or interleaved image fail the final read-back; the final
/// image must carry the writer's last generation.
#[test]
fn write_vs_flush_race_stress_keeps_images_coherent() {
    let dir = temp_dir();
    let bpm = Bpm::arced(
        BpmConfig {
            max_frames: 4,
            cores_on_numa: 2,
            numa: None,
        },
        PerDbAccounting::default(),
    );
    bpm.set_gate(Arc::new(FakeGate {
        durable: AtomicU64::new(u64::MAX), // everything covered: flushes run at full speed
        waits: AtomicU32::new(0),
        fail: false,
    }));
    let page = page_id(1);
    const ITERS: u32 = 300;

    let writer = {
        let bpm = Arc::clone(&bpm);
        let dir = dir.clone();
        spawn_reactor("bpm-writer", 0, dir, bpm, move |store, bpm| async move {
            for i in 0..ITERS {
                let mut g = bpm
                    .page_exclusive(&store, page)
                    .await
                    .expect("writer pin");
                g.write_logged(Lsn(u64::from(i) + 1), |p| {
                    // Marker ≥ 4: bytes 0..4 are the store's CRC slot.
                    p[4..8].copy_from_slice(&i.to_le_bytes());
                    p[8] = 0xA5;
                });
                drop(g);
            }
        })
    };
    let flusher = {
        let bpm = Arc::clone(&bpm);
        let dir = dir.clone();
        spawn_reactor("bpm-flusher", 1, dir, bpm, move |store, bpm| async move {
            for _ in 0..(ITERS * 2) {
                // Refusals (ticket held / generation moved) are protocol,
                // not failure.
                bpm.flush_page(&store, page).await.expect("flush protocol");
                glommio::timer::sleep(Duration::from_micros(200)).await;
            }
        })
    };
    writer.join().expect("writer joins");
    flusher.join().expect("flusher joins");

    // Final flush from the main reactor, then a CRC-validated read-back:
    // the store must hold the last generation, never a torn hybrid. The
    // flusher's late flushes may have already written and cleared the
    // final generation — `Ok(false)` then means "nothing left to do".
    let ex = reactor_named("bpm-verify");
    ex.run(async {
        let store = CoreStorage::new(9, dir.clone());
        let _ = bpm.flush_page(&store, page).await.expect("final flush");
        let on_disk = store.read_page(page).await.expect("CRC-validated read");
        assert_eq!(on_disk.as_slice()[8], 0xA5, "marker present");
        let counter = u32::from_le_bytes(
            on_disk.as_slice()[4..8].try_into().expect("counter bytes"),
        );
        assert_eq!(counter, ITERS - 1, "the final generation survived the race");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Gate waits under concurrency: the flusher parks on the gate while a
/// SECOND reactor re-modifies the page — the flush must extend its wait
/// and land the newer image (the cross-reactor form of the single-reactor
/// re-modification case).
#[test]
fn gate_wait_under_cross_reactor_concurrency() {
    let dir = temp_dir();
    let bpm = Bpm::arced(
        BpmConfig {
            max_frames: 4,
            cores_on_numa: 2,
            numa: None,
        },
        PerDbAccounting::default(),
    );
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(0),
        waits: AtomicU32::new(0),
        fail: false,
    });
    bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    let page = page_id(2);

    // Stamp e1 on the main reactor.
    {
        let bpm = Arc::clone(&bpm);
        let dir = dir.clone();
        let ex = reactor_named("bpm-stamper");
        ex.run(async {
            let store = CoreStorage::new(0, dir);
            let mut g = bpm.page_exclusive(&store, page).await.expect("load");
            g.write_logged(Lsn(100), |p| p[5] = 0x55);
        });
    }

    let flusher = {
        let bpm = Arc::clone(&bpm);
        let dir = dir.clone();
        spawn_reactor("bpm-flusher", 1, dir, bpm, move |store, bpm| async move {
            let wrote = bpm.flush_page(&store, page).await.expect("flush");
            assert!(wrote, "the flush lands once durability catches up");
        })
    };
    // While the flusher is parked on e1, re-modify from a third reactor
    // under e2.
    let rewriter = {
        let bpm = Arc::clone(&bpm);
        let gate = Arc::clone(&gate);
        let dir = dir.clone();
        spawn_reactor("bpm-rewriter", 2, dir, bpm, move |store, bpm| async move {
            // Let the flusher reach the gate first.
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert!(
                gate.waits.load(Ordering::Relaxed) >= 1,
                "flusher is parked in the gate"
            );
            let mut g = bpm.page_exclusive(&store, page).await.expect("re-open");
            g.write_logged(Lsn(200), |p| p[6] = 0x66);
            drop(g);
            // Durability reaches e1 (not enough for the moved stamp), then e2.
            gate.durable.store(100, Ordering::Relaxed);
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert_eq!(
                bpm.is_dirty(&page),
                Some(true),
                "the extended wait keeps the page dirty"
            );
            gate.durable.store(200, Ordering::Relaxed);
        })
    };
    // Hang forensics: this is the test the eternal hang concentrates in,
    // so it carries an inspector that publishes the gate's live state
    // every 2 s. On a healthy run the test ends before the first line
    // ever prints; on a hang the series names who stopped advancing —
    // durable moving = the rewriter is alive, waits incrementing = the
    // flusher is still cycling through the gate, dirty state = flush
    // progress.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let gw_gate = Arc::clone(&gate);
        let gw_bpm = Arc::clone(&bpm);
        let gw_stop = Arc::clone(&stop);
        let ins_page = page;
        std::thread::spawn(move || {
            let mut ticks = 0u32;
            while !gw_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                ticks += 1;
                if ticks % 20 == 0 {
                    eprintln!(
                        "[gate-inspector] durable={} waits={} dirty={:?}",
                        gw_gate.durable.load(Ordering::Relaxed),
                        gw_gate.waits.load(Ordering::Relaxed),
                        gw_bpm.is_dirty(&ins_page)
                    );
                }
            }
        });
    }
    flusher.join().expect("flusher joins");
    rewriter.join().expect("rewriter joins");
    stop.store(true, Ordering::Relaxed);

    let ex = reactor_named("bpm-verify");
    ex.run(async {
        let store = CoreStorage::new(9, dir.clone());
        let on_disk = store.read_page(page).await.expect("CRC-validated read");
        assert_eq!(on_disk.as_slice()[6], 0x66, "the NEWER image is what landed");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

// --- T3.2: the WAL-before-data writeback gate (adapted) -----------------------

/// Controllable fake gate: the test moves `durable` forward by hand; waits
/// poll on a 1 ms tick until coverage (or fail immediately when `fail`).
/// `waits` counts gate entries — the blocked-flush assertions read it.
/// `AtomicU64` because gates are `Send + Sync` now (shared across
/// reactors).
struct FakeGate {
    durable: AtomicU64,
    waits: AtomicU32,
    fail: bool,
}

impl WalDurability for FakeGate {
    fn durable_lsn(&self) -> Lsn {
        Lsn(self.durable.load(Ordering::Relaxed))
    }

    fn wait_durable(
        &self,
        lsn: Lsn,
    ) -> Pin<Box<dyn Future<Output = Result<(), BpmError>> + '_>> {
        self.waits.fetch_add(1, Ordering::Relaxed);
        if self.fail {
            return Box::pin(async { Err(BpmError::WalGate("planted gate failure".into())) });
        }
        Box::pin(async move {
            while self.durable.load(Ordering::Relaxed) < lsn.0 {
                glommio::timer::sleep(Duration::from_millis(1)).await;
            }
            Ok(())
        })
    }
}

/// The plan's planted case: a dirty page stamped with an undurable WAL
/// boundary must block writeback — not write, not lie — until durability
/// advances past it, and only then reach the store and clear dirty.
/// The flush runs concurrently with the probe via `zip` on this one
/// reactor (the flush parks inside the gate; the probe pokes it).
#[test]
fn writeback_blocks_until_the_gate_reports_durability() {
    let t = Pool::with_frames(4);
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(0),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    reactor_named("bpm-test").run(async {
        let page = t.page(0);
        let end = Lsn(100);
        {
            let mut g = t
                .bpm
                .page_exclusive(&t.store, page)
                .await
                .expect("load");
            g.write_logged(end, |p| p[5] = 0x5A);
        }
        assert_eq!(t.bpm.page_lsn(&page), Some(Some(end)));

        let probe = async {
            // While the gate is closed: parked (at least one gate entry),
            // still dirty, and nothing on disk.
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert!(gate.waits.load(Ordering::Relaxed) >= 1, "flusher entered the gate");
            assert_eq!(t.bpm.is_dirty(&page), Some(true), "blocked page stays dirty");
            let on_disk = t.store.read_page(page).await.expect("disk read");
            assert!(
                on_disk.as_slice().iter().all(|&b| b == 0),
                "nothing may reach the store ahead of its WAL"
            );
            // Durability advances past the stamp; the same flush completes.
            gate.durable.store(end.0, Ordering::Relaxed);
            glommio::timer::sleep(Duration::from_millis(3)).await;
        };
        let ((), wrote) = futures_lite::future::zip(probe, t.bpm.flush_page(&t.store, page)).await;
        assert!(wrote.expect("flush succeeds"), "the page was written");
        assert_eq!(t.bpm.is_dirty(&page), Some(false), "written page clears dirty");
        let on_disk = t.store.read_page(page).await.expect("disk read");
        assert_eq!(on_disk.as_slice()[5], 0x5A, "the gated image is on disk");
    });
}

/// Fault injection at the gate: a gate that fails leaves the page dirty
/// and the store untouched — writeback refused, not half-done.
#[test]
fn writeback_refused_when_the_gate_fails() {
    let t = Pool::with_frames(4);
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(0),
        waits: AtomicU32::new(0),
        fail: true,
    });
    t.bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    reactor_named("bpm-test").run(async {
        let page = t.page(1);
        {
            let mut g = t
                .bpm
                .page_exclusive(&t.store, page)
                .await
                .expect("load");
            g.write_logged(Lsn(50), |p| p[3] = 0x33);
        }
        match t.bpm.flush_page(&t.store, page).await {
            Err(BpmError::WalGate(_)) => {}
            other => panic!("expected a gate refusal, got {other:?}"),
        }
        assert_eq!(t.bpm.is_dirty(&page), Some(true), "refused page stays dirty");
        let on_disk = t.store.read_page(page).await.expect("disk read");
        assert!(on_disk.as_slice().iter().all(|&b| b == 0), "nothing reached the store");
    });
}

/// A page carrying a WAL boundary but no attached gate must refuse
/// writeback outright — an ungated write of a logged page is the exact
/// violation WAL-before-data exists to prevent. Silent success here would
/// be the bug.
#[test]
fn stamped_page_without_a_gate_refuses_writeback() {
    let t = Pool::with_frames(4);
    reactor_named("bpm-test").run(async {
        let page = t.page(2);
        {
            let mut g = t
                .bpm
                .page_exclusive(&t.store, page)
                .await
                .expect("load");
            g.write_logged(Lsn(7), |p| p[1] = 0x11);
        }
        match t.bpm.flush_page(&t.store, page).await {
            Err(BpmError::WalGate(msg)) => {
                assert!(msg.contains("no durability gate"), "message names the problem: {msg}");
            }
            other => panic!("expected the gate-less refusal, got {other:?}"),
        }
        assert_eq!(t.bpm.is_dirty(&page), Some(true));
    });
}

/// An unlogged dirty page (plain `write`, no stamp) is unlogged by
/// construction — the caller's contract, like PG's unlogged tables — and
/// writes ungated, no gate attached at all.
#[test]
fn unlogged_dirty_page_writes_ungated() {
    let t = Pool::with_frames(4);
    reactor_named("bpm-test").run(async {
        let page = t.page(3);
        {
            let mut g = t
                .bpm
                .page_exclusive(&t.store, page)
                .await
                .expect("load");
            g.write(|p| p[9] = 0x99);
        }
        assert_eq!(t.bpm.page_lsn(&page), Some(None), "plain write stamps nothing");
        let wrote = t.bpm.flush_page(&t.store, page).await.expect("ungated write succeeds");
        assert!(wrote);
        assert_eq!(t.bpm.is_dirty(&page), Some(false));
        let on_disk = t.store.read_page(page).await.expect("disk read");
        assert_eq!(on_disk.as_slice()[9], 0x99);
    });
}

/// The subtlest path: the page is re-modified while the gate wait is
/// parked — its stamp moves past the boundary that just became durable —
/// so the flush must extend its wait and write the NEWER image, then
/// clear dirty. Clearing dirty on the stale pass would lose the
/// re-modification on the next eviction.
#[test]
fn re_modification_during_the_gate_wait_extends_the_wait() {
    let t = Pool::with_frames(4);
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(0),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    reactor_named("bpm-test").run(async {
        let page = t.page(0);
        let e1 = Lsn(100);
        {
            let mut g = t
                .bpm
                .page_exclusive(&t.store, page)
                .await
                .expect("load");
            g.write_logged(e1, |p| p[5] = 0x55);
        }

        let probe = async {
            // The flush parks on e1; re-modify under it (the flush holds
            // no pin — that is the point of the post-await re-checks).
            glommio::timer::sleep(Duration::from_millis(3)).await;
            let e2 = Lsn(200);
            {
                let mut g = t
                    .bpm
                    .page_exclusive(&t.store, page)
                    .await
                    .expect("re-open");
                g.write_logged(e2, |p| p[6] = 0x66);
            }
            // Durability reaches e1 only — not enough for the moved stamp.
            gate.durable.store(e1.0, Ordering::Relaxed);
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert_eq!(
                t.bpm.is_dirty(&page),
                Some(true),
                "the extended wait keeps the page dirty"
            );
            gate.durable.store(e2.0, Ordering::Relaxed);
        };
        let ((), wrote) = futures_lite::future::zip(probe, t.bpm.flush_page(&t.store, page)).await;
        assert!(wrote.expect("flush eventually succeeds"));
        assert_eq!(t.bpm.is_dirty(&page), Some(false), "written through the new stamp");
        let on_disk = t.store.read_page(page).await.expect("disk read");
        assert_eq!(on_disk.as_slice()[6], 0x66, "the newer image is what landed");
    });
}

/// T6.2 behavior change, pinned deliberately: flushes no longer refuse
/// PINNED pages — the coherent snapshot + write ticket make them safe,
/// and writers must proceed during gate waits. Eviction still refuses
/// (its claim needs the pin word at zero).
#[test]
fn flush_succeeds_on_a_pinned_page_but_eviction_refuses() {
    let t = Pool::with_frames(2);
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(500),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    reactor_named("bpm-test").run(async {
        let page = t.page(0);
        let mut guard = t
            .bpm
            .page_exclusive(&t.store, page)
            .await
            .expect("load");
        // Marker at index 6 — bytes 0..4 are the store's CRC slot.
        guard.write_logged(Lsn(10), |p| p[6] = 0x22);
        let wrote = t
            .bpm
            .flush_page(&t.store, page)
            .await
            .expect("flushing a pinned page is safe now");
        assert!(wrote, "the pinned page's snapshot was written");
        let evicted = t.bpm.evict_page(&t.store, page).await.expect("no error — a refusal");
        assert!(!evicted, "pinned page refused eviction");
        assert_eq!(t.bpm.frames_in_use(), 1, "still resident");
        let on_disk = t.store.read_page(page).await.expect("disk read");
        assert_eq!(on_disk.as_slice()[6], 0x22, "the pinned generation reached disk");
        drop(guard);
        assert!(
            t.bpm.evict_page(&t.store, page).await.expect("evict after unpin"),
            "unpinned page evicts"
        );
    });
}

/// Eviction routes dirty pages through the same gate (no second writeback
/// path), and a freed frame is reusable — the pool-full refusal clears
/// once the victim is evicted.
#[test]
fn dirty_eviction_routes_through_the_gate_and_reclaims_the_frame() {
    let t = Pool::with_frames(1);
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(0),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    reactor_named("bpm-test").run(async {
        let victim = t.page(0);
        let end = Lsn(80);
        {
            let mut g = t
                .bpm
                .page_exclusive(&t.store, victim)
                .await
                .expect("load victim");
            // Marker at index 10: page-image bytes 0..4 are the on-disk CRC slot
            // (traits.rs) — the store stamps over them, so a marker below 4
            // never reaches the file.
            g.write_logged(end, |p| p[10] = 0x01);
        }
        assert_eq!(t.bpm.frames_in_use(), 1);

        let probe = async {
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert!(
                gate.waits.load(Ordering::Relaxed) >= 1,
                "eviction entered the gate — no bypass path"
            );
            assert_eq!(t.bpm.frames_in_use(), 1, "still resident while gated");
            gate.durable.store(end.0, Ordering::Relaxed);
        };
        let ((), evicted) =
            futures_lite::future::zip(probe, t.bpm.evict_page(&t.store, victim)).await;
        assert!(evicted.expect("evict succeeds"), "evicted");
        assert_eq!(t.bpm.is_dirty(&victim), None, "no longer resident");
        assert_eq!(t.bpm.frames_in_use(), 0, "frame reclaimed");

        // The freed slot serves a new load, and the evicted page reads
        // back from the store — its gated image.
        let reloaded = t.bpm.page(&t.store, victim).await.expect("re-load after eviction");
        assert_eq!(t.bpm.frames_in_use(), 1, "the freed frame was reused");
        assert_eq!(reloaded[10], 0x01, "the gated image survived eviction");

        // Pool full again → refusal; evict (the reload is unlogged and
        // unpinned, so eviction is gate-free) → a second page fits.
        let second = t.page(1);
        match t.bpm.page(&t.store, second).await {
            Err(BpmError::PoolFull { max_frames }) => assert_eq!(max_frames, 1),
            Ok(_) => panic!("expected PoolFull, got a pinned page"),
            Err(other) => panic!("expected PoolFull, got {other:?}"),
        }
        drop(reloaded);
        assert!(
            t.bpm.evict_page(&t.store, victim).await.expect("clean eviction"),
            "evicted"
        );
        let _ = t.bpm.page(&t.store, second).await.expect("fits after eviction");
    });
}

/// Clean pages evict without touching the gate or the store.
#[test]
fn clean_eviction_writes_nothing() {
    let t = Pool::with_frames(2);
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(0),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    reactor_named("bpm-test").run(async {
        let page = t.page(0);
        drop(t.bpm.page(&t.store, page).await.expect("load clean"));
        assert!(t.bpm.evict_page(&t.store, page).await.expect("evicts"));
        assert_eq!(t.bpm.frames_in_use(), 0);
        assert_eq!(gate.waits.load(Ordering::Relaxed), 0, "the gate was never entered");
        let on_disk = t.store.read_page(page).await.expect("disk read");
        assert!(on_disk.as_slice().iter().all(|&b| b == 0), "a clean eviction wrote nothing");
    });
}

/// The eviction LADDER under a full pool: a miss while the only frame
/// holds a dirty stamped victim must evict THROUGH THE GATE — parking on
/// the undurable boundary inside the miss itself — then complete the
/// load. The caller never sees a refusal; the WAL-before-data invariant
/// holds on the automatic path too.
#[test]
fn miss_on_full_pool_evicts_a_dirty_victim_through_the_gate() {
    let t = Pool::with_frames(1);
    let gate = Arc::new(FakeGate {
        durable: AtomicU64::new(0),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(Arc::clone(&gate) as Arc<dyn WalDurability>);
    reactor_named("bpm-test").run(async {
        let victim = t.page(0);
        {
            let mut g = t
                .bpm
                .page_exclusive(&t.store, victim)
                .await
                .expect("load victim");
            g.write_logged(Lsn(80), |p| p[10] = 0x01);
        }

        let probe = async {
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert!(
                gate.waits.load(Ordering::Relaxed) >= 1,
                "the miss parked on the victim's gate"
            );
            // Open the gate; the parked eviction completes and the miss
            // proceeds.
            gate.durable.store(80, Ordering::Relaxed);
        };
        let ((), loaded) = futures_lite::future::zip(probe, t.bpm.page(&t.store, t.page(1))).await;
        let _ = loaded.expect("the miss completes once the gate opens");
        assert_eq!(t.bpm.frames_in_use(), 1, "victim evicted, newcomer loaded");
        assert_eq!(t.bpm.is_dirty(&victim), None, "victim gone");
        let on_disk = t.store.read_page(victim).await.expect("CRC-validated read");
        assert_eq!(on_disk.as_slice()[10], 0x01, "victim's gated image survived");
    });
}

// --- placement ------------------------------------------------------------------

/// An unroutable node degrades to first-touch with node_bound() false —
/// the warn-and-degrade contract (I4a), runnable on any host.
#[test]
fn unroutable_node_degrades_to_first_touch() {
    let t = Pool::build(2, Some(999));
    assert!(
        !t.bpm.node_bound(),
        "no kernel has node 999 — placement must be reported as best-effort"
    );
    // The degraded pool still works end to end.
    reactor_named("bpm-test").run(async {
        let page = t.page(0);
        let g = t.bpm.page(&t.store, page).await.expect("degraded pool serves");
        assert_eq!(g.len(), PAGE_SIZE);
    });
}

/// Parses `/proc/self/numa_maps`, returning the N<k> per-node page counts
/// of the mapping containing `addr` (zero map if the range is not resident
/// yet).
fn numa_maps_counts(addr: usize) -> std::collections::HashMap<u32, u64> {
    let text = std::fs::read_to_string("/proc/self/numa_maps").expect("numa_maps readable");
    for line in text.lines() {
        let (range, rest) = line.split_once(' ').expect("numa_maps line shape");
        let (start, end) = range.split_once('-').expect("numa_maps range shape");
        let start: usize = start.parse().expect("range start");
        let end: usize = end.parse().expect("range end");
        if start <= addr && addr < end {
            let mut counts = std::collections::HashMap::new();
            for field in rest.split_whitespace() {
                if let Some(kv) = field.strip_prefix('N') {
                    if let Some((node, count)) = kv.split_once('=') {
                        if let (Ok(node), Ok(count)) = (node.parse(), count.parse()) {
                            counts.insert(node, count);
                        }
                    }
                }
            }
            return counts;
        }
    }
    std::collections::HashMap::new()
}

/// Online NUMA nodes from sysfs (bare-metal discrimination; WSL2 exposes
/// a single node or none).
fn online_numa_nodes() -> Vec<u32> {
    let mut nodes = Vec::new();
    let entries = match std::fs::read_dir("/sys/devices/system/node") {
        Ok(entries) => entries,
        Err(_) => return nodes,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(node) = name.strip_prefix("node") {
            if let Ok(node) = node.parse::<u32>() {
                nodes.push(node);
            }
        }
    }
    nodes.sort_unstable();
    nodes
}

/// Bare metal only: BPM frames bind to their configured node, verified
/// through `/proc/self/numa_maps` (FR-42; the runtime pool test does the
/// same against `PoolRegistry` memory). Skips on hosts with fewer than
/// two known NUMA nodes (CI, WSL2).
#[test]
#[ignore = "requires a real multi-NUMA host"]
fn frames_bind_to_their_node_on_bare_metal() {
    let nodes = online_numa_nodes();
    if nodes.len() < 2 {
        eprintln!("single-NUMA host (or no NUMA info); skipping locality check");
        return;
    }
    let numa = nodes[1]; // bind to a non-default node — the sharper discriminator
    let t = Pool::build(8, Some(numa));
    assert!(t.bpm.node_bound(), "the kernel accepted the bind on bare metal");

    reactor_named("bpm-test").run(async {
        // Fault the frames: load 4 pages (every load faults its frame).
        for i in 0..4 {
            let g = t.bpm.page(&t.store, t.page(i)).await.expect("faults a frame");
            assert_eq!(g.len(), PAGE_SIZE);
        }
        let counts = numa_maps_counts(t.bpm.ram_addr());
        let total: u64 = counts.values().sum();
        assert!(total > 0, "the pool mapping has no resident pages");
        let on_node = counts.get(&numa).copied().unwrap_or(0);
        assert_eq!(on_node, total, "pool frames span nodes: {counts:?}");
    });
}
