//! Admin contracts: creating and attaching scopes (§4.6). Creating a scope
//! is never a side effect of writing to it — that would hide a lifecycle
//! decision inside the data path. `cascade-scope` and the local controller
//! (single server) or `cascade-cluster` (managed service) call these.

use cascade_types::{Epoch, LocalBoxFuture, ScopeId};

use crate::error::StoreError;
use crate::key_provider::KeyRef;
use crate::types::Lineage;
use crate::OpCtx;

/// Scope lifecycle on the log store (§4.6).
pub trait LogScopes: Send + Sync {
    /// Creates an empty log for `scope`. Its durable LSN starts at 0, and
    /// `open_writer` with `initial` or higher may follow.
    fn create_scope<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        initial: Epoch,
        key: KeyRef,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;

    /// Fences every epoch and frees the scope's log once no holder
    /// remains. The scope's encryption key is scheduled for destruction at
    /// the end of the recovery window (S21, crypto-shredding).
    fn drop_scope<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;
}

/// Scope lifecycle on the page store (§4.6).
pub trait PageScopes: Send + Sync {
    /// Starts materializing `scope` from a log source. With a lineage,
    /// the scope is a branch: a page it has not written is read from the
    /// parent as of the fork LSN. Lineage attach returns `Unsupported`
    /// in v1 (§4.6) — the hook is the branch point (§4.12).
    fn attach<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        lineage: Option<Lineage>,
        key: KeyRef,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;

    fn detach<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;
}
