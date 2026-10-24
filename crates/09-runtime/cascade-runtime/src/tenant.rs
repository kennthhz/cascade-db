//! Tenant/span plumbing for the runtime slice (FR-20, FR-21; I5, I6).
//!
//! The point of this module is that the *signatures* exist from the first
//! query: every hot-path function takes `&TenantContext` and `&Span` and
//! forwards both. v1 spans are no-ops (cascade-telemetry's reservation) and
//! the counters here are the per-tenant tagging the plan asks for — a real
//! exporter replaces the map in a later phase without touching call sites.

use std::{collections::HashMap, sync::Mutex};

use cascade_telemetry::Span;
use cascade_tenant::TenantContext;
use cascade_types::{DatabaseId, TenantId};

/// Per-tenant query counters, tagged by the exact context received — the
/// read side is what tests (and later, stats views) assert against.
#[derive(Debug, Default)]
pub struct TenantCounters {
    inner: Mutex<HashMap<(DatabaseId, Option<TenantId>), u64>>,
}

impl TenantCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// One query handled for `ctx`. Borrows only — the context is never
    /// cloned into the hot path.
    pub fn count_query(&self, ctx: &TenantContext) {
        let mut map = self.inner.lock().expect("tenant counters lock");
        *map.entry((ctx.db, ctx.tenant)).or_insert(0) += 1;
    }

    /// Queries seen for exactly this (database, tenant) pair.
    pub fn query_count(&self, ctx: &TenantContext) -> u64 {
        let map = self.inner.lock().expect("tenant counters lock");
        map.get(&(ctx.db, ctx.tenant)).copied().unwrap_or(0)
    }
}

/// The slice's fixed dev tenant for the debug engine (spec §5.3 debug; plan
/// A1: one fixed-row table). T1.7 maps real database names to contexts via
/// the catalog.
pub const fn dev_context() -> TenantContext {
    TenantContext {
        db: DatabaseId(0),
        tenant: None,
    }
}

/// The hot-path shape every query-path function carries (FR-20/FR-21): data
/// in, context and span borrowed, nothing else global.
///
/// Today it records the per-tenant counter; the debug engine (T1.7) calls
/// this from its executor loop.
pub fn debug_hot_path(ctx: &TenantContext, span: &Span, counters: &TenantCounters) {
    // Shape reservation: the span is forwarded to children (none yet), never
    // stored — v1 spans are no-ops.
    let _ = span;
    counters.count_query(ctx);
}
