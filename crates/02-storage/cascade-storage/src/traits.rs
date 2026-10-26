//! `PageStore` trait, error type, and bootstrap structs.
//!
//! Originally drafted as `storage/src/traits.rs` during Phase 1; moved here
//! when the workspace was restructured. Type definitions (`PageId`, `Lsn`,
//! `AlignedBuf`) live in [`cascade-types`].
//!
//! Page format (owned by this crate, invariant 3): one page is `PAGE_SIZE`
//! bytes on disk — `[4-byte CRC32 of the remainder][payload]`. `read_page`
//! returns the full image; `write_page` stamps the CRC so callers never
//! touch checksums. The 4 reserved leading bytes are the seed of the page
//! header [`cascade-page`] will own.

use std::path::PathBuf;

use cascade_types::{AlignedBuf, Lsn, PageId};

#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    Corruption(PageId), // e.g., CRC32 Checksum failed on read
    UnalignedBuffer,    // Buffer didn't meet O_DIRECT requirements
    OutOfSpace,
    ShortRead, // Hit EOF before filling all requested buffers
}

// -----------------------------------------------------------------------------
// 1. The Random I/O Interface (Used by the Buffer Pool)
// -----------------------------------------------------------------------------
// `async fn` in trait is deliberate: stores are reactor-bound (a store never
// crosses threads), so no Send bounds on the futures — the lint's concern
// does not apply to this trait.
#[allow(async_fn_in_trait)]
pub trait PageStore {
    /// Reads one page, CRC-validated. A page that was never written (the
    /// read starts at or past EOF) comes back zero-filled — fresh files
    /// grow on first write, so "not there yet" means "all zeroes".
    async fn read_page(&self, page_id: PageId) -> Result<AlignedBuf, StorageError>;

    /// Writes one page via `O_DIRECT`, stamping the CRC (see module doc).
    /// Returns the buffer so its owner keeps the allocation — storage never
    /// holds page memory.
    async fn write_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> Result<AlignedBuf, StorageError>;

    /// Reads `count` sequential pages starting at `start_page_id` — the
    /// vectored path for sequential scans and prefetching (invariant 4: the
    /// trait shape exists from day one; implementations land with the
    /// scan/copy milestones).
    async fn read_pages(
        &self,
        start_page_id: PageId,
        count: u32,
    ) -> Result<Vec<AlignedBuf>, StorageError>;

    /// Writes `count` sequential pages starting at `start_page_id` — the
    /// vectored path for bulk loads (`COPY FROM`) and index creation.
    async fn write_pages(
        &self,
        start_page_id: PageId,
        bufs: Vec<AlignedBuf>,
    ) -> Result<Vec<AlignedBuf>, StorageError>;

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
    /// Ensures the storage tree exists. Boot-time, synchronous, runs before
    /// any reactor exists.
    pub fn mount(config: StorageConfig) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&config.data_dir).map_err(StorageError::Io)?;
        std::fs::create_dir_all(&config.wal_dir).map_err(StorageError::Io)?;
        Ok(StorageManager { config })
    }

    /// Spawns a dedicated, lock-free io_uring storage instance for a specific
    /// CPU core. The returned `CoreStorage` is strictly `!Send` and `!Sync` —
    /// own it on exactly one thread; its file handles never escape
    /// `cascade-storage` (I2).
    pub fn local_worker(&self, core_id: usize) -> crate::core_storage::CoreStorage {
        crate::core_storage::CoreStorage::new(core_id, self.config.data_dir.clone())
    }
}

// (Re-exported `Lsn` is used by the WAL crate; kept available here for callers
// that touch both subsystems.)
#[allow(dead_code)]
fn _lsn_re_export_anchor(_lsn: Lsn) {}
