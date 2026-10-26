//! T1.9: the single-pool BPM — hit/miss/pin/unpin, panic-release, the
//! same-reactor lookup race, pool-full refusal, and the I2 compile-shape.
//! Every test runs one reactor on the test thread with a real `CoreStorage`
//! over a temp dir.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

use cascade_bpm::{Bpm, BpmConfig, BpmError, PerDbAccounting};
use cascade_storage::{CoreStorage, StorageConfig, StorageManager};
use cascade_types::{DatabaseId, PageId, SegmentId, PAGE_SIZE};
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
