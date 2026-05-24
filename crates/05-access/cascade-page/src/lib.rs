//! 8 KB page layout for Cascade DB.
//!
//! See `README.md` and `AI/CONTEXT.md`. Format is locked in Phase 3.

#![allow(dead_code)]

use cascade_types::{Lsn, PAGE_SIZE};

/// Fixed page-header bytes. Exact layout settled in Phase 3.
#[repr(C)]
pub struct PageHeader {
    pub format_version: u16,
    pub page_kind: u16, // heap, btree-leaf, btree-internal, undo, columnar-seg, ...
    pub free_space_offset: u16,
    pub special_offset: u16,
    pub page_lsn: Lsn,
    pub crc32: u32,
    // ... TBD
}

const _: () = assert!(PAGE_SIZE == 8192);
