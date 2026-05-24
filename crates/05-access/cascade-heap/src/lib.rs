//! Heap (row-store) access method.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

use cascade_types::{PageId, PageNo};

/// Tuple identifier: (page, slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tid {
    pub page: PageId,
    pub slot: u16,
}

/// Convenience type for callers that want a compact representation.
pub type SlotId = u16;

/// A heap relation handle. Implementation lands in Phase 3.
pub struct Heap {
    _placeholder: (),
}

const _: () = {
    let _ = std::mem::size_of::<PageNo>();
};
