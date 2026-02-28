use std::io::Result;
use std::path::PathBuf;

/// Represents a 4096-byte aligned memory buffer required for O_DIRECT.
/// Backed by the pre-allocated Buffer Pool RAM.
pub struct AlignedBuf {
    // Internal pointer to aligned memory
}

/// Uniquely identifies an 8KB physical page across the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    pub db_id: u32,
    pub space_id: u32, // Table, Index, or Undo Segment
    pub page_no: u32,  // 8KB logical offset
}

/// A physical byte offset in the Write-Ahead Log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Lsn(pub u64);

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
        buf: AlignedBuf
    ) -> (AlignedBuf, Result<(), StorageError>);

    /// Reads a contiguous range of 8KB pages from disk into multiple buffers.
    /// Highly optimized for Sequential Scans and Prefetching via io_uring vectored I/O.
    /// The `bufs` length determines how many sequential pages are read starting at `start_page_id`.
    async fn read_pages(
        &self, 
        start_page_id: PageId, 
        bufs: Vec<AlignedBuf>
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>);

    /// Writes an 8KB page via O_DIRECT.
    /// The Buffer Pool must stamp the `PageLSN` and CRC32 inside the buffer before calling this.
    async fn write_page(
        &self, 
        page_id: PageId, 
        buf: AlignedBuf
    ) -> (AlignedBuf, Result<(), StorageError>);

    /// Writes a contiguous range of 8KB pages to disk from multiple buffers.
    /// Highly optimized for Bulk Loads (`COPY FROM`) and Index Creation.
    /// The pages must be physically sequential on disk starting from `start_page_id`.
    async fn write_pages(
        &self, 
        start_page_id: PageId, 
        bufs: Vec<AlignedBuf>
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>);

    /// Pre-allocates a chunk of disk space to prevent file fragmentation.
    /// Returns the starting `page_no` of the newly allocated extent.
    async fn allocate_extent(
        &self, 
        db_id: u32, 
        space_id: u32, 
        num_pages: u32
    ) -> Result<u32, StorageError>;
    
    /// Reclaims space to the OS (punching a hole or truncating).
    async fn free_extent(
        &self, 
        db_id: u32, 
        space_id: u32, 
        start_page: u32, 
        num_pages: u32
    ) -> Result<(), StorageError>;
}

// -----------------------------------------------------------------------------
// 2. The Sequential I/O Interface (Used by the Transaction Manager)
// -----------------------------------------------------------------------------
pub trait WalStore {
    /// Appends a binary WAL record to the end of the log.
    /// Returns the exact byte offset (LSN) where this record was written.
    async fn append_wal(
        &self, 
        db_id: u32, 
        payload: &[u8]
    ) -> Result<Lsn, StorageError>;

    /// Issues an `io_uring` flush for the WAL file up to the current tail.
    /// Call this when the user types `COMMIT`.
    async fn flush_wal(&self, db_id: u32) -> Result<(), StorageError>;

    /// Deletes or recycles physical WAL segment files older than the given LSN.
    /// Called by the Checkpointer after 8KB data pages are safely on disk.
    async fn truncate_wal(&self, db_id: u32, up_to_lsn: Lsn) -> Result<(), StorageError>;
}

// -----------------------------------------------------------------------------
// 3. The Thread-Per-Core Initialization Model
// -----------------------------------------------------------------------------

/// Global configuration for the storage engine.
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub wal_dir: PathBuf,
    pub io_uring_entries: u32, // e.g., 1024 or 2048
}

/// The global manager that boots the database, discovers files, and runs crash recovery.
pub struct StorageManager {
    config: StorageConfig,
}

impl StorageManager {
    pub fn mount(config: StorageConfig) -> Result<Self, StorageError> {
        // ... scans directories, maps db_id to physical paths ...
        todo!()
    }

    /// Spawns a dedicated, lock-free io_uring storage instance for a specific CPU core.
    /// Note: The returned `CoreStorage` is strictly `!Send` and `!Sync`.
    pub fn local_worker(&self, core_id: usize) -> CoreStorage {
        todo!()
    }
}

/// The actual engine running on a single thread. It holds the `tokio-uring` ring
/// and an array of open File Descriptors.
pub struct CoreStorage {
    core_id: usize,
    // active_files: HashMap<(u32, u32), std::os::fd::RawFd>,
}

impl PageStore for CoreStorage {
    async fn read_page(&self, page_id: PageId, buf: AlignedBuf) -> (AlignedBuf, Result<(), StorageError>) { todo!() }
    async fn read_pages(&self, start_page_id: PageId, bufs: Vec<AlignedBuf>) -> (Vec<AlignedBuf>, Result<(), StorageError>) { todo!() }
    async fn write_page(&self, page_id: PageId, buf: AlignedBuf) -> (AlignedBuf, Result<(), StorageError>) { todo!() }
    async fn write_pages(&self, start_page_id: PageId, bufs: Vec<AlignedBuf>) -> (Vec<AlignedBuf>, Result<(), StorageError>) { todo!() }
    async fn allocate_extent(&self, db_id: u32, space_id: u32, num_pages: u32) -> Result<u32, StorageError> { todo!() }
    async fn free_extent(&self, db_id: u32, space_id: u32, start_page: u32, num_pages: u32) -> Result<(), StorageError> { todo!() }
}

impl WalStore for CoreStorage {
    async fn append_wal(&self, db_id: u32, payload: &[u8]) -> Result<Lsn, StorageError> { todo!() }
    async fn flush_wal(&self, db_id: u32) -> Result<(), StorageError> { todo!() }
    async fn truncate_wal(&self, db_id: u32, up_to_lsn: Lsn) -> Result<(), StorageError> { todo!() }
}

