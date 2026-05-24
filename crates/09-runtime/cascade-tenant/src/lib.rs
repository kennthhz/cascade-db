//! Tenant / database context propagation. §6.1 reservation.
//!
//! See `README.md` and `AI/CONTEXT.md`.

use cascade_types::{DatabaseId, TenantId};

/// Per-request context carrying tenant + database identity through the call stack.
///
/// In v1 nothing enforces quotas, but every layer must already accept and
/// forward this so §6.1 enforcement can be added later without touching call sites.
#[derive(Debug, Clone, Copy)]
pub struct TenantContext {
    pub db: DatabaseId,
    pub tenant: Option<TenantId>,
}

impl TenantContext {
    /// A synthetic context for background work that isn't tied to a user session.
    pub const fn system() -> Self {
        Self {
            db: DatabaseId(0),
            tenant: None,
        }
    }
}
