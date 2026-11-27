//! T1.9 + T6.2 + storage M8: the buffer pool. Hit/miss/pin/unpin, panic
//! release, lookup races, pool-full; the T6.2 cross-core generalization
//! (multiple reactors sharing one `Arc<Bpm>`); and the M8 cache rules:
//! misses read at the scope's durable LSN, pages are never written back,
//! and eviction drops only what the durable LSN covers (§12.5).
//!
//! The harness is the storage contract's own fakes: `MemLogStore` and
//! `MemPageStore` (the real contract surface: `LogSink`/`LogSource`/
//! `LogScopes` plus `PageSource`/`PageScopes`, conformance-covered in
//! cascade-storage-api) with the conformance suite's toy redo, and a
//! `MemDurableLsn` the tests advance by hand. No glommio. The pool has no
//! I/O of its own anymore (no write-back, no gate waits), so every future
//! resolves inline and plain `block_on` polls them, on the main thread or
//! on worker threads for the cross-core tests. (The old suite's watchdog
//! existed for glommio's lost-completion freezes; with no ring in this
//! suite, a hang here is an honest deadlock.)

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Barrier, Mutex,
};

use cascade_bpm::{Bpm, BpmConfig, BpmError, PerDbAccounting};
use cascade_storage_api::conformance::{toy_apply, toy_route};
use cascade_storage_api::fake::{test_ctx, MemDurableLsn, MemLogStore, MemPageStore};
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::page_source::PageSource;
use cascade_storage_api::scope_admin::{LogScopes, PageScopes};
use cascade_storage_api::types::AppendMeta;
use cascade_storage_api::{KeyRef, OpCtx, StoreError};
use cascade_types::{
    DatabaseId, Epoch, Hlc, Lsn, PageId, ScopeId, SegmentId, PAGE_SIZE,
};
use cascade_wal_format::{encode, encode_with_flags, WalRecord, FLAG_GROUP_END};
use futures_lite::future::block_on;
use static_assertions::assert_impl_all;

const SCOPE: ScopeId = ScopeId(7);
const DB: DatabaseId = DatabaseId(7);
const SEGMENT: SegmentId = SegmentId(3);
const KEY: KeyRef = KeyRef { key_id: 1 };

/// One node of the harness: a log store, a page store over it, the
/// durable-LSN view the tests advance by hand, and the pool under test.
struct Rig {
    log: Arc<MemLogStore>,
    pages: Arc<MemPageStore<MemLogStore>>,
    durable: Arc<MemDurableLsn>,
    bpm: Arc<Bpm>,
    /// The log's next append start — seeding is main-thread-only in these
    /// tests.
    next_lsn: Mutex<u64>,
}

impl Rig {
    fn with_frames(max_frames: u32) -> Arc<Rig> {
        Self::build(max_frames, None)
    }

    fn build(max_frames: u32, numa: Option<u32>) -> Arc<Rig> {
        let log = Arc::new(MemLogStore::new());
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 7);
        block_on(async {
            log.create_scope(&ctx, SCOPE, Epoch(1), KEY).await.expect("scope created");
        });
        let pages = Arc::new(MemPageStore::new(
            Arc::clone(&log),
            Arc::new(toy_route),
            Arc::new(toy_apply),
        ));
        block_on(async {
            pages.attach(&ctx, SCOPE, None, KEY).await.expect("page scope attached");
        });
        let durable = Arc::new(MemDurableLsn::new());
        let bpm = Bpm::arced(
            BpmConfig {
                max_frames,
                cores_on_numa: 1,
                numa,
            },
            PerDbAccounting::default(),
            Arc::clone(&durable) as Arc<dyn cascade_storage_api::DurableLsn>,
        );
        Arc::new(Rig {
            log,
            pages,
            durable,
            bpm,
            next_lsn: Mutex::new(0),
        })
    }

    /// Appends one mini-transaction whose HEAP_INSERT routes to `page_no`
    /// (the toy contract: the cell is `[page_no u32][content..]`) and
    /// makes it durable. Returns the INSERT record's END LSN — the page
    /// LSN the toy apply stamps (S17).
    fn seed(&self, page_no: u32, content: &[u8]) -> Lsn {
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 7);
        let mut cell = Vec::with_capacity(4 + content.len());
        cell.extend_from_slice(&page_no.to_le_bytes());
        cell.extend_from_slice(content);
        let start = *self.next_lsn.lock().unwrap();
        let ins = encode(
            &WalRecord::HeapInsert {
                xid: start,
                segment: SEGMENT,
                cell,
            },
            Lsn(start),
            1,
        );
        let com = encode_with_flags(
            &WalRecord::CommitTxn {
                xid: start,
                commit_ts: Hlc(start),
            },
            Lsn(start + ins.len() as u64),
            1,
            FLAG_GROUP_END,
        );
        let ins_len = ins.len();
        let mut bytes = ins;
        bytes.extend(com);
        block_on(async {
            self.log
                .append(
                    &ctx,
                    SCOPE,
                    Epoch(1),
                    Lsn(start),
                    &bytes,
                    AppendMeta {
                        max_commit_ts: Some(Hlc(start)),
                        writer_durable: Lsn(0),
                    },
                )
                .await
                .expect("seed appends");
            self.log
                .wait_durable(&ctx, SCOPE, Lsn(start + bytes.len() as u64))
                .await
                .expect("seed makes the group durable");
        });
        *self.next_lsn.lock().unwrap() = start + bytes.len() as u64;
        Lsn(start + ins_len as u64)
    }

    /// Raises the durable-LSN view — what the pool reads pages at and
    /// what its eviction rule defers to.
    fn durable_to(&self, lsn: Lsn) {
        self.durable.advance(SCOPE, lsn);
    }

    fn page(&self, page_no: u32) -> PageId {
        PageId {
            db: DB,
            segment: SEGMENT,
            page_no,
        }
    }
}

/// `MemPageStore` wrapper counting reads and recording every `at_lsn` it
/// was asked for — the miss-point and double-install detectors.
struct CountingSource {
    inner: Arc<MemPageStore<MemLogStore>>,
    reads: Arc<AtomicU32>,
    at_lsns: Arc<Mutex<Vec<Lsn>>>,
}

impl CountingSource {
    fn new(inner: Arc<MemPageStore<MemLogStore>>) -> Self {
        CountingSource {
            inner,
            reads: Arc::new(AtomicU32::new(0)),
            at_lsns: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl PageSource for CountingSource {
    fn get_page<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        page: cascade_storage_api::PageKey,
        at_lsn: Lsn,
    ) -> cascade_types::LocalBoxFuture<'a, Result<cascade_storage_api::PageImage, StoreError>>
    {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.at_lsns.lock().unwrap().push(at_lsn);
        self.inner.get_page(ctx, scope, page, at_lsn)
    }

    fn materialized_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        self.inner.materialized_lsn(scope)
    }

    fn oldest_readable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        self.inner.oldest_readable_lsn(scope)
    }

    fn set_read_floor<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        reader: cascade_storage_api::ReaderId,
        lsn: Lsn,
    ) -> cascade_types::LocalBoxFuture<'a, Result<(), StoreError>> {
        self.inner.set_read_floor(ctx, scope, reader, lsn)
    }
}

// --- shape -------------------------------------------------------------------

/// I2 survives the generalization, split in two: the POOL is shared
/// (Send + Sync — the whole point of T6.2, guards included), and the
/// PageSource contract is Send + Sync too (a source handle may be shared
/// as `Arc<dyn PageSource>`) — while the PRODUCTION sources stay
/// reactor-bound per core at the cascade-pagestore layer. The store moved
/// out of the pool; nothing store-shaped can smuggle back in.
#[test]
fn pool_is_cross_core_and_the_source_contract_is_shareable() {
    assert_impl_all!(Bpm: Send, Sync);
    assert_impl_all!(cascade_bpm::PageGuard: Send, Sync);
    assert_impl_all!(cascade_bpm::PageGuardMut: Send, Sync);
    assert_impl_all!(PerDbAccounting: Send, Sync);
    assert_impl_all!(MemPageStore<MemLogStore>: Send, Sync);
    // The contract's dyn form exists — the pool takes `&impl PageSource`
    // per call, but a caller may hold the object behind an Arc.
    let rig = Rig::with_frames(2);
    let dyn_source: Arc<dyn PageSource> = Arc::clone(&rig.pages) as Arc<dyn PageSource>;
    assert_impl_all!(CountingSource: Send, Sync);
    drop(dyn_source);
}

// --- T1.9: pin/unpin basics ---------------------------------------------------

#[test]
fn miss_reads_then_hit_pins() {
    let t = Rig::with_frames(4);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(0);
    // Miss: fresh page reads as zeroes, exactly one frame used.
    {
        let guard = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page))
            .expect("miss loads the page");
        assert_eq!(t.bpm.frames_in_use(), 1);
        assert!(guard.iter().all(|&b| b == 0), "fresh page is zeroes");
        assert_eq!(t.bpm.pin_count(&page), 1);
    }
    // The writer guard applies a logged change (the M8 apply path: the
    // stamp is the page's LSN; the same bytes went to the log first —
    // the caller's half of the mini-transaction rule).
    {
        let mut guard = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
            .expect("writer pin");
        guard.write_logged(Lsn(40), |p| p[10] = 0xAB);
    }
    assert_eq!(t.bpm.pin_count(&page), 0, "guard drop unpins");
    assert_eq!(t.bpm.is_dirty(&page), Some(true), "write marked the page");
    assert_eq!(t.bpm.page_lsn(&page), Some(Some(Lsn(40))), "the stamp is the page's LSN");

    // Hit: same frame, no second load, applied change visible in memory
    // (nothing here ever writes back — the page store has its own copy
    // from the log).
    {
        let g1 = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)).expect("hit");
        let _g2 = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)).expect("hit again");
        assert_eq!(t.bpm.frames_in_use(), 1, "no duplicate frame");
        assert_eq!(t.bpm.pin_count(&page), 2);
        assert_eq!(g1[10], 0xAB, "earlier apply visible through the hit");
    }
    assert_eq!(t.bpm.pin_count(&page), 0);
}

/// The exclusive pin: a writer guard refuses a second writer AND any
/// reader (`Contended`, fail fast — the caller owns the retry); readers
/// refuse a writer. Shared pins coexist freely.
#[test]
fn exclusive_writer_refuses_readers_and_writers() {
    let t = Rig::with_frames(4);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(3);
    let mut writer = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
        .expect("first writer");
    writer.write_logged(Lsn(5), |p| p[4] = 0x44);

    match block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)) {
        Err(BpmError::Contended(p)) => assert_eq!(p, page),
        Ok(_) => panic!("reader must refuse while the writer holds EXCL"),
        Err(other) => panic!("expected Contended, got {other:?}"),
    }
    match block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page)) {
        Err(BpmError::Contended(p)) => assert_eq!(p, page),
        Ok(_) => panic!("second writer must refuse"),
        Err(other) => panic!("expected Contended, got {other:?}"),
    }

    drop(writer);
    let r1 = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)).expect("reader 1");
    let r2 = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)).expect("reader 2");
    assert_eq!(t.bpm.pin_count(&page), 2, "readers share the frame");
    match block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page)) {
        Err(BpmError::Contended(_)) => {}
        Ok(_) => panic!("writer must refuse while readers hold the frame"),
        Err(other) => panic!("expected Contended, got {other:?}"),
    }
    drop((r1, r2));
    let _w2 = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
        .expect("writer after readers");
}

#[test]
fn panic_while_pinned_releases_the_pin() {
    let t = Rig::with_frames(4);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(1);
    let guard = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)).expect("pinned");
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
    assert_eq!(t.bpm.accounting().pinned(DB), 0);
}

#[test]
fn racing_lookups_from_one_reactor_share_one_frame() {
    let t = Rig::with_frames(4);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(2);
    let (a, b) = block_on(futures_lite::future::zip(
        t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page),
        t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page),
    ));
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
}

/// Pool full with every frame PINNED refuses — the greedy eviction may
/// only take unpinned victims. Releasing the pin lets the next miss drop
/// the clean victim instead of refusing.
#[test]
fn pinned_frame_refuses_but_released_frame_evicts() {
    let t = Rig::with_frames(1);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let _held = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(0)))
        .expect("first page fits");
    match block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(1))) {
        Err(BpmError::PoolFull { max_frames }) => assert_eq!(max_frames, 1),
        Ok(_) => panic!("expected PoolFull, got a pinned page"),
        Err(other) => panic!("expected PoolFull, got {other:?}"),
    }
    drop(_held);
    // The clean unpinned victim is now droppable — the second page fits,
    // reusing the frame.
    let second = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(1)))
        .expect("drops the victim");
    assert_eq!(t.bpm.frames_in_use(), 1, "the frame was reused");
    assert_eq!(t.bpm.page_lsn(&t.page(0)), None, "victim unmapped");
    drop(second);
}

#[test]
fn accounting_follows_pins_not_frames() {
    let t = Rig::with_frames(4);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let a = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(0))).expect("pin a");
    let b = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(1))).expect("pin b");
    assert_eq!(t.bpm.accounting().pinned(DB), 2);
    drop(a);
    assert_eq!(t.bpm.accounting().pinned(DB), 1);
    drop(b);
    assert_eq!(t.bpm.accounting().pinned(DB), 0);
}

#[test]
fn pages_are_page_sized() {
    let t = Rig::with_frames(2);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let guard = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(0)))
        .expect("loaded");
    assert_eq!(guard.len(), PAGE_SIZE);
}

// --- T6.2: cross-core sharing -------------------------------------------------

/// THE T6.2 core test: four reactors on four threads share one pool and
/// load the same page at once. The source sees 1..=REACTORS reads (each
/// miss reads outside every lock — §8.1's no-I/O-under-locks rule) but
/// the INSTALL is exactly once: one frame, ever, every guard seeing the
/// same bytes.
#[test]
fn double_install_from_many_reactors_is_exactly_once() {
    let t = Rig::build(8, None);
    let counting = Arc::new(CountingSource::new(Arc::clone(&t.pages)));
    const REACTORS: usize = 4;
    let barrier = Arc::new(Barrier::new(REACTORS));
    let page = t.page(0);

    let mut handles = Vec::new();
    for _core in 0..REACTORS {
        let bpm = Arc::clone(&t.bpm);
        let counting = Arc::clone(&counting);
        let barrier = Arc::clone(&barrier);
        let h = std::thread::Builder::new()
            .name("bpm-xcore".into())
            .spawn(move || {
                let span = cascade_telemetry::Span::noop();
                let ctx = test_ctx(&span, 7);
                barrier.wait();
                let guard =
                    block_on(bpm.page(counting.as_ref(), &ctx, SCOPE, page))
                        .expect("cross-core load");
                assert!(guard.iter().all(|&b| b == 0), "fresh page is zeroes");
                // Hold until everyone has loaded, so the pin race window is
                // fully exercised before any unpins.
                barrier.wait();
                drop(guard);
            })
            .expect("reactor thread spawns");
        handles.push(h);
    }
    for h in handles {
        h.join().expect("reactor joined cleanly");
    }
    let reads = counting.reads.load(Ordering::Relaxed);
    assert!(
        (1..=REACTORS as u32).contains(&reads),
        "expected 1..={REACTORS} source reads, got {reads}"
    );
    assert_eq!(t.bpm.frames_in_use(), 1, "one frame, not four");
    assert_eq!(t.bpm.pin_count(&page), 0);
}

/// Guards are `'static` and `Send`: a pin taken on one reactor can be
/// released on another (the unpin path is lock-based, not reactor-bound),
/// and the freed frame serves a load from yet another reactor.
#[test]
fn guards_move_across_reactors() {
    let t = Rig::with_frames(4);
    let page = t.page(5);
    {
        let bpm = Arc::clone(&t.bpm);
        let pages = Arc::clone(&t.pages);
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 7);
        let guard = block_on(bpm.page(pages.as_ref(), &ctx, SCOPE, page)).expect("pin");
        // Hand the guard to another thread while pinned.
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let guard: cascade_bpm::PageGuard = rx.recv().expect("guard arrives");
            assert_eq!(guard[0], 0);
            drop(guard); // unpins on THIS thread
        });
        tx.send(guard).expect("guard crosses threads");
        handle.join().expect("dropping thread joins");
        assert_eq!(bpm.pin_count(&page), 0, "cross-thread unpin worked");
    }
    // Another reactor loads it (the frame is still resident — hit path).
    let bpm = Arc::clone(&t.bpm);
    let pages = Arc::clone(&t.pages);
    let h = std::thread::Builder::new()
        .name("bpm-third".into())
        .spawn(move || {
            let span = cascade_telemetry::Span::noop();
            let ctx = test_ctx(&span, 7);
            let g = block_on(bpm.page(pages.as_ref(), &ctx, SCOPE, page))
                .expect("hit from another reactor");
            assert_eq!(g.len(), PAGE_SIZE);
        })
        .expect("third reactor spawns");
    h.join().expect("third reactor joins");
}

// --- storage M8: the cache rules ----------------------------------------------

/// The miss path reads at the scope's durable LSN (§4.5, §12.5): the
/// pool asks its DurableLsn view and hands that exact number to the
/// source. (The view mirrors the log's durable prefix, so the test seeds
/// a real group and raises the view to its boundary — the fake refuses a
/// request past what the log has made durable, which is promise 2.)
#[test]
fn miss_reads_at_the_scopes_durable_lsn() {
    let t = Rig::with_frames(4);
    let stamp = t.seed(9, &[0x07]);
    t.durable_to(stamp);
    let counting = CountingSource::new(Arc::clone(&t.pages));
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(9);
    let _guard = block_on(t.bpm.page(&counting, &ctx, SCOPE, page)).expect("load");
    let ats = counting.at_lsns.lock().unwrap().clone();
    assert_eq!(
        ats,
        vec![stamp],
        "the miss fetched the page exactly as of the durable LSN"
    );
}

/// A seeded page loads with the store's image and the image's own page
/// LSN (S17) — not the read point.
#[test]
fn loaded_page_serves_the_store_image_and_page_lsn() {
    let t = Rig::with_frames(4);
    let stamp = t.seed(0, &[0xA5, 0x5A]);
    t.durable_to(stamp);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(0);
    let guard = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)).expect("load");
    assert_eq!(guard[0], 0xA5, "the toy apply writes content at offset 0");
    assert_eq!(guard[1], 0x5A);
    drop(guard);
    assert_eq!(
        t.bpm.page_lsn(&page),
        Some(Some(stamp)),
        "the frame remembers the image's page LSN, not the read point"
    );
}

/// §12.5's rule, pinned: a page whose stamp is above the durable LSN is
/// never evicted — not by name, not by the ladder. When durability
/// covers the stamp, the same page drops and the miss proceeds. The
/// undurable window is the honest pool pressure (§12.5's open question).
#[test]
fn never_evicts_above_the_durable_lsn() {
    let t = Rig::with_frames(1);
    let stamp = t.seed(0, &[0x01]);
    let group_end = *t.next_lsn.lock().unwrap();
    t.durable_to(stamp);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let victim = t.page(0);
    {
        let mut g = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, victim))
            .expect("load victim");
        // The apply-ahead-of-durability fiction: a group applied to the
        // cached page whose log bytes the durable LSN has not covered
        // yet (group_end = the boundary being reserved in the log).
        g.write_logged(Lsn(group_end), |p| p[6] = 0x66);
    }
    assert!(
        !t.bpm.evict_page(victim).expect("no error — a refusal"),
        "above the durable LSN: eviction refuses"
    );
    assert_eq!(t.bpm.frames_in_use(), 1, "still resident");
    // The ladder refuses too: a full pool with this one undurable frame
    // cannot serve the miss.
    match block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(1))) {
        Err(BpmError::PoolFull { max_frames }) => assert_eq!(max_frames, 1),
        Ok(_) => panic!("the undurable victim must not be dropped"),
        Err(other) => panic!("expected PoolFull, got {other:?}"),
    }
    // Durability covers the stamp; now the same page drops with no write
    // (there is no write path at all), and the miss proceeds.
    t.durable_to(Lsn(group_end));
    let second = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(1)))
        .expect("the miss succeeds once the durable LSN covers the victim");
    assert_eq!(t.bpm.frames_in_use(), 1, "victim dropped, newcomer loaded");
    assert_eq!(t.bpm.page_lsn(&victim), None, "victim gone");
    drop(second);
}

/// Dropping a durable-covered page loses nothing: the store serves the
/// same image again on the next miss (§12.5's invariant).
#[test]
fn evict_below_durable_drops_and_the_store_serves_it_again() {
    let t = Rig::with_frames(2);
    let stamp = t.seed(0, &[0x42]);
    t.durable_to(stamp);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(0);
    {
        let g = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page)).expect("load");
        assert_eq!(g[0], 0x42);
    }
    assert!(
        t.bpm.evict_page(page).expect("durable-covered page evicts"),
        "dropped"
    );
    assert_eq!(t.bpm.frames_in_use(), 0, "frame reclaimed");
    let again = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, page))
        .expect("re-load after the drop");
    assert_eq!(again[0], 0x42, "the store served the same image again");
    assert_eq!(
        t.bpm.page_lsn(&page),
        Some(Some(stamp)),
        "and the same page LSN with it"
    );
}

/// Unlogged changes (plain `write`, no stamp) are never evictable in v1:
/// the log cannot rebuild them, so an eviction would lose the image
/// outright. The ladder skips such a frame — pool pressure, honestly.
#[test]
fn unlogged_dirty_page_refuses_eviction() {
    let t = Rig::with_frames(1);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(3);
    {
        let mut g = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
            .expect("load");
        g.write(|p| p[9] = 0x99);
    }
    assert_eq!(t.bpm.page_lsn(&page), Some(None), "plain write stamps nothing");
    assert!(
        !t.bpm.evict_page(page).expect("no error — a refusal"),
        "unlogged changes refuse eviction"
    );
    assert_eq!(t.bpm.frames_in_use(), 1, "still resident");
    // And the ladder cannot drop it either.
    match block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(1))) {
        Err(BpmError::PoolFull { .. }) => {}
        Ok(_) => panic!("the unlogged victim must not be dropped"),
        Err(other) => panic!("expected PoolFull, got {other:?}"),
    }
}

/// A clean loaded page drops without touching the source — eviction is
/// an unmap, and the M8 pool has no write path to hide behind it.
#[test]
fn clean_loaded_page_evicts_without_touching_the_source() {
    let t = Rig::with_frames(2);
    let counting = CountingSource::new(Arc::clone(&t.pages));
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(0);
    {
        let _g = block_on(t.bpm.page(&counting, &ctx, SCOPE, page)).expect("load");
    }
    let reads_after_load = counting.reads.load(Ordering::Relaxed);
    assert!(t.bpm.evict_page(page).expect("clean page evicts"));
    assert_eq!(
        counting.reads.load(Ordering::Relaxed),
        reads_after_load,
        "eviction did no I/O — it dropped, it did not write"
    );
    assert_eq!(t.bpm.frames_in_use(), 0);
}

/// The ladder's last rung on a full pool: a miss drops a DURABLE-COVERED
/// dirty victim in place — no source call, no wait — and the load
/// completes. The caller never sees a refusal.
#[test]
fn miss_on_full_pool_drops_a_durable_covered_victim() {
    let t = Rig::with_frames(1);
    let counting = CountingSource::new(Arc::clone(&t.pages));
    let stamp = t.seed(0, &[0x01]);
    t.durable_to(stamp);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let victim = t.page(0);
    {
        let mut g = block_on(t.bpm.page_exclusive(&counting, &ctx, SCOPE, victim))
            .expect("load victim");
        // Logged and durable-covered (stamp == durable): dirty but
        // droppable.
        g.write_logged(stamp, |p| p[10] = 0x01);
    }
    assert_eq!(counting.reads.load(Ordering::Relaxed), 1);
    let loaded = block_on(t.bpm.page(&counting, &ctx, SCOPE, t.page(1)))
        .expect("the miss completes, dropping the covered victim");
    assert_eq!(t.bpm.frames_in_use(), 1, "victim dropped, newcomer loaded");
    assert_eq!(t.bpm.page_lsn(&victim), None, "victim gone");
    assert_eq!(
        counting.reads.load(Ordering::Relaxed),
        2,
        "exactly the newcomer's miss — the drop made no source call"
    );
    drop(loaded);
}

/// An unlogged mutation AFTER a logged apply clears the stamp: a stale
/// covered stamp must never make the frame look rebuildable while the
/// image carries changes the log cannot reproduce. Eviction refuses at
/// any durable LSN.
#[test]
fn unlogged_write_after_a_logged_stamp_refuses_eviction() {
    let t = Rig::with_frames(1);
    let stamp = t.seed(0, &[0x01]);
    let group_end = *t.next_lsn.lock().unwrap();
    t.durable_to(Lsn(group_end));
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(0);
    {
        let mut g = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
            .expect("load");
        g.write_logged(stamp, |p| p[5] = 0x55);
    }
    assert_eq!(t.bpm.page_lsn(&page), Some(Some(stamp)));
    {
        let mut g = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
            .expect("re-open");
        // The unlogged tweak: the image now differs from anything the
        // log can rebuild.
        g.write(|p| p[6] = 0x66);
    }
    assert_eq!(
        t.bpm.page_lsn(&page),
        Some(None),
        "the unlogged write cleared the stale stamp"
    );
    assert!(
        !t.bpm.evict_page(page).expect("no error — a refusal"),
        "the mixed frame must not be droppable through its old stamp"
    );
    // The ladder refuses too: the only frame is (dirty, no stamp), and
    // no durable advance changes that — no stamp means no coverage
    // relation at all.
    match block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(1))) {
        Err(BpmError::PoolFull { .. }) => {}
        Ok(_) => panic!("the mixed frame must not be dropped by the ladder"),
        Err(other) => panic!("expected PoolFull, got {other:?}"),
    }
}

/// The stamp never moves backward: two groups whose boundaries landed in
/// either pin order leave the stamp at the HIGHER end, so the frame
/// stays undroppable until durability covers the newest applied record.
#[test]
fn the_stamp_never_moves_backward() {
    let t = Rig::with_frames(2);
    let _stamp = t.seed(0, &[0x01]);
    let group_end = *t.next_lsn.lock().unwrap();
    t.durable_to(Lsn(group_end));
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(0);
    let e_high = Lsn(group_end + 200);
    let e_low = Lsn(group_end + 100);
    {
        let mut g = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
            .expect("load");
        // The out-of-order fiction: the later group applies first.
        g.write_logged(e_high, |p| p[5] = 0x55);
        g.write_logged(e_low, |p| p[6] = 0x66);
    }
    assert_eq!(
        t.bpm.page_lsn(&page),
        Some(Some(e_high)),
        "the backward apply must not drag the stamp down"
    );
    assert!(
        !t.bpm.evict_page(page).expect("no error — a refusal"),
        "undurable at e_low's coverage: the frame must refuse"
    );
    t.durable_to(e_low);
    assert!(
        !t.bpm.evict_page(page).expect("no error — a refusal"),
        "durability covering the LOWER boundary is not enough"
    );
    t.durable_to(e_high);
    assert!(
        t.bpm.evict_page(page).expect("covered now"),
        "durability covering the true (higher) stamp drops the frame"
    );
}

/// The `Lsn(0)` sentinel means "no boundary": it clears the stamp like
/// plain `write` — an uncovered change must never hide behind an older,
/// covered stamp.
#[test]
fn the_zero_sentinel_clears_the_stamp() {
    let t = Rig::with_frames(2);
    let stamp = t.seed(0, &[0x01]);
    t.durable_to(stamp);
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let page = t.page(0);
    {
        let mut g = block_on(t.bpm.page_exclusive(t.pages.as_ref(), &ctx, SCOPE, page))
            .expect("load");
        g.write_logged(stamp, |p| p[5] = 0x55);
        g.write_logged(Lsn(0), |p| p[6] = 0x66);
    }
    assert_eq!(
        t.bpm.page_lsn(&page),
        Some(None),
        "the sentinel is an uncovered mutation: the stamp clears"
    );
    assert!(
        !t.bpm.evict_page(page).expect("no error — a refusal"),
        "the frame keeps its changes until the layer above decides"
    );
}

// --- placement ------------------------------------------------------------------

/// An unroutable node degrades to first-touch with node_bound() false —
/// the warn-and-degrade contract (I4a), runnable on any host.
#[test]
fn unroutable_node_degrades_to_first_touch() {
    let t = Rig::build(2, Some(999));
    assert!(
        !t.bpm.node_bound(),
        "no kernel has node 999 — placement must be reported as best-effort"
    );
    // The degraded pool still works end to end.
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    let g = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(0)))
        .expect("degraded pool serves");
    assert_eq!(g.len(), PAGE_SIZE);
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
    let t = Rig::build(8, Some(numa));
    assert!(t.bpm.node_bound(), "the kernel accepted the bind on bare metal");

    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 7);
    // Fault the frames: load 4 pages (every load faults its frame).
    for i in 0..4 {
        let g = block_on(t.bpm.page(t.pages.as_ref(), &ctx, SCOPE, t.page(i)))
            .expect("faults a frame");
        assert_eq!(g.len(), PAGE_SIZE);
    }
    let counts = numa_maps_counts(t.bpm.ram_addr());
    let total: u64 = counts.values().sum();
    assert!(total > 0, "the pool mapping has no resident pages");
    let on_node = counts.get(&numa).copied().unwrap_or(0);
    assert_eq!(on_node, total, "pool frames span nodes: {counts:?}");
}
