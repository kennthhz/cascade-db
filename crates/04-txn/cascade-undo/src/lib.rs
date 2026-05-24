//! Undo-log rollback segments.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

use cascade_types::{DatabaseId, Lsn};

/// Stable pointer from a row header to its pre-image record in the undo log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UndoRef {
    pub db: DatabaseId,
    pub segment: u32,
    pub offset: u32,
}

/// Allocates, writes, recycles undo segments. Implementation lands in Phase 3.
pub struct UndoSegmentMgr {
    // pool: ...
}

impl UndoSegmentMgr {
    pub async fn write_pre_image(&self, _db: DatabaseId, _payload: &[u8]) -> Result<(UndoRef, Lsn), UndoError> {
        todo!()
    }
}

#[derive(Debug)]
pub enum UndoError {
    Io,
    SnapshotTooOld,
    OutOfSpace,
}
