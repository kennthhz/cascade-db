//! [`MemBlockDevice`] — the crash fake (§4.8): two images per file
//! (synced, unsynced); `crash(seed)` keeps a random subset of the unsynced
//! 4 KiB blocks and discards the rest. A failed sync is permanent (S19):
//! the unsynced blocks are forgotten and every later operation on the
//! file reports failure — reading back proves nothing, exactly like the
//! fsyncgate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cascade_types::{AlignedBuf, LocalBoxFuture};

use crate::error::IoError;
use crate::types::{FileName, FileId};
use crate::{BlockDevice, BlockHandle, OpCtx};

/// 4 KiB — the alignment and crash-cut granularity (§4.8).
const BLOCK: usize = 4096;

#[derive(Default)]
struct FileState {
    /// Block index → bytes, durable across crash.
    synced: HashMap<u64, [u8; BLOCK]>,
    /// Written since the last successful sync — lost at crash, kept at
    /// sync. `None` after a failed sync (S19: contents unknown).
    unsynced: Option<HashMap<u64, [u8; BLOCK]>>,
    /// File length (max of both images).
    len: u64,
    /// Set by a failed sync; every later op on this file fails (S19).
    poisoned: bool,
    exists_synced: bool,
}

#[derive(Default)]
struct DevInner {
    files: HashMap<FileName, FileState>,
    ids: HashMap<FileId, FileName>,
    next_id: u64,
}

/// The in-memory device. `Send + Sync`; each `local()` handle shares the
/// same inner state (a fake has no reactors to keep apart).
#[derive(Clone)]
pub struct MemBlockDevice {
    inner: Arc<Mutex<DevInner>>,
    inject_sync_failure_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for MemBlockDevice {
    fn default() -> Self {
        MemBlockDevice {
            inner: Arc::new(Mutex::new(DevInner::default())),
            inject_sync_failure_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// Deterministic xorshift for crash cut points — replayable from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl MemBlockDevice {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self, f: FileId) -> Result<FileName, IoError> {
        self.inner
            .lock()
            .expect("device lock")
            .ids
            .get(&f)
            .cloned()
            .ok_or_else(|| IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file id")))
    }

    /// Crash at a cut point chosen by `seed`: for every file, keep each
    /// unsynced 4 KiB block with probability one half, discard the rest,
    /// then fold the survivors into the disk image. Synced blocks always
    /// survive (§4.8 crash semantics).
    pub fn crash(&self, seed: u64) {
        let mut inner = self.inner.lock().expect("device lock");
        let mut rng = Rng(seed | 1);
        for st in inner.files.values_mut() {
            if let Some(unsynced) = st.unsynced.take() {
                for (idx, block) in unsynced {
                    if rng.next() & 1 == 0 {
                        st.synced.insert(idx, block);
                    }
                }
                st.exists_synced = true;
            }
        }
    }
}

fn check_aligned(off: u64, len: usize) -> Result<(), IoError> {
    if off % BLOCK as u64 != 0 || len % BLOCK != 0 {
        return Err(IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "offsets and lengths are multiples of 4 KiB",
        )));
    }
    Ok(())
}

impl BlockDevice for MemBlockDevice {
    fn local(&self) -> Box<dyn BlockHandle> {
        Box::new(MemHandle {
            dev: self.clone(),
        })
    }
}

struct MemHandle {
    dev: MemBlockDevice,
}

impl MemHandle {
    /// Reads the current image (synced + unsynced) of `name` into `buf`.
    fn read_file(&self, name: &FileName, off: u64, mut buf: AlignedBuf) -> Result<AlignedBuf, IoError> {
        let inner = self.dev.inner.lock().expect("device lock");
        let st = inner.files.get(name).ok_or_else(|| {
            IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
        })?;
        if st.poisoned {
            return Err(IoError(std::io::Error::other(
                "file failed a sync; contents unknown (S19)",
            )));
        }
        let need = off as usize + buf.len();
        if need > st.len as usize {
            return Err(IoError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read past end of file",
            )));
        }
        let out = buf.as_mut_slice();
        let mut done = 0;
        while done < out.len() {
            let idx = ((off as usize + done) / BLOCK) as u64;
            let in_block = (off as usize + done) % BLOCK;
            let take = (BLOCK - in_block).min(out.len() - done);
            let block = st
                .unsynced
                .as_ref()
                .and_then(|u| u.get(&idx))
                .or_else(|| st.synced.get(&idx));
            match block {
                Some(b) => out[done..done + take].copy_from_slice(&b[in_block..in_block + take]),
                None => out[done..done + take].fill(0),
            }
            done += take;
        }
        Ok(buf)
    }

    fn write_file(&self, name: &FileName, off: u64, buf: &AlignedBuf) -> Result<(), IoError> {
        let mut inner = self.dev.inner.lock().expect("device lock");
        // Snapshot the synced blocks this write touches, so a partial-block
        // write starts from the current image rather than zeroes.
        let mut seeded: HashMap<u64, [u8; BLOCK]> = HashMap::new();
        {
            let st = inner.files.get(name).ok_or_else(|| {
                IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
            })?;
            if st.poisoned {
                return Err(IoError(std::io::Error::other(
                    "file failed a sync; refuse further ops (S19)",
                )));
            }
            let first = off / BLOCK as u64;
            let last = (off + buf.len() as u64 - 1) / BLOCK as u64;
            for idx in first..=last {
                if let Some(b) = st.synced.get(&idx) {
                    seeded.insert(idx, *b);
                }
            }
        }
        let st = inner.files.get_mut(name).ok_or_else(|| {
            IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
        })?;
        let unsynced = st.unsynced.get_or_insert_with(HashMap::new);
        for (idx, block) in seeded {
            unsynced.entry(idx).or_insert(block);
        }
        let src = buf.as_slice();
        let mut done = 0;
        while done < src.len() {
            let idx = ((off as usize + done) / BLOCK) as u64;
            let in_block = (off as usize + done) % BLOCK;
            let take = (BLOCK - in_block).min(src.len() - done);
            unsynced.entry(idx).or_insert([0u8; BLOCK])[in_block..in_block + take]
                .copy_from_slice(&src[done..done + take]);
            done += take;
        }
        st.len = st.len.max(off + src.len() as u64);
        Ok(())
    }
}

impl BlockHandle for MemHandle {
    fn open(
        &self,
        _ctx: &OpCtx<'_>,
        name: &FileName,
        create: bool,
    ) -> LocalBoxFuture<'_, Result<FileId, IoError>> {
        let name = name.clone(); // own the capture: one lifetime, not three
        Box::pin(async move {
            let mut inner = self.dev.inner.lock().expect("device lock");
            if create && !inner.files.contains_key(&name) {
                inner.files.insert(name.clone(), FileState::default());
            }
            if !inner.files.contains_key(&name) {
                return Err(IoError(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such file",
                )));
            }
            let id = FileId(inner.next_id);
            inner.next_id += 1;
            inner.ids.insert(id, name.clone());
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
            let name = self.dev.state(f)?;
            self.read_file(&name, off, buf)
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
            let name = self.dev.state(f)?;
            self.write_file(&name, off, &buf)?;
            Ok(buf)
        })
    }

    fn sync(&self, _ctx: &OpCtx<'_>, f: FileId) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move {
            let name = self.dev.state(f)?;
            let fail = self.dev.take_sync_failure();
            let mut inner = self.dev.inner.lock().expect("device lock");
            let st = inner.files.get_mut(&name).ok_or_else(|| {
                IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
            })?;
            if st.poisoned {
                return Err(IoError(std::io::Error::other(
                    "file failed a sync; refuse further ops (S19)",
                )));
            }
            if fail {
                // S19: a failed sync is permanent. The unsynced contents are
                // unknown — forgotten, never retried — and every later
                // operation on this file reports failure.
                st.unsynced = None;
                st.poisoned = true;
                return Err(IoError(std::io::Error::other(
                    "sync failed; contents since the last successful sync are unknown",
                )));
            }
            if let Some(unsynced) = st.unsynced.take() {
                for (idx, block) in unsynced {
                    st.synced.insert(idx, block);
                }
                st.exists_synced = true;
            }
            Ok(())
        })
    }

    fn len(&self, f: FileId) -> LocalBoxFuture<'_, Result<u64, IoError>> {
        Box::pin(async move {
            let name = self.dev.state(f)?;
            let inner = self.dev.inner.lock().expect("device lock");
            let st = inner.files.get(&name).ok_or_else(|| {
                IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
            })?;
            if st.poisoned {
                return Err(IoError(std::io::Error::other(
                    "file failed a sync",
                )));
            }
            Ok(st.len)
        })
    }

    fn allocate(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        len: u64,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        // The fake materializes the reserved range as zeroes through the
        // internal write path (ctx never enters the boxed capture set).
        Box::pin(async move {
            check_aligned(off, len as usize)?;
            let name = self.dev.state(f)?;
            let zeros = AlignedBuf::zeroed(len as usize);
            self.write_file(&name, off, &zeros)
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
            let name = self.dev.state(f)?;
            let mut inner = self.dev.inner.lock().expect("device lock");
            let st = inner.files.get_mut(&name).ok_or_else(|| {
                IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
            })?;
            let unsynced = st.unsynced.get_or_insert_with(HashMap::new);
            let mut idx = off / BLOCK as u64;
            let end = (off + len) / BLOCK as u64;
            while idx < end {
                unsynced.insert(idx, [0u8; BLOCK]);
                idx += 1;
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
            let mut inner = self.dev.inner.lock().expect("device lock");
            inner
                .files
                .remove(&name)
                .ok_or_else(|| IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file")))?;
            inner.ids.retain(|_, n| *n != name);
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
            let mut inner = self.dev.inner.lock().expect("device lock");
            let st = inner
                .files
                .remove(&from)
                .ok_or_else(|| IoError(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file")))?;
            inner.files.insert(to.clone(), st);
            inner.ids.retain(|_, n| *n != to);
            for id_name in inner.ids.values_mut() {
                if *id_name == from {
                    *id_name = to.clone();
                }
            }
            Ok(())
        })
    }

    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<FileName>, IoError>> {
        Box::pin(async move {
            let inner = self.dev.inner.lock().expect("device lock");
            Ok(inner.files.keys().cloned().collect())
        })
    }

    fn sync_namespace(&self) -> LocalBoxFuture<'_, Result<(), IoError>> {
        // The fake's namespace operations are already durable in memory.
        Box::pin(async { Ok(()) })
    }
}

impl MemBlockDevice {
    /// Arms a one-shot sync failure (S19): the next `sync` on any file
    /// fails, forgets that file's unsynced blocks, and poisons the file.
    pub fn inject_sync_failure(&self) {
        // Stored as a device-wide flag consumed by the next sync.
        self.inject_sync_failure_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the one-shot sync failure is armed; consumed by `sync`.
    pub(crate) fn take_sync_failure(&self) -> bool {
        self.inject_sync_failure_flag
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}
