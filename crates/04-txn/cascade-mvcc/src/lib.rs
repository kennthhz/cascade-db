//! In-place MVCC for Cascade DB.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

use cascade_types::{DatabaseId, TxnId};

/// Per-database, monotonic XID allocator. 64-bit; no wraparound.
pub struct XidAllocator {
    db: DatabaseId,
    // next: AtomicU64,
}

/// Snapshot — the set of committed XIDs visible to a reader.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub xmin: TxnId,
    pub xmax: TxnId,
    // pub running: Vec<TxnId>, // XIDs concurrent at snapshot time
}

/// Visibility predicate.
pub fn visible_to(_row_xmin: TxnId, _row_xmax: Option<TxnId>, _snapshot: &Snapshot) -> bool {
    // Implementation lands in Phase 3.
    todo!()
}
