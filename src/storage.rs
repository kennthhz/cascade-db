use std::alloc::{alloc, dealloc, Layout};
use std::future::Future;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crc32fast::Hasher;
use tokio_uring::fs::File;

pub const PAGE_SIZE: usize = 8192;
pub const PAGES_PER_SEGMENT: u32 = 131_072;
pub const SEGMENT_SIZE_BYTES: u64 = 1_073_741_824; // 1 GiB

// ---- Compatibility shim ----------------------------------------------------
// tokio-uring 0.5.0: write_at() -> UnsubmittedWrite<T> (has inherent submit())  :contentReference[oaicite:2]{index=2}
// Some other versions / forks: write_at() -> impl Future<Output=BufResult<..>> (no submit)
// This trait makes `.submit()` a no-op for Futures, so `.submit().await` works either way.
trait SubmitCompat: Future + Sized {
    fn submit(self) -> Self {
        self
    }
}

impl<F: Future> SubmitCompat for F {}
// ---------------------------------------------------------------------------

pub struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
    init: usize, // number of initialized bytes
}

unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Allocate *uninitialized* memory aligned to 4096 (often required for O_DIRECT).
    pub fn new(size: usize) -> Self {
        let layout = Layout::from_size_align(size, 4096).expect("Layout failed");
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            panic!("Allocation failed");
        }
        Self { ptr, layout, init: 0 }
    }

    pub fn len(&self) -> usize {
        self.layout.size()
    }

    /// Initialized prefix only (safe to read).
    pub fn as_init_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.init) }
    }

    /// Full capacity (mutable). Use when writing/initializing bytes.
    pub fn as_mut_total_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }

    pub fn set_init_len(&mut self, n: usize) {
        self.init = n.min(self.layout.size());
    }

    /// Ensure [0..n) is initialized by zero-filling any not-yet-initialized tail.
    /// Fixes UB risk when hashing bytes that were never written.
    pub fn ensure_init_up_to(&mut self, n: usize) {
        let target = n.min(self.layout.size());
        let init = self.init; // <-- capture before borrowing self mutably (fixes E0503)
        if init < target {
            let s = self.as_mut_total_slice();
            s[init..target].fill(0);
            self.init = target;
        }
    }
}

unsafe impl tokio_uring::buf::IoBuf for AlignedBuf {
    fn stable_ptr(&self) -> *const u8 {
        self.ptr
    }
    fn bytes_init(&self) -> usize {
        self.init
    }
    fn bytes_total(&self) -> usize {
        self.layout.size()
    }
}

unsafe impl tokio_uring::buf::IoBufMut for AlignedBuf {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    unsafe fn set_init(&mut self, pos: usize) {
        if pos > self.init {
            self.init = pos.min(self.layout.size());
        }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    pub space_id: u32,
    pub page_no: u32,
}

pub struct UringStorage {
    base_path: PathBuf,
}

impl UringStorage {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            base_path: path.as_ref().to_path_buf(),
        }
    }

    fn get_location(&self, page_id: PageId) -> (PathBuf, u64) {
        let segment_no = page_id.page_no / PAGES_PER_SEGMENT;
        let local_page_no = page_id.page_no % PAGES_PER_SEGMENT;
        let filename = format!("space_{}_seg_{:04}.db", page_id.space_id, segment_no);
        let offset = local_page_no as u64 * PAGE_SIZE as u64;
        (self.base_path.join(filename), offset)
    }

    async fn open_segment(&self, path: &PathBuf) -> std::io::Result<File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);

        // NOTE: O_DIRECT has strict alignment rules (buf addr, len, offset).
        // O_SYNC is expensive; keep only if you truly need it.
        options.custom_flags(libc::O_DIRECT | libc::O_SYNC);

        let std_file = options.open(path)?;
        let fd = std_file.as_raw_fd();

        // Pre-allocate the segment file. (This is a blocking syscall.)
        unsafe {
            let res = libc::fallocate(fd, 0, 0, SEGMENT_SIZE_BYTES as libc::off_t);
            if res != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        Ok(File::from_std(std_file))
    }

    pub async fn write_page(
        &self,
        page_id: PageId,
        mut buf: AlignedBuf,
    ) -> (std::io::Result<()>, AlignedBuf) {
        // Ensure deterministic initialized content for the whole page before hashing/writing.
        buf.ensure_init_up_to(PAGE_SIZE);

        // CRC over bytes [4..PAGE_SIZE], store big-endian in [0..4]
        let mut hasher = Hasher::new();
        hasher.update(&buf.as_init_slice()[4..PAGE_SIZE]);
        let checksum = hasher.finalize();
        buf.as_mut_total_slice()[0..4].copy_from_slice(&checksum.to_be_bytes());

        let (path, offset) = self.get_location(page_id);
        let file = match self.open_segment(&path).await {
            Ok(f) => f,
            Err(e) => return (Err(e), buf),
        };

        // Works for tokio-uring 0.5.0 (UnsubmittedWrite::submit().await) :contentReference[oaicite:3]{index=3}
        // Also works when write_at already returns a Future (SubmitCompat makes submit() a no-op).
        let (res, buf) = file.write_at(buf, offset).submit().await;

        match res {
            Ok(n) if n == PAGE_SIZE => (Ok(()), buf),
            Ok(n) => (
                Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!("short write: {n} bytes (expected {PAGE_SIZE})"),
                )),
                buf,
            ),
            Err(e) => (Err(e), buf),
        }
    }

    pub async fn read_page(
        &self,
        page_id: PageId,
        buf: AlignedBuf,
    ) -> (std::io::Result<()>, AlignedBuf) {
        let (path, offset) = self.get_location(page_id);
        let file = match self.open_segment(&path).await {
            Ok(f) => f,
            Err(e) => return (Err(e), buf),
        };

        // In 0.5.0, read_at is `async fn ... -> BufResult` (awaitable). :contentReference[oaicite:4]{index=4}
        // Some docs/examples use submit-based ops elsewhere; this `.submit().await` is compatible
        // because SubmitCompat makes submit() a no-op for Futures.
        let (res, mut buf) = file.read_at(buf, offset).submit().await;

        match res {
            Ok(n) => {
                buf.set_init_len(n);

                if n != PAGE_SIZE {
                    return (
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!("short read: {n} bytes (expected {PAGE_SIZE})"),
                        )),
                        buf,
                    );
                }

                let s = buf.as_init_slice();
                let stored =
                    u32::from_be_bytes(s[0..4].try_into().expect("checksum bytes missing"));

                let mut hasher = Hasher::new();
                hasher.update(&s[4..PAGE_SIZE]);
                let computed = hasher.finalize();

                if stored != computed {
                    return (
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Checksum mismatch",
                        )),
                        buf,
                    );
                }

                (Ok(()), buf)
            }
            Err(e) => (Err(e), buf),
        }
    }
}

