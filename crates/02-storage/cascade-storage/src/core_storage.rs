//! `CoreStorage` — the per-CPU-core, `!Send`, `!Sync` storage engine.
//!
//! Originally drafted as `storage/src/core_storage.rs` during Phase 1; moved
//! here when the workspace was restructured and now implemented on glommio
//! (the runtime executor, plan R3) — `DmaFile` gives `O_DIRECT` reads and
//! writes on the calling reactor's own ring (§3.3: I/O is always
//! thread-local; no handle is ever shared or shipped across reactors, I2).
//!
//! File-per-table: `<data_dir>/db_<db>/space_<segment>.dat`, one 8 KiB page
//! per `page_no` (format in [`crate::traits`]). Handles are cached per
//! `(db, segment)` and never leave this struct.

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use cascade_types::{AlignedBuf, PageId, PAGE_SIZE};
use glommio::io::{DmaFile, OpenOptions};
use static_assertions::assert_not_impl_any;

use crate::traits::{PageStore, StorageError};

// Invariant 6: owned by exactly one worker thread; sharing it across cores
// defeats §3.2. Compile-enforced.
assert_not_impl_any!(CoreStorage: Send, Sync);

/// Reserved leading bytes of every on-disk page: the CRC32 (little-endian)
/// of the remaining `PAGE_SIZE - 4` bytes.
const CRC_LEN: usize = 4;

pub struct CoreStorage {
    #[allow(dead_code)] // diagnostics only until per-core metrics land
    core_id: usize,
    base_data_dir: PathBuf,

    // Lock-free cache of open file handles. Rc is safe here because
    // CoreStorage is !Send (thread-local).
    data_files: RefCell<HashMap<(u32, u32), Rc<DmaFile>>>,
}

impl CoreStorage {
    pub fn new(core_id: usize, base_data_dir: PathBuf) -> Self {
        CoreStorage {
            core_id,
            base_data_dir,
            data_files: RefCell::new(HashMap::new()),
        }
    }

    /// Get or open the table file with `O_DIRECT` (glommio `DmaFile`). The
    /// cache borrow is dropped before any await — sibling tasks on this
    /// reactor run at await points.
    async fn get_data_file(&self, db_id: u32, space_id: u32) -> Result<Rc<DmaFile>, StorageError> {
        if let Some(file) = self.data_files.borrow().get(&(db_id, space_id)) {
            return Ok(Rc::clone(file));
        }
        let dir = self.base_data_dir.join(format!("db_{db_id}"));
        std::fs::create_dir_all(&dir).map_err(StorageError::Io)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .dma_open(dir.join(format!("space_{space_id}.dat")))
            .await
            .map_err(|e| StorageError::Io(e.into()))?;
        let rc = Rc::new(file);
        self.data_files
            .borrow_mut()
            .insert((db_id, space_id), Rc::clone(&rc));
        Ok(rc)
    }

    fn offset_of(page_id: PageId) -> u64 {
        page_id.page_no as u64 * PAGE_SIZE as u64
    }
}

// -----------------------------------------------------------------------------
// Random I/O Implementation (Data Pages)
// -----------------------------------------------------------------------------
impl PageStore for CoreStorage {
    async fn read_page(&self, page_id: PageId) -> Result<AlignedBuf, StorageError> {
        let file = self.get_data_file(page_id.db.0, page_id.segment.0).await?;
        let offset = Self::offset_of(page_id);
        if file
            .file_size()
            .await
            .map_err(|e| StorageError::Io(e.into()))?
            <= offset
        {
            // Fresh page: the file has never carried it, so its contents are
            // zeroes by definition.
            return Ok(AlignedBuf::zeroed(PAGE_SIZE));
        }
        let rr = file
            .read_at(offset, PAGE_SIZE)
            .await
            .map_err(|e| StorageError::Io(e.into()))?;
        if rr.len() < PAGE_SIZE {
            return Err(StorageError::ShortRead);
        }
        let stored_crc = u32::from_le_bytes(rr[..CRC_LEN].try_into().expect("CRC_LEN bytes"));
        let payload = &rr[CRC_LEN..];
        if crc32fast::hash(payload) != stored_crc {
            return Err(StorageError::Corruption(page_id));
        }
        let mut buf = AlignedBuf::zeroed(PAGE_SIZE);
        buf.as_mut_slice()[CRC_LEN..].copy_from_slice(payload);
        Ok(buf)
    }

    async fn write_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> Result<AlignedBuf, StorageError> {
        if buf.len() != PAGE_SIZE {
            return Err(StorageError::UnalignedBuffer);
        }
        let mut dma = glommio::allocate_dma_buffer_global(PAGE_SIZE);
        dma.as_bytes_mut()[..CRC_LEN]
            .copy_from_slice(&crc32fast::hash(&buf.as_slice()[CRC_LEN..]).to_le_bytes());
        dma.as_bytes_mut()[CRC_LEN..].copy_from_slice(&buf.as_slice()[CRC_LEN..]);

        let file = self.get_data_file(page_id.db.0, page_id.segment.0).await?;
        let written = file
            .write_at(dma, Self::offset_of(page_id))
            .await
            .map_err(|e| StorageError::Io(e.into()))?;
        if written != PAGE_SIZE {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("short page write: {written} of {PAGE_SIZE} bytes"),
            )));
        }
        Ok(buf)
    }

    async fn read_pages(
        &self,
        _start_page_id: PageId,
        _count: u32,
    ) -> Result<Vec<AlignedBuf>, StorageError> {
        todo!("vectored reads land with the scan/prefetch milestone (M3+)")
    }

    async fn write_pages(
        &self,
        _start_page_id: PageId,
        _bufs: Vec<AlignedBuf>,
    ) -> Result<Vec<AlignedBuf>, StorageError> {
        todo!("vectored writes land with the bulk-load milestone (M3+)")
    }

    async fn allocate_extent(
        &self,
        _db_id: u32,
        _space_id: u32,
        _num_pages: u32,
    ) -> Result<u32, StorageError> {
        todo!("extent allocation lands with the space-management milestone (M3+)")
    }

    async fn free_extent(
        &self,
        _db_id: u32,
        _space_id: u32,
        _start_page: u32,
        _num_pages: u32,
    ) -> Result<(), StorageError> {
        todo!("extent reclamation lands with the space-management milestone (M3+)")
    }
}
