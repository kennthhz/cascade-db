//! T1.9: the glommio `PageStore` — write/read roundtrip, fresh-page zeroes,
//! CRC corruption detection. Each test runs its own reactor on the test
//! thread and its own data dir under the OS temp dir.

use std::{
    io::{Seek, Write},
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

use cascade_storage::{CoreStorage, PageStore, StorageConfig, StorageError, StorageManager};
use cascade_types::{DatabaseId, PageId, SegmentId, PAGE_SIZE};

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("store-test")
        .make()
        .expect("executor builds")
}

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// Unique per-test data dir; cleaned up when the returned guard drops.
struct TempStore {
    dir: PathBuf,
    store: CoreStorage,
}

impl TempStore {
    fn mount() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cascade-store-test-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let manager = StorageManager::mount(StorageConfig {
            data_dir: dir.clone(),
            wal_dir: dir.join("wal"),
            io_uring_entries: 64,
        })
        .expect("mount creates the storage tree");
        TempStore {
            dir,
            store: manager.local_worker(0),
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

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn patterned_page(seed: u8) -> cascade_types::AlignedBuf {
    let mut buf = cascade_types::AlignedBuf::zeroed(PAGE_SIZE);
    for (i, b) in buf.as_mut_slice()[4..].iter_mut().enumerate() {
        *b = seed.wrapping_add(i as u8);
    }
    buf
}

#[test]
fn write_then_read_roundtrips_the_payload() {
    let t = TempStore::mount();
    reactor().run(async {
        let page = t.page(0);
        let written = t
            .store
            .write_page(page, patterned_page(42))
            .await
            .expect("write succeeds");
        let read = t.store.read_page(page).await.expect("read succeeds");
        // The payload region is identical; the reserved CRC bytes are
        // stamped by storage (write ignores the caller's leading bytes).
        assert_eq!(
            read.as_slice()[4..],
            written.as_slice()[4..],
            "payload survives the disk roundtrip"
        );
    });
}

#[test]
fn never_written_page_reads_as_zeroes() {
    let t = TempStore::mount();
    reactor().run(async {
        // Page 5 of a brand-new table: past EOF, so all zeroes — fresh
        // files grow on first write.
        let read = t.store.read_page(t.page(5)).await.expect("read succeeds");
        assert!(read.as_slice().iter().all(|&b| b == 0));
    });
}

#[test]
fn crc_catches_a_corrupted_page() {
    let t = TempStore::mount();
    let page = t.page(0);
    let on_disk = t.dir.join("db_7/space_3.dat");
    // One reactor for the whole lifecycle: a CoreStorage (and its cached
    // handles) is bound to the reactor that opened it.
    reactor().run(async {
        t.store
            .write_page(page, patterned_page(9))
            .await
            .expect("write succeeds");

        // Flip one payload byte on disk, flushing the buffered write so the
        // O_DIRECT read (which bypasses the page cache) sees it.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&on_disk)
            .expect("file exists after write");
        f.seek(std::io::SeekFrom::Start(100)).expect("seek");
        f.write_all(&[0xFF]).expect("corrupt one byte");
        f.sync_all().expect("flush corruption to disk");
        drop(f);

        match t.store.read_page(page).await {
            Err(StorageError::Corruption(p)) => assert_eq!(p, page),
            other => panic!("expected Corruption({page:?}), got {other:?}"),
        }
    });
}

#[test]
fn wrong_size_buffer_is_refused() {
    let t = TempStore::mount();
    reactor().run(async {
        let small = cascade_types::AlignedBuf::zeroed(512);
        match t.store.write_page(t.page(0), small).await {
            Err(StorageError::UnalignedBuffer) => {}
            other => panic!("expected UnalignedBuffer, got {other:?}"),
        }
    });
}
