//! Shared primitive types for Cascade DB.
//!
//! See `README.md` and `AI/CONTEXT.md` for the design contract.

// -----------------------------------------------------------------------------
// Identifiers
// -----------------------------------------------------------------------------

/// Logical database. Each has its own WAL, XID space, and (eventually) resource group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DatabaseId(pub u32);

/// Tenant identity. Reserved for §6.1 governance. In v1 a tenant typically = one database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(pub u32);

/// A physical storage segment within a database (table, index, undo segment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(pub u32);

/// Page offset within a segment.
pub type PageNo = u32;

/// Globally unique, backend-agnostic identifier for an 8 KB page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    pub db: DatabaseId,
    pub segment: SegmentId,
    pub page_no: PageNo,
}

// -----------------------------------------------------------------------------
// Sequence numbers
// -----------------------------------------------------------------------------

/// Log Sequence Number — monotonic byte offset within a database's WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

/// Transaction ID. Per-database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Xid(pub u64);

// -----------------------------------------------------------------------------
// I/O buffer
// -----------------------------------------------------------------------------

/// 4 KB-aligned, 8 KB-sized buffer required by `O_DIRECT`.
///
/// Backed by pre-allocated Buffer Pool RAM. Owned by the BPM, lent to storage
/// for the duration of an I/O. Ownership-move semantics matter — see
/// `cascade-storage` for usage.
pub struct AlignedBuf {
    // Implementation: aligned allocation + Drop. Filled in during Phase 2.
    _placeholder: (),
}

/// Size of an `AlignedBuf` and the storage page.
pub const PAGE_SIZE: usize = 8192;
