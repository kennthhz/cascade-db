//! The in-memory fakes (§4.10) behind the `fake` feature:
//! [`MemLogStore`] implements `LogSink`/`LogSource`/`LogScopes` in memory
//! with `crash()` and injectors; [`MemPageStore`] implements
//! `PageSource`/`PageScopes` over any [`LogSource`] with an injected redo
//! function; [`MemBlockDevice`] models crash semantics at random cut
//! points (§4.8); [`MemKeyProvider`] hands out versioned keys.
//!
//! The fakes are single-threaded-test-shaped: their futures resolve
//! inline (a `wait_durable` performs the flush on the calling task, the
//! way the real flusher would — no scheduler is needed to honor the
//! promises), and their state is `Arc<Mutex<_>>` so a `Send + Sync` trait
//! object is honestly shared.

mod block_device;
mod key_provider;
mod log_store;
mod page_store;

pub use block_device::MemBlockDevice;
pub use key_provider::MemKeyProvider;
pub use log_store::MemLogStore;
pub use page_store::MemPageStore;

use crate::OpCtx;

/// A trivial context for fake-driven tests (span no-op, no deadline).
pub fn test_ctx<'a>(span: &'a cascade_telemetry::Span, db: u32) -> OpCtx<'a> {
    OpCtx::new(
        crate::TenantContext::for_database(cascade_types::DatabaseId(db)),
        span,
    )
}
