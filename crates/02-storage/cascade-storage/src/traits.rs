//! `PageStore` trait, error type, and bootstrap structs.
//!
//! Originally drafted as `storage/src/traits.rs` during Phase 1; moved here
//! when the workspace was restructured. Type definitions (`PageId`, `Lsn`,
//! `AlignedBuf`) now live in [`cascade-types`].

use std::io::Result;
use std::path::PathBuf;

use cascade_types::{AlignedBuf, Lsn, PageId};

#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    Corruption(PageId), // e.g., CRC32 Checksum failed on read
    UnalignedBuffer,    // Buffer didn't meet O_DIRECT requirements
    OutOfSpace,
    ShortRead,          // Hit EOF before filling all requested buffers
}

// -----------------------------------------------------------------------------
// 1. The Random I/O Interface (Used by the Buffer Pool)
// -----------------------------------------------------------------------------
pub trait PageStore {
    /// Reads a single 8KB page from the NVMe drive.
    /// Takes ownership of the AlignedBuf and returns it to avoid copying.
    async fn read_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> (AlignedBuf, Result<(), StorageError>);

    /// Reads a contiguous range of 8KB pages from disk into multiple buffers.
    /// Highly optimized for Sequential Scans and Prefetching via io_uring vectored I/O.
    /// The `bufs` length determines how many sequential pages are read starting at `start_page_id`.
    async fn read_pages(
        &self,
        start_page_id: PageId,
        bufs: Vec<AlignedBuf>,
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>);

    /// Writes an 8KB page via O_DIRECT.
    /// The Buffer Pool must stamp the `PageLSN` and CRC32 inside the buffer before calling this.
    async fn write_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> (AlignedBuf, Result<(), StorageError>);

    /// Writes a contiguous range of 8KB pages to disk from multiple buffers.
    /// Highly optimized for Bulk Loads (`COPY FROM`) and Index Creation.
    /// The pages must be physically sequential on disk starting from `start_page_id`.
    async fn write_pages(
        &self,
        start_page_id: PageId,
        bufs: Vec<AlignedBuf>,
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>);

    /// Pre-allocates a chunk of disk space to prevent file fragmentation.
    /// Returns the starting `page_no` of the newly allocated extent.
    async fn allocate_extent(
        &self,
        db_id: u32,
        space_id: u32,
        num_pages: u32,
    ) -> Result<u32, StorageError>;

    /// Reclaims space to the OS (punching a hole or truncating).
    async fn free_extent(
        &self,
        db_id: u32,
        space_id: u32,
        start_page: u32,
        num_pages: u32,
    ) -> Result<(), StorageError>;
}

// -----------------------------------------------------------------------------
// 2. The Thread-Per-Core Initialization Model
// -----------------------------------------------------------------------------

/// Global configuration for the storage engine.
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub wal_dir: PathBuf,
    pub io_uring_entries: u32, // e.g., 1024 or 2048
}

/// The global manager that boots the database, discovers files, and runs crash recovery.
pub struct StorageManager {
    pub config: StorageConfig,
}

impl StorageManager {
    pub fn mount(config: StorageConfig) -> Result<Self, StorageError> {
        // ... scans directories, maps db_id to physical paths ...
        let _ = config;
        todo!()
    }

    /// Spawns a dedicated, lock-free io_uring storage instance for a specific CPU core.
    /// Note: The returned `CoreStorage` is strictly `!Send` and `!Sync`.
    pub fn local_worker(&self, _core_id: usize) -> crate::core_storage::CoreStorage {
        todo!()
    }
}

// (Re-exported `Lsn` is used by the WAL crate; kept available here for callers
// that touch both subsystems.)
#[allow(dead_code)]
fn _lsn_re_export_anchor(_lsn: Lsn) {}
