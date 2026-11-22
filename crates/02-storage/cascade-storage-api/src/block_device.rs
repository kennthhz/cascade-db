//! `BlockDevice`: the storage components' only access to disk (§4.8).
//! The real implementation is glommio `O_DIRECT` (`cascade-blockdev`);
//! the fake (`MemBlockDevice`, feature `fake`) models crash semantics at
//! random cut points. No storage component opens files or spawns threads
//! itself (Rule 4) — everything goes through this contract.

use cascade_types::{AlignedBuf, LocalBoxFuture};

use crate::error::IoError;
use crate::types::{FileName, FileId};
use crate::OpCtx;

/// The device (§4.8): shared by all cores; `local()` hands out the calling
/// core's own handle, because file handles never cross reactors.
pub trait BlockDevice: Send + Sync {
    /// A handle for the calling core. File handles never cross cores.
    fn local(&self) -> Box<dyn BlockHandle>;
}

/// One core's view of the device (§4.8). `!Send` by construction — the
/// trait's methods return `LocalBoxFuture`, so a handle cannot escape the
/// reactor that opened it (I2).
pub trait BlockHandle {
    fn open(
        &self,
        ctx: &OpCtx<'_>,
        name: &FileName,
        create: bool,
    ) -> LocalBoxFuture<'_, Result<FileId, IoError>>;

    /// Reads `buf.len()` bytes at `off` into and returning the aligned
    /// buffer; offsets and lengths are multiples of 4 KiB and buffers are
    /// `AlignedBuf` (§4.8 alignment rule).
    fn read_at(
        &self,
        ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        buf: AlignedBuf,
    ) -> LocalBoxFuture<'_, Result<AlignedBuf, IoError>>;

    fn write_at(
        &self,
        ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        buf: AlignedBuf,
    ) -> LocalBoxFuture<'_, Result<AlignedBuf, IoError>>;

    /// Data durability (§4.8 crash semantics). A failed sync is permanent
    /// (S19): everything written since the last successful sync is
    /// unknown, later syncs may lie, and the implementation reports every
    /// later operation on the file as failed.
    fn sync(&self, ctx: &OpCtx<'_>, f: FileId) -> LocalBoxFuture<'_, Result<(), IoError>>;

    fn len(&self, f: FileId) -> LocalBoxFuture<'_, Result<u64, IoError>>;

    /// Preallocates space (segments arrive preallocated, §5.2).
    fn allocate(
        &self,
        ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        len: u64,
    ) -> LocalBoxFuture<'_, Result<(), IoError>>;

    fn punch_hole(
        &self,
        ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        len: u64,
    ) -> LocalBoxFuture<'_, Result<(), IoError>>;

    fn remove(&self, ctx: &OpCtx<'_>, name: &FileName)
        -> LocalBoxFuture<'_, Result<(), IoError>>;

    /// Atomic rename (§4.8): manifests replace files through write → sync
    /// → rename → `sync_namespace`.
    fn rename(
        &self,
        ctx: &OpCtx<'_>,
        from: &FileName,
        to: &FileName,
    ) -> LocalBoxFuture<'_, Result<(), IoError>>;

    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<FileName>, IoError>>;

    /// Makes creates, removes and renames durable (§4.8 crash semantics).
    fn sync_namespace(&self) -> LocalBoxFuture<'_, Result<(), IoError>>;
}
