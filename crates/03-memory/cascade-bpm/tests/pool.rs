//! T1.9: the single-pool BPM — hit/miss/pin/unpin, panic-release, the
//! same-reactor lookup race, pool-full refusal, and the I2 compile-shape.
//! Every test runs one reactor on the test thread with a real `CoreStorage`
//! over a temp dir.
//!
//! T3.2 adds the WAL-before-data writeback gate: blocked/refused/gated
//! writeback, gate-less stamping refusal, unlogged writes, gated eviction
//! with frame reclamation, and the re-modified-during-wait re-check.

use std::{
    cell::Cell,
    future::Future,
    path::PathBuf,
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use cascade_bpm::{Bpm, BpmConfig, BpmError, PerDbAccounting, WalDurability};
use cascade_storage::{CoreStorage, PageStore, StorageConfig, StorageManager};
use cascade_types::{DatabaseId, Lsn, PageId, SegmentId, PAGE_SIZE};
use static_assertions::assert_not_impl_any;

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("bpm-test")
        .make()
        .expect("executor builds")
}

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

struct TempPool {
    dir: PathBuf,
    bpm: Bpm<CoreStorage>,
}

impl TempPool {
    fn with_frames(max_frames: u32) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cascade-bpm-test-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let manager = StorageManager::mount(StorageConfig {
            data_dir: dir.clone(),
            wal_dir: dir.join("wal"),
            io_uring_entries: 64,
        })
        .expect("mount");
        let store = manager.local_worker(0);
        TempPool {
            dir,
            bpm: Bpm::new(store, BpmConfig { max_frames }, PerDbAccounting::default()),
        }
    }

    fn page(&self, page_no: u32) -> PageId {
        PageId {
            db: DatabaseId(7),
            segment: SegmentId(3),
            page_no,
        }
    }
}

impl Drop for TempPool {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// I2/shape: the pool over the real store stays reactor-bound — it must
/// never be sharable across threads.
#[test]
fn pool_and_store_are_reactor_bound() {
    assert_not_impl_any!(CoreStorage: Send, Sync);
    let t = TempPool::with_frames(2);
    assert_not_impl_any!(Bpm<CoreStorage>: Send, Sync);
    drop(t);
}

#[test]
fn miss_reads_then_hit_pins() {
    let t = TempPool::with_frames(4);
    reactor().run(async {
        let page = t.page(0);
        // Miss: fresh page reads as zeroes, exactly one frame used.
        {
            let mut guard = t.bpm.page(page).await.expect("miss loads the page");
            assert_eq!(t.bpm.frames_in_use(), 1);
            assert!(guard.iter().all(|&b| b == 0), "fresh page is zeroes");
            assert_eq!(t.bpm.pin_count(&page), 1);
            // Exclusive-pin write through the guard.
            guard.write(|page| page[10] = 0xAB); // exclusive-pin write
        }
        assert_eq!(t.bpm.pin_count(&page), 0, "guard drop unpins");
        assert_eq!(t.bpm.is_dirty(&page), Some(true), "write marked dirty");

        // Hit: same frame, no second load, mutation visible in memory
        // (writeback is M3 — the dirty page simply stays dirty).
        {
            let g1 = t.bpm.page(page).await.expect("hit");
            let _g2 = t.bpm.page(page).await.expect("hit again");
            assert_eq!(t.bpm.frames_in_use(), 1, "no duplicate frame");
            assert_eq!(t.bpm.pin_count(&page), 2);
            assert_eq!(g1[10], 0xAB, "earlier write visible through the hit");
        }
        assert_eq!(t.bpm.pin_count(&page), 0);
    });
}

#[test]
fn writes_require_the_exclusive_pin() {
    let t = TempPool::with_frames(4);
    reactor().run(async {
        let page = t.page(3);
        let mut g1 = t.bpm.page(page).await.expect("first guard");
        let g2 = t.bpm.page(page).await.expect("second guard");
        // Two guards → pin 2 → the &mut would alias g2's reads.
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            g1.write(|_| panic!("must not run"));
        }));
        assert!(refused.is_err(), "write with pin 2 must refuse");
        drop(g2);
        g1.write(|page| page[0] = 7); // exclusive again: proceeds
    });
}

#[test]
fn panic_while_pinned_releases_the_pin() {
    let t = TempPool::with_frames(4);
    reactor().run(async {
        let page = t.page(1);
        let mut guard = t.bpm.page(page).await.expect("pinned");
        guard.write(|page| page[0] = 1);
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
    let t = TempPool::with_frames(4);
    reactor().run(async {
        let page = t.page(2);
        let (a, b) = futures_lite::future::zip(t.bpm.page(page), t.bpm.page(page)).await;
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

#[test]
fn full_pool_refuses_instead_of_evicting() {
    let t = TempPool::with_frames(1);
    reactor().run(async {
        let _held = t.bpm.page(t.page(0)).await.expect("first page fits");
        match t.bpm.page(t.page(1)).await {
            Err(BpmError::PoolFull { max_frames }) => assert_eq!(max_frames, 1),
            Ok(_) => panic!("expected PoolFull, got a pinned page"),
            Err(other) => panic!("expected PoolFull, got {other:?}"),
        }
    });
}

#[test]
fn accounting_follows_pins_not_frames() {
    let t = TempPool::with_frames(4);
    reactor().run(async {
        let db = DatabaseId(7);
        let a = t.bpm.page(t.page(0)).await.expect("pin a");
        let b = t.bpm.page(t.page(1)).await.expect("pin b");
        assert_eq!(t.bpm.accounting().pinned(db), 2);
        drop(a);
        assert_eq!(t.bpm.accounting().pinned(db), 1);
        drop(b);
        assert_eq!(t.bpm.accounting().pinned(db), 0);
    });
}

#[test]
fn pages_are_page_sized() {
    let t = TempPool::with_frames(2);
    reactor().run(async {
        let guard = t.bpm.page(t.page(0)).await.expect("loaded");
        assert_eq!(guard.len(), PAGE_SIZE);
    });
}

// --- T3.2: the WAL-before-data writeback gate -------------------------------

/// Controllable fake gate: the test moves `durable` forward by hand; waits
/// poll on a 1 ms tick until coverage (or fail immediately when `fail`).
/// `waits` counts gate entries — the blocked-flush assertions read it.
struct FakeGate {
    durable: Cell<Lsn>,
    waits: AtomicU32,
    fail: bool,
}

impl WalDurability for FakeGate {
    fn durable_lsn(&self) -> Lsn {
        self.durable.get()
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
            while self.durable.get() < lsn {
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
    let t = TempPool::with_frames(4);
    let gate = Rc::new(FakeGate {
        durable: Cell::new(Lsn(0)),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(gate.clone());
    reactor().run(async {
        let page = t.page(0);
        let end = Lsn(100);
        {
            let mut g = t.bpm.page(page).await.expect("load");
            g.write_logged(end, |p| p[5] = 0x5A);
        }
        assert_eq!(t.bpm.page_lsn(&page), Some(Some(end)));

        let probe = async {
            // While the gate is closed: parked (at least one gate entry),
            // still dirty, and nothing on disk.
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert!(gate.waits.load(Ordering::Relaxed) >= 1, "flusher entered the gate");
            assert_eq!(t.bpm.is_dirty(&page), Some(true), "blocked page stays dirty");
            let on_disk = t.bpm.store().read_page(page).await.expect("disk read");
            assert!(
                on_disk.as_slice().iter().all(|&b| b == 0),
                "nothing may reach the store ahead of its WAL"
            );
            // Durability advances past the stamp; the same flush completes.
            gate.durable.set(end);
            glommio::timer::sleep(Duration::from_millis(3)).await;
        };
        let ((), wrote) = futures_lite::future::zip(probe, t.bpm.flush_page(page)).await;
        assert!(wrote.expect("flush succeeds"), "the page was written");
        assert_eq!(t.bpm.is_dirty(&page), Some(false), "written page clears dirty");
        let on_disk = t.bpm.store().read_page(page).await.expect("disk read");
        assert_eq!(on_disk.as_slice()[5], 0x5A, "the gated image is on disk");
    });
}

/// Fault injection at the gate: a gate that fails leaves the page dirty
/// and the store untouched — writeback refused, not half-done.
#[test]
fn writeback_refused_when_the_gate_fails() {
    let t = TempPool::with_frames(4);
    let gate = Rc::new(FakeGate {
        durable: Cell::new(Lsn(0)),
        waits: AtomicU32::new(0),
        fail: true,
    });
    t.bpm.set_gate(gate.clone());
    reactor().run(async {
        let page = t.page(1);
        {
            let mut g = t.bpm.page(page).await.expect("load");
            g.write_logged(Lsn(50), |p| p[3] = 0x33);
        }
        match t.bpm.flush_page(page).await {
            Err(BpmError::WalGate(_)) => {}
            other => panic!("expected a gate refusal, got {other:?}"),
        }
        assert_eq!(t.bpm.is_dirty(&page), Some(true), "refused page stays dirty");
        let on_disk = t.bpm.store().read_page(page).await.expect("disk read");
        assert!(on_disk.as_slice().iter().all(|&b| b == 0), "nothing reached the store");
    });
}

/// A page carrying a WAL boundary but no attached gate must refuse
/// writeback outright — an ungated write of a logged page is the exact
/// violation WAL-before-data exists to prevent. Silent success here would
/// be the bug.
#[test]
fn stamped_page_without_a_gate_refuses_writeback() {
    let t = TempPool::with_frames(4);
    reactor().run(async {
        let page = t.page(2);
        {
            let mut g = t.bpm.page(page).await.expect("load");
            g.write_logged(Lsn(7), |p| p[1] = 0x11);
        }
        match t.bpm.flush_page(page).await {
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
    let t = TempPool::with_frames(4);
    reactor().run(async {
        let page = t.page(3);
        {
            let mut g = t.bpm.page(page).await.expect("load");
            g.write(|p| p[9] = 0x99);
        }
        assert_eq!(t.bpm.page_lsn(&page), Some(None), "plain write stamps nothing");
        let wrote = t.bpm.flush_page(page).await.expect("ungated write succeeds");
        assert!(wrote);
        assert_eq!(t.bpm.is_dirty(&page), Some(false));
        let on_disk = t.bpm.store().read_page(page).await.expect("disk read");
        assert_eq!(on_disk.as_slice()[9], 0x99);
    });
}

/// The subtlest path: the page is re-modified while the gate wait is
/// parked — its stamp moves past the boundary that just became durable —
/// so the flush must extend its wait to the new stamp and write the NEWER
/// image, then clear dirty. Clearing dirty on the stale pass would lose
/// the re-modification on the next eviction.
#[test]
fn re_modification_during_the_gate_wait_extends_the_wait() {
    let t = TempPool::with_frames(4);
    let gate = Rc::new(FakeGate {
        durable: Cell::new(Lsn(0)),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(gate.clone());
    reactor().run(async {
        let page = t.page(0);
        let e1 = Lsn(100);
        {
            let mut g = t.bpm.page(page).await.expect("load");
            g.write_logged(e1, |p| p[5] = 0x55);
        }

        let probe = async {
            // The flush parks on e1; re-modify under it (the flush holds
            // no pin — that is the point of the post-await re-checks).
            glommio::timer::sleep(Duration::from_millis(3)).await;
            let e2 = Lsn(200);
            {
                let mut g = t.bpm.page(page).await.expect("re-open");
                g.write_logged(e2, |p| p[6] = 0x66);
            }
            // Durability reaches e1 only — not enough for the moved stamp.
            gate.durable.set(e1);
            glommio::timer::sleep(Duration::from_millis(3)).await;
            assert_eq!(
                t.bpm.is_dirty(&page),
                Some(true),
                "the extended wait keeps the page dirty"
            );
            gate.durable.set(e2);
        };
        let ((), wrote) = futures_lite::future::zip(probe, t.bpm.flush_page(page)).await;
        assert!(wrote.expect("flush eventually succeeds"));
        assert_eq!(t.bpm.is_dirty(&page), Some(false), "written through the new stamp");
        let on_disk = t.bpm.store().read_page(page).await.expect("disk read");
        assert_eq!(on_disk.as_slice()[6], 0x66, "the newer image is what landed");
    });
}

/// Eviction routes dirty pages through the same gate (no second writeback
/// path), and a freed frame is reusable — the pool-full refusal clears
/// once the victim is evicted.
#[test]
fn dirty_eviction_routes_through_the_gate_and_reclaims_the_frame() {
    let t = TempPool::with_frames(1);
    let gate = Rc::new(FakeGate {
        durable: Cell::new(Lsn(0)),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(gate.clone());
    reactor().run(async {
        let victim = t.page(0);
        let end = Lsn(80);
        {
            let mut g = t.bpm.page(victim).await.expect("load victim");
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
            gate.durable.set(end);
        };
        let ((), evicted) = futures_lite::future::zip(probe, t.bpm.evict_page(victim)).await;
        assert!(evicted.expect("evict succeeds"), "evicted");
        assert_eq!(t.bpm.is_dirty(&victim), None, "no longer resident");
        assert_eq!(t.bpm.frames_in_use(), 0, "frame reclaimed");

        // The freed slot serves a new load, and the evicted page reads
        // back from the store — its gated image.
        let reloaded = t.bpm.page(victim).await.expect("re-load after eviction");
        assert_eq!(t.bpm.frames_in_use(), 1, "the freed frame was reused");
        assert_eq!(reloaded[10], 0x01, "the gated image survived eviction");

        // Pool full again → refusal; evict (the reload is unlogged and
        // unpinned, so eviction is gate-free) → a second page fits.
        let second = t.page(1);
        match t.bpm.page(second).await {
            Err(BpmError::PoolFull { max_frames }) => assert_eq!(max_frames, 1),
            Ok(_) => panic!("expected PoolFull, got a pinned page"),
            Err(other) => panic!("expected PoolFull, got {other:?}"),
        }
        drop(reloaded);
        assert!(
            t.bpm.evict_page(victim).await.expect("clean eviction"),
            "evicted"
        );
        let _ = t.bpm.page(second).await.expect("fits after eviction");
    });
}

/// Clean pages evict without touching the gate or the store.
#[test]
fn clean_eviction_writes_nothing() {
    let t = TempPool::with_frames(2);
    let gate = Rc::new(FakeGate {
        durable: Cell::new(Lsn(0)),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(gate.clone());
    reactor().run(async {
        let page = t.page(0);
        drop(t.bpm.page(page).await.expect("load clean"));
        assert!(t.bpm.evict_page(page).await.expect("evicts"));
        assert_eq!(t.bpm.frames_in_use(), 0);
        assert_eq!(gate.waits.load(Ordering::Relaxed), 0, "the gate was never entered");
        let on_disk = t.bpm.store().read_page(page).await.expect("disk read");
        assert!(on_disk.as_slice().iter().all(|&b| b == 0), "a clean eviction wrote nothing");
    });
}

/// A pinned page refuses both writeback and eviction (this slice's rule);
/// nothing is written and the frame stays.
#[test]
fn pinned_page_refuses_writeback_and_eviction() {
    let t = TempPool::with_frames(2);
    let gate = Rc::new(FakeGate {
        durable: Cell::new(Lsn(500)),
        waits: AtomicU32::new(0),
        fail: false,
    });
    t.bpm.set_gate(gate.clone());
    reactor().run(async {
        let page = t.page(0);
        let mut guard = t.bpm.page(page).await.expect("load");
        guard.write_logged(Lsn(10), |p| p[2] = 0x22);
        let wrote = t.bpm.flush_page(page).await.expect("no error — a refusal");
        assert!(!wrote, "pinned page refused writeback");
        let evicted = t.bpm.evict_page(page).await.expect("no error — a refusal");
        assert!(!evicted, "pinned page refused eviction");
        assert_eq!(t.bpm.frames_in_use(), 1, "still resident");
        assert_eq!(gate.waits.load(Ordering::Relaxed), 0, "never reached the gate");
    });
}
