//! The L2 storage contracts (storage-layer-design.md §4) — what the rest
//! of the system may call, the data that crosses each boundary, the
//! errors, and the promises callers may rely on.
//!
//! Data path: [`LogSink`] (compute writes the log, §4.3), [`LogSource`]
//! (anyone reads durable log, §4.4), [`PageSource`] (compute reads pages
//! as of an LSN, §4.5). Admin: [`LogScopes`] and [`PageScopes`] (§4.6).
//! Consumed by the components: [`KeyProvider`] (§4.7) and [`BlockDevice`]
//! (§4.8). Errors in [`error::StoreError`] (§4.9).
//!
//! Shape rules (S2, S3): every contract is an object-safe trait whose
//! async methods return [`LocalBoxFuture`], so callers hold
//! `Arc<dyn LogSink>` and kin without becoming generic over single-server
//! versus managed service. The traits are `Send + Sync` (every core of a
//! scope's core set calls them); the futures, subscriptions and
//! per-core block handles are not — a call made on core 3 is polled to
//! completion on core 3 (thread-per-core is the point, and the type
//! system says so).
//!
//! Behind the `fake` feature: the in-memory fakes of §4.10. Behind
//! `conformance`: one suite per contract, generic over a
//! create/crash/restart harness, run against the fakes here, the local
//! implementations, and later the remote clients.

pub mod block_device;
pub mod error;
pub mod key_provider;
pub mod log_sink;
pub mod log_source;
pub mod page_source;
pub mod scope_admin;
pub mod types;

#[cfg(feature = "fake")]
pub mod fake;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use block_device::{BlockDevice, BlockHandle};
pub use error::{CorruptionSite, FrameError, IoError, KeyError, StoreError};
pub use key_provider::{KeyProvider, KeyRef, KeyVersion, ScopeKey};
pub use log_sink::LogSink;
pub use log_source::{LogSource, LogSubscription};
pub use page_source::{DurableLsn, PageSource};
pub use scope_admin::{LogScopes, PageScopes};
pub use types::{
    AppendMeta, FileId, FileName, HolderReport, Lineage, LogChunk, PageImage, PageKey, ReaderId,
    RetentionHolder,
};

use cascade_types::{DatabaseId, TenantId};

/// Which tenant and database a storage operation acts on (§4.2, M0) —
/// every contract call carries one, and every I/O submission is tagged
/// with it (§4.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TenantContext {
    /// The database the operation touches. In v1 every database is one
    /// scope, which also holds its catalog (§12.2).
    pub database: DatabaseId,
    /// The governing tenant, when one exists.
    pub tenant: Option<TenantId>,
}

impl TenantContext {
    /// The context for a lone database with no governing tenant — the v1
    /// single-server shape.
    pub const fn for_database(database: DatabaseId) -> Self {
        TenantContext {
            database,
            tenant: None,
        }
    }
}

/// The context every contract call takes explicitly (S4 / Rule 8): who
/// the caller acts for, under which tracing span, by when it must finish.
#[derive(Debug, Clone, Copy)]
pub struct OpCtx<'a> {
    pub tenant: TenantContext,
    pub span: &'a cascade_telemetry::Span,
    /// `None` means no caller deadline; components check it at their
    /// natural await points.
    pub deadline: Option<std::time::Instant>,
}

impl<'a> OpCtx<'a> {
    /// A context with no deadline.
    pub fn new(tenant: TenantContext, span: &'a cascade_telemetry::Span) -> Self {
        OpCtx {
            tenant,
            span,
            deadline: None,
        }
    }

    /// Builder: sets the caller deadline (Rule 8).
    pub fn with_deadline(mut self, deadline: std::time::Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }
}

#[cfg(test)]
mod tests {
    use cascade_telemetry::Span;
    use static_assertions::{assert_impl_all, assert_not_impl_any, assert_obj_safe};

    use super::*;

    // S2: the contracts are object-safe; callers hold trait objects.
    assert_obj_safe!(LogSink, LogSource, PageSource, LogScopes, PageScopes, KeyProvider, BlockDevice);

    // S3: the shared contracts are Send + Sync — every core calls them.
    assert_impl_all!(
        std::sync::Arc<dyn LogSink>: Send, Sync
    );
    assert_impl_all!(std::sync::Arc<dyn LogSource>: Send, Sync);
    assert_impl_all!(std::sync::Arc<dyn PageSource>: Send, Sync);
    assert_impl_all!(std::sync::Arc<dyn KeyProvider>: Send, Sync);
    assert_impl_all!(std::sync::Arc<dyn BlockDevice>: Send, Sync);

    // S3: their futures never cross cores — structurally, every contract
    // method returns `LocalBoxFuture`, whose trait object carries no Send
    // bound (the alias itself is pinned !Send in cascade-types' tests).
    assert_not_impl_any!(cascade_types::LocalBoxFuture<'static, ()>: Send);

    #[test]
    fn op_ctx_carries_the_rule_eight_triple() {
        let span = Span::noop();
        let ctx = OpCtx::new(TenantContext::for_database(DatabaseId(1)), &span)
            .with_deadline(std::time::Instant::now());
        assert!(ctx.deadline.is_some());
        assert_eq!(ctx.tenant.database, DatabaseId(1));
    }

    // S3, negatively: the subscription and the per-core handle are !Send.
    assert_not_impl_any!(Box<dyn LogSubscription>: Send);
    assert_not_impl_any!(Box<dyn BlockHandle>: Send);
}
