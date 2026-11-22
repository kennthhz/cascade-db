//! The glommio `BlockDevice` (storage-layer-design.md §4.8, step M3):
//! `DmaFile` O_DIRECT reads and writes on the calling reactor's own
//! ring — the I/O core of `CoreStorage`, wrapped in `WalHandle`'s
//! watchdog. (WSL2 + glommio 0.9 drops a CQE roughly once per few
//! thousand ops; a bounded wait plus re-issue turns the hang into a
//! retry, and both reads and idempotent pwrite/fdatasync survive a
//! superseded late completion.)
//!
//! Promises (§4.8): offsets and lengths are multiples of 4 KiB and buffers
//! are `AlignedBuf`; a write followed by a successful `sync` survives a
//! crash, an unsynced one may not; **a failed sync is permanent (S19)** —
//! the file is poisoned, every later operation reports failure, and
//! retrying the sync proves nothing; `rename` is atomic and durable once
//! `sync_namespace` returns; names are flat within the device's root —
//! no caller ever sees a path.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use cascade_storage_api::{BlockDevice, BlockHandle, FileName, FileId, IoError, OpCtx};
use cascade_types::{AlignedBuf, LocalBoxFuture};
use glommio::io::DmaFile;

/// Watchdog bound for one ring op — `WalHandle`'s bound, for the same
/// reason (the doc comment there holds for DMA I/O too).
const OP_WATCHDOG: Duration = Duration::from_millis(250);
/// Give up after this many firings: an honest error, not a stall.
const OP_ATTEMPTS: usize = 3;

const BLOCK: u64 = 4096;

fn check_aligned(off: u64, len: usize) -> Result<(), IoError> {
    if off % BLOCK != 0 || len as u64 % BLOCK != 0 {
        return Err(IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "offsets and lengths are multiples of 4 KiB (§4.8)",
        )));
    }
    Ok(())
}

fn io(e: impl std::fmt::Display) -> IoError {
    IoError(std::io::Error::other(e.to_string()))
}

/// The device: one root directory namespace, shared by every core
/// (`Send + Sync`); each `local()` handle keeps its own reactor-bound
/// `DmaFile` cache.
pub struct GlommioBlockDevice {
    root: PathBuf,
}

impl GlommioBlockDevice {
    /// A device over `root`. The directory is created lazily on first
    /// `open(create = true)`, like `StorageManager::mount` did.
    pub fn new(root: PathBuf) -> Self {
        GlommioBlockDevice { root }
    }

    fn path_of(&self, name: &FileName) -> PathBuf {
        self.root.join(&name.0)
    }
}

impl BlockDevice for GlommioBlockDevice {
    fn local(&self) -> Box<dyn BlockHandle> {
        Box::new(GlommioHandle {
            dev: self.clone_shallow(),
            files: RefCell::new(HashMap::new()),
            ids: RefCell::new(HashMap::new()),
            poisoned: RefCell::new(HashSet::new()),
            next_id: RefCell::new(0),
        })
    }
}

impl GlommioBlockDevice {
    // The device is copied by handle construction; it is just a root path.
    fn clone_shallow(&self) -> GlommioBlockDevice {
        GlommioBlockDevice {
            root: self.root.clone(),
        }
    }
}

/// One core's handle. `!Send` by construction: `Rc`/`RefCell` state and
/// reactor-bound `DmaFile`s never leave the opening core (§4.8, I2).
struct GlommioHandle {
    dev: GlommioBlockDevice,
    files: RefCell<HashMap<FileName, Rc<DmaFile>>>,
    ids: RefCell<HashMap<FileId, FileName>>,
    poisoned: RefCell<HashSet<FileId>>,
    next_id: RefCell<u64>,
}

impl GlommioHandle {
    /// The aligned write path shared by `write_at` and `allocate` — an
    /// inherent async fn so no boxed capture ever mixes two lifetimes.
    async fn write_file(&self, f: FileId, off: u64, buf: &AlignedBuf) -> Result<(), IoError> {
        check_aligned(off, buf.len())?;
        self.check_poisoned(f)?;
        let (_, file) = self.file(f).await?;
        // DmaFile::write_at consumes the buffer; each watchdog attempt
        // builds a fresh DMA copy (the bytes are the caller's, and a
        // pwrite of the same bytes at an explicit offset is idempotent).
        let file = &file;
        let buf = &buf;
        let written = self
            .guarded("write", || {
                let mut dma = glommio::allocate_dma_buffer(buf.len());
                dma.as_bytes_mut().copy_from_slice(buf.as_slice());
                async move { file.write_at(dma, off).await.map_err(|e| e.to_string()) }
            })
            .await?;
        if written as usize != buf.len() {
            return Err(io(format!("short write: {written} of {} bytes", buf.len())));
        }
        Ok(())
    }

    fn check_poisoned(&self, f: FileId) -> Result<(), IoError> {
        if self.poisoned.borrow().contains(&f) {
            return Err(IoError(std::io::Error::other(
                "file failed a sync; contents since the last successful sync are unknown (S19)",
            )));
        }
        Ok(())
    }

    /// The open `DmaFile` for `f`, opening through the O_DIRECT path on
    /// first touch.
    async fn file(&self, f: FileId) -> Result<(FileName, Rc<DmaFile>), IoError> {
        self.check_poisoned(f)?;
        let name = self
            .ids
            .borrow()
            .get(&f)
            .cloned()
            .ok_or_else(|| io("no such file id"))?;
        if let Some(rc) = self.files.borrow().get(&name) {
            return Ok((name, Rc::clone(rc)));
        }
        let path = self.dev.path_of(&name);
        let file = glommio::io::OpenOptions::new()
            .read(true)
            .write(true)
            .dma_open(path)
            .await
            .map_err(io)?;
        let rc = Rc::new(file);
        self.files.borrow_mut().insert(name.clone(), Rc::clone(&rc));
        Ok((name, rc))
    }

    /// `WalHandle`'s bounded-wait-plus-re-issue around one ring op: a
    /// watchdog firing drops the stalled future and builds a fresh one —
    /// reads and idempotent pwrite/fdatasync survive a superseded late
    /// completion (the io.rs doc comment holds for DMA I/O too). The race
    /// is over `timer::sleep` rather than `timer::timeout`, whose bound
    /// pairs the future's Ok type with its error payload — read's error
    /// payload is `()`, so timeout refuses it; sleep has no such law.
    async fn guarded<T, Fut, F>(&self, op: &'static str, make: F) -> Result<T, IoError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        for attempt in 0..OP_ATTEMPTS {
            let outcome;
            {
                let fut = make();
                let timer = glommio::timer::sleep(OP_WATCHDOG);
                futures_lite::pin!(fut, timer);
                outcome = futures_lite::future::race(
                    async { fut.await.map_err(|e| e.to_string()) },
                    async move { timer.await; Err(format!("{op} stalled past the watchdog")) },
                )
                .await;
            }
            match outcome {
                Ok(v) => return Ok(v),
                Err(msg) => {
                    if attempt + 1 == OP_ATTEMPTS {
                        return Err(io(format!("{msg} ({OP_ATTEMPTS} attempts)")));
                    }
                    // Re-issue: a superseded op that lands late is harmless
                    // by idempotence (io.rs).
                }
            }
        }
        unreachable!("the last attempt returns")
    }
}

impl BlockHandle for GlommioHandle {
    fn open(
        &self,
        _ctx: &OpCtx<'_>,
        name: &FileName,
        create: bool,
    ) -> LocalBoxFuture<'_, Result<FileId, IoError>> {
        let name = name.clone(); // own the capture: one lifetime, not two
        Box::pin(async move {
            if create {
                std::fs::create_dir_all(&self.dev.root).map_err(io)?;
            }
            let path = self.dev.path_of(&name);
            if create && !path.exists() {
                // A fresh O_DIRECT file: dma_open requires the file to
                // exist, so create it through a plain truncate-then-open.
                std::fs::File::create(&path).map_err(io)?;
            } else if !path.exists() {
                return Err(io("no such file"));
            }
            let file = glommio::io::OpenOptions::new()
                .read(true)
                .write(true)
                .dma_open(&path)
                .await
                .map_err(io)?;
            let id = FileId(*self.next_id.borrow());
            *self.next_id.borrow_mut() += 1;
            self.ids.borrow_mut().insert(id, name.clone());
            self.files.borrow_mut().insert(name.clone(), Rc::new(file));
            Ok(id)
        })
    }

    fn read_at(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        buf: AlignedBuf,
    ) -> LocalBoxFuture<'_, Result<AlignedBuf, IoError>> {
        Box::pin(async move {
            check_aligned(off, buf.len())?;
            let (_, file) = self.file(f).await?;
            let len = buf.len();
            let mut buf = buf;
            let file = &file;
            let read = self
                .guarded("read", || async move {
                    file.read_at(off, len).await.map_err(|e| e.to_string())
                })
                .await?;
            let bytes: &[u8] = &read;
            if bytes.len() < len {
                // Strict, like the fake: an unwritten range is the log
                // store's allocate() duty, never a silent zero-fill.
                return Err(io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "short read: the file does not hold the requested range",
                )));
            }
            buf.as_mut_slice().copy_from_slice(bytes);
            Ok(buf)
        })
    }

    fn write_at(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        buf: AlignedBuf,
    ) -> LocalBoxFuture<'_, Result<AlignedBuf, IoError>> {
        Box::pin(async move {
            check_aligned(off, buf.len())?;
            self.write_file(f, off, &buf).await?;
            Ok(buf)
        })
    }

    fn sync(&self, _ctx: &OpCtx<'_>, f: FileId) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move {
            self.check_poisoned(f)?;
            let (_, file) = self.file(f).await?;
            let file = &file;
            match self
                .guarded("sync", || async move {
                    file.fdatasync().await.map_err(|e| e.to_string())
                })
                .await
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    // S19: a failed sync is permanent — contents unknown,
                    // every later op refuses, retrying proves nothing.
                    self.poisoned.borrow_mut().insert(f);
                    Err(e)
                }
            }
        })
    }

    fn len(&self, f: FileId) -> LocalBoxFuture<'_, Result<u64, IoError>> {
        Box::pin(async move {
            self.check_poisoned(f)?;
            let (_, file) = self.file(f).await?;
            file.file_size().await.map_err(io)
        })
    }

    fn allocate(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        len: u64,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        // The reserved range is materialized as zeroes through the aligned
        // write path (segments arrive preallocated, §5.2).
        Box::pin(async move {
            check_aligned(off, len as usize)?;
            let zeros = AlignedBuf::zeroed(len as usize);
            self.write_file(f, off, &zeros).await
        })
    }

    fn punch_hole(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        len: u64,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move {
            check_aligned(off, len as usize)?;
            self.check_poisoned(f)?;
            let (name, _) = self.file(f).await?;
            let path = self.dev.path_of(&name);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(io)?;
            let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
            // FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE — the segment-reuse
            // reclaim (§5.6) rides the same syscall today's recycle rule
            // reserves.
            let rc = unsafe {
                libc::fallocate(
                    fd,
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    off as libc::off_t,
                    len as libc::off_t,
                )
            };
            if rc != 0 {
                return Err(io(std::io::Error::last_os_error()));
            }
            Ok(())
        })
    }

    fn remove(
        &self,
        _ctx: &OpCtx<'_>,
        name: &FileName,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        let name = name.clone(); // own the capture
        Box::pin(async move {
            let path = self.dev.path_of(&name);
            std::fs::remove_file(&path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => io("no such file"),
                _ => io(e),
            })?;
            self.files.borrow_mut().remove(&name);
            self.ids
                .borrow_mut()
                .retain(|_, n| *n != name);
            Ok(())
        })
    }

    fn rename(
        &self,
        _ctx: &OpCtx<'_>,
        from: &FileName,
        to: &FileName,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        let (from, to) = (from.clone(), to.clone()); // own the captures
        Box::pin(async move {
            // Atomic on the same filesystem (§4.8): the manifest replace
            // order rides write → sync → rename → sync_namespace.
            std::fs::rename(self.dev.path_of(&from), self.dev.path_of(&to)).map_err(io)?;
            {
                let mut ids = self.ids.borrow_mut();
                ids.retain(|_, n| *n != to);
                for n in ids.values_mut() {
                    if *n == from {
                        *n = to.clone();
                    }
                }
            }
            let cached = self.files.borrow_mut().remove(&from);
            if let Some(rc) = cached {
                self.files.borrow_mut().insert(to.clone(), rc);
            }
            Ok(())
        })
    }

    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<FileName>, IoError>> {
        Box::pin(async move {
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&self.dev.root).map_err(io)? {
                let entry = entry.map_err(io)?;
                if entry.file_type().map_err(io)?.is_file() {
                    names.push(FileName(entry.file_name().to_string_lossy().into_owned()));
                }
            }
            names.sort();
            Ok(names)
        })
    }

    fn sync_namespace(&self) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move {
            // fsync the directory: makes creates, removes and renames
            // durable (§4.8 crash semantics).
            std::fs::create_dir_all(&self.dev.root).map_err(io)?;
            let dir = std::fs::File::open(&self.dev.root).map_err(io)?;
            dir.sync_all().map_err(io)
        })
    }
}
