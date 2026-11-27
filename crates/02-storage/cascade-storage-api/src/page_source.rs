//! `PageSource`: compute reads pages (§4.5). A page comes back exactly as
//! of an LSN — every record for it below `at_lsn` applied, none at or
//! above (§4.5 promise 1, S17's end-LSN rule). Commits never wait on this
//! contract; only a cache miss pays it.

use cascade_types::{Lsn, LocalBoxFuture, ScopeId};

use crate::error::StoreError;
use crate::types::{PageImage, PageKey, ReaderId};
use crate::OpCtx;

/// The page store's read face (§4.5).
pub trait PageSource: Send + Sync {
    /// The page exactly as of `at_lsn`: every record for the page with an
    /// LSN below `at_lsn` is applied, and none at or above it. The
    /// returned image's page LSN is at or below `at_lsn` (§4.5 promise 1).
    fn get_page<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        page: PageKey,
        at_lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<PageImage, StoreError>>;

    /// Every page on disk reflects the log up to here.
    fn materialized_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// The oldest `at_lsn` this page store can still serve for the scope:
    /// any request at or above it succeeds (§4.5 promise 5). Without page
    /// versions (v1) only the newest image of each page is held, so this
    /// is the LSN through which those images are complete, in memory or on
    /// disk — at or above [`materialized_lsn`](Self::materialized_lsn),
    /// since a page changed since the last flush is held only in memory.
    /// A request below it is served only if the image's page LSN is at or
    /// below `at_lsn`, and fails with `TooOld` otherwise; a leader, whose
    /// requests move forward with its own writes, never sees `TooOld`.
    fn oldest_readable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError>;

    /// A reader promises it will not ask for pages below `lsn`. The page
    /// store may discard page versions below the minimum over all readers
    /// (§4.5; page versions arrive with the managed service).
    fn set_read_floor<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        reader: ReaderId,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;
}
