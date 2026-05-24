//! Userspace Buffer Pool Manager.
//!
//! See `README.md` and `AI/CONTEXT.md`.

// Phase 2 lands the real implementation. The shape of the public API will be
// drafted here first so dependent crates (access methods, executor) can be
// written against it.

#![allow(dead_code)]

use cascade_types::{DatabaseId, PageId};

/// Per-database page residency counters. §6.1 reservation: real counters,
/// no enforcement in v1.
#[derive(Debug, Default)]
pub struct PerDbAccounting {
    // Filled in during Phase 2.
}

impl PerDbAccounting {
    pub fn on_pin(&self, _db: DatabaseId, _page: PageId) { /* counter++ */ }
    pub fn on_unpin(&self, _db: DatabaseId, _page: PageId) { /* counter-- */ }
}
