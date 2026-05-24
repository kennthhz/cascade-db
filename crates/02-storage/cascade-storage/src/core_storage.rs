//! `CoreStorage` — the per-CPU-core, `!Send`, `!Sync` storage engine.
//!
//! Originally drafted as `storage/src/core_storage.rs` during Phase 1; moved
//! here when the workspace was restructured. Type definitions (`PageId`, `Lsn`,
//! `AlignedBuf`) now come from [`cascade-types`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::cell::RefCell;
use tokio_uring::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;

use cascade_types::{AlignedBuf, PageId, PAGE_SIZE};

use crate::traits::{PageStore, StorageError};

pub struct CoreStorage {
    core_id: usize,
    base_data_dir: PathBuf,
    base_wal_dir: PathBuf,

    // Lock-free cache of open File Descriptors.
    // Rc is safe here because CoreStorage is !Send (thread-local).
    data_files: RefCell<HashMap<(u32, u32), Rc<File>>>,
    wal_files: RefCell<HashMap<u32, Rc<File>>>,

    // Tracks the current tail byte offset (LSN) for each database's WAL
    wal_offsets: RefCell<HashMap<u32, u64>>,
}

impl CoreStorage {
    /// Internal helper to get or open a data file with O_DIRECT
    async fn get_data_file(&self, db_id: u32, space_id: u32) -> Result<Rc<File>, StorageError> {
        let mut cache = self.data_files.borrow_mut();
        if let Some(file) = cache.get(&(db_id, space_id)) {
            return Ok(Rc::clone(file));
        }

        // e.g., /data_dir/db_10/space_25.dat
        let path = self.base_data_dir.join(format!("db_{}", db_id)).join(format!("space_{}.dat", space_id));

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .custom_flags(libc::O_DIRECT) // Bypass the Linux Page Cache!
            .open(path)
            .await
            .map_err(StorageError::Io)?;

        let rc_file = Rc::new(file);
        cache.insert((db_id, space_id), Rc::clone(&rc_file));
        Ok(rc_file)
    }

    /// Internal helper to get or open a WAL file (O_APPEND is handled manually via offset)
    async fn get_wal_file(&self, _db_id: u32) -> Result<Rc<File>, StorageError> {
        // ... similar logic to get_data_file, but points to wal_dir
        // and doesn't necessarily need O_DIRECT if we rely on fsync for WAL ...
        todo!()
    }
}

// -----------------------------------------------------------------------------
// Random I/O Implementation (Data Pages)
// -----------------------------------------------------------------------------
impl PageStore for CoreStorage {
    async fn read_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> (AlignedBuf, Result<(), StorageError>) {
        let file_res = self.get_data_file(page_id.db.0, page_id.segment.0).await;
        let file = match file_res {
            Ok(f) => f,
            Err(e) => return (buf, Err(e)),
        };

        let offset = (page_id.page_no as u64) * PAGE_SIZE as u64;

        // tokio-uring takes ownership of `buf` and returns it when the kernel is done.
        // NOTE: AlignedBuf integration with tokio-uring is filled in during Phase 2.
        let _ = (file, offset);
        todo!("Phase 2: wire AlignedBuf through tokio-uring read_at")
    }

    async fn write_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> (AlignedBuf, Result<(), StorageError>) {
        let _ = (page_id, buf);
        todo!("Phase 2: wire AlignedBuf through tokio-uring write_at")
    }

    async fn read_pages(
        &self,
        start_page_id: PageId,
        bufs: Vec<AlignedBuf>,
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>) {
        // To do vectored I/O with tokio-uring, we can concurrently submit
        // multiple read_at calls to the ring. The kernel will batch them.
        let _ = (start_page_id, bufs);
        todo!()
    }

    async fn write_pages(
        &self,
        start_page_id: PageId,
        bufs: Vec<AlignedBuf>,
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>) {
        let _ = (start_page_id, bufs);
        todo!()
    }

    async fn allocate_extent(&self, db_id: u32, space_id: u32, num_pages: u32) -> Result<u32, StorageError> {
        let file = self.get_data_file(db_id, space_id).await?;
        let _bytes_to_allocate = (num_pages as u64) * PAGE_SIZE as u64;

        // Note: tokio-uring provides `fallocate` to reserve disk blocks at the OS level
        // file.fallocate(0, current_size, bytes_to_allocate).await?;
        let _ = file;
        todo!()
    }

    async fn free_extent(&self, _db_id: u32, _space_id: u32, _start_page: u32, _num_pages: u32) -> Result<(), StorageError> {
        // Uses `fallocate` with FALLOC_FL_PUNCH_HOLE
        todo!()
    }
}

#[allow(dead_code)]
fn _silence_unused_field_warnings(s: &CoreStorage) {
    let _ = (&s.core_id, &s.base_wal_dir, &s.wal_files, &s.wal_offsets);
}
