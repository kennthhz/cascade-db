//! Logical decoder for CDC.
//!
//! **v1: trait surface + no-op.** See `README.md`, `AI/CONTEXT.md`, README §6.3.

#![allow(dead_code)]

use cascade_types::{DatabaseId, Lsn, Xid};

/// A decoded logical change event. v1: placeholder shape.
#[derive(Debug)]
pub struct ChangeEvent {
    pub lsn: Lsn,
    pub xid: Xid,
    pub db: DatabaseId,
    // table, op, before, after — filled in Phase 7.
}

/// CDC subscriber trait. Phase 7+ implements gRPC + PG-compatible publication streams.
pub trait CdcSubscriber {
    fn deliver(&self, evt: &ChangeEvent);
}
