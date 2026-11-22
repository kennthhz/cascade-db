//! `LogSource`: anyone who reads the log (§4.4) — the page store's ingest,
//! followers catching up, CDC taps, the archive. Only durable records are
//! ever served (§4.4 promise 1), in LSN order with no gaps.

use cascade_types::{Hlc, Lsn, LocalBoxFuture, ScopeId};

use crate::error::StoreError;
use crate::types::RetentionHolder;
use crate::OpCtx;

/// The log store's read face (§4.4).
pub trait LogSource: Send + Sync {
    /// Returns durable records starting at `from`, up to about
    /// `max_bytes`. Always returns at least one whole record if any is
    /// durable at `from`, even if it is larger than `max_bytes`. Returns
    /// an empty chunk if `from` equals the durable LSN.
    fn read_log<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        from: Lsn,
        max_bytes: usize,
    ) -> LocalBoxFuture<'a, Result<crate::types::LogChunk, StoreError>>;

    /// An ordered stream of durable chunks starting at `from`. It waits
    /// when it reaches the durable LSN, and ends with an error if the
    /// scope is dropped or `from` falls below the retained floor.
    /// The subscription is `!Send`: it lives on the core that opened it.
    fn subscribe(&self, ctx: &OpCtx<'_>, scope: ScopeId, from: Lsn)
        -> Box<dyn LogSubscription>;

    /// Records that `holder` no longer needs log below `lsn`. A holder's
    /// value only moves forward. A new holder must register at or above
    /// the current floor (§4.4 promise 4).
    fn set_retained_from<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;

    /// A holder exits (its scope detached, its follower removed).
    fn release_holder<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;

    /// The lowest LSN that is still readable.
    fn retained_floor(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// For point-in-time restore: the start of the earliest append whose
    /// highest commit timestamp is above `ts`. Every commit at or below
    /// `ts` ends before the end of that append. The caller scans forward
    /// from the returned LSN with `cascade-wal-format` to find the exact
    /// commit boundary. Fails with `Truncated` if the answer is below the
    /// retained floor, in which case the archive answers instead.
    fn append_before_time<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        ts: Hlc,
    ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>>;
}

/// A subscription's cursor (§4.4): chunks contiguous from `from`, waiting
/// at the durable LSN. Not `Send` (S3) — it lives on its opening core.
pub trait LogSubscription {
    fn next(&mut self) -> LocalBoxFuture<'_, Result<crate::types::LogChunk, StoreError>>;
}
