//! Multi-tenant resource governance.
//!
//! **v1: API surface + no-op enforcement.** See `README.md`, `AI/CONTEXT.md`, README §6.1.

#![allow(dead_code)]

use cascade_types::DatabaseId;

/// Quota definition. Phase 7+ wires this into the runtime / BPM / I/O paths.
#[derive(Debug, Clone)]
pub struct ResourceGroup {
    pub name: String,
    pub cpu_shares: u32,
    pub memory_limit_bytes: u64,
    pub io_bandwidth_bps: u64,
    pub max_connections: u32,
    // ... etc.
}

/// Look up the resource group bound to a database. v1: always returns `None`.
pub fn group_for(_db: DatabaseId) -> Option<ResourceGroup> {
    None
}
