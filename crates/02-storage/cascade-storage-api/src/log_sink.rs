//! `LogSink`: compute writes the log (§4.3). Implemented by the local log
//! store (single server) or by the quorum client (managed service);
//! callers hold `Arc<dyn LogSink>` — never generic over the choice (S2).
//!
//! Compute never calls this directly: the L3 `Log` contract (the log
//! writer) is `LogSink`'s only caller, so switching editions changes
//! nothing above L3.

use cascade_types::{Epoch, Lsn, LocalBoxFuture, ScopeId};

use crate::error::StoreError;
use crate::types::AppendMeta;
use crate::OpCtx;

/// The log store's write face (§4.3). Bytes in, epochs enforced, bytes
/// durable — the store understands framing through `cascade-wal-format`
/// and never a record payload (S5).
pub trait LogSink: Send + Sync {
    /// Takes ownership of `scope` at `epoch`. Fences every older epoch, so
    /// their appends fail from now on. Durably records the new epoch before
    /// returning, and returns the durable LSN, which is where the new
    /// writer's first append must start (§4.3 promise 1; the fence is
    /// serialized with in-flight flushes — an old-epoch frame can never
    /// become durable at the new writer's LSNs).
    fn open_writer<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        epoch: Epoch,
    ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>>;

    /// Appends whole records starting at `start`. Returns once the bytes
    /// are *accepted* (epoch, contiguity, framing, idempotence all
    /// checked; bytes copied into the pending queue), not once they are
    /// durable. The last record must carry the group-end flag (S16).
    fn append<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        epoch: Epoch,
        start: Lsn,
        bytes: &'a [u8],
        meta: AppendMeta,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;

    /// The scope's durable LSN: every byte below it is durable, and the
    /// boundary always falls on a mini-transaction end (§4.3 promise 5,
    /// S16). Never blocks.
    fn durable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// Resolves once the durable LSN is at or above `lsn`. Waiting is what
    /// triggers a flush — this is where group commit happens (§5.4).
    fn wait_durable<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>>;
}
