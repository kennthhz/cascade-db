use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::cell::RefCell;
use tokio_uring::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;

// 8KB Page Size constant
const PAGE_SIZE: u64 = 8192;

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
    async fn get_wal_file(&self, db_id: u32) -> Result<Rc<File>, StorageError> {
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
        buf: AlignedBuf
    ) -> (AlignedBuf, Result<(), StorageError>) {
        let file_res = self.get_data_file(page_id.db_id, page_id.space_id).await;
        let file = match file_res {
            Ok(f) => f,
            Err(e) => return (buf, Err(e)),
        };

        let offset = (page_id.page_no as u64) * PAGE_SIZE;
        
        // tokio-uring takes ownership of `buf` and returns it when the kernel is done
        let (res, returned_buf) = file.read_at(buf, offset).await;
        
        if let Err(e) = res {
            return (returned_buf, Err(StorageError::Io(e)));
        }
        
        // TODO: Validate CRC32 checksum here
        
        (returned_buf, Ok(()))
    }

    async fn write_page(
        &self, 
        page_id: PageId, 
        buf: AlignedBuf
    ) -> (AlignedBuf, Result<(), StorageError>) {
        let file_res = self.get_data_file(page_id.db_id, page_id.space_id).await;
        let file = match file_res {
            Ok(f) => f,
            Err(e) => return (buf, Err(e)),
        };

        let offset = (page_id.page_no as u64) * PAGE_SIZE;
        
        // The kernel DMAs the data straight from `buf` to the NVMe controller
        let (res, returned_buf) = file.write_at(buf, offset).await;
        
        match res {
            Ok(_) => (returned_buf, Ok(())),
            Err(e) => (returned_buf, Err(StorageError::Io(e))),
        }
    }

    async fn read_pages(
        &self, 
        start_page_id: PageId, 
        bufs: Vec<AlignedBuf>
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>) {
        // To do vectored I/O with tokio-uring, we can concurrently submit 
        // multiple read_at calls to the ring. The kernel will batch them.
        // (Implementation omitted for brevity, but relies on looping and `FuturesUnordered`)
        todo!()
    }

    async fn write_pages(
        &self, 
        start_page_id: PageId, 
        bufs: Vec<AlignedBuf>
    ) -> (Vec<AlignedBuf>, Result<(), StorageError>) {
        todo!()
    }

    async fn allocate_extent(&self, db_id: u32, space_id: u32, num_pages: u32) -> Result<u32, StorageError> {
        let file = self.get_data_file(db_id, space_id).await?;
        let bytes_to_allocate = (num_pages as u64) * PAGE_SIZE;
        
        // Note: tokio-uring provides `fallocate` to reserve disk blocks at the OS level
        // file.fallocate(0, current_size, bytes_to_allocate).await?;
        todo!()
    }

    async fn free_extent(&self, db_id: u32, space_id: u32, start_page: u32, num_pages: u32) -> Result<(), StorageError> {
        // Uses `fallocate` with FALLOC_FL_PUNCH_HOLE
        todo!()
    }
}

// -----------------------------------------------------------------------------
// Sequential I/O Implementation (Write-Ahead Log)
// -----------------------------------------------------------------------------
impl WalStore for CoreStorage {
    async fn append_wal(&self, db_id: u32, payload: &[u8]) -> Result<Lsn, StorageError> {
        let mut offsets = self.wal_offsets.borrow_mut();
        let current_lsn = offsets.entry(db_id).or_insert(0);
        
        let start_offset = *current_lsn;
        
        // In a real implementation, you would copy `payload` into an AlignedBuf 
        // to submit via io_uring, or use standard AsyncRead/Write if not O_DIRECT.
        
        *current_lsn += payload.len() as u64;
        
        Ok(Lsn(start_offset))
    }

    async fn flush_wal(&self, db_id: u32) -> Result<(), StorageError> {
        let file = self.get_wal_file(db_id).await?;
        
        // io_uring's fdatasync equivalent. This is what you call on COMMIT.
        file.sync_data().await.map_err(StorageError::Io)?;
        Ok(())
    }

    async fn truncate_wal(&self, db_id: u32, up_to_lsn: Lsn) -> Result<(), StorageError> {
        // Unlink old segment files.
        todo!()
    }
}