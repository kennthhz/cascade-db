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

/// 4 KB-aligned buffer required by `O_DIRECT`.
///
/// Owned by the BPM as frame memory and lent to storage for the duration of
/// an I/O. Alignment is the whole point: `O_DIRECT` rejects unaligned
/// buffers, so allocation goes through the global allocator with a 4 096-byte
/// layout alignment, never through `Vec<u8>`.
pub struct AlignedBuf {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

// The buffer owns its memory exclusively — it is a value like any other.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

/// Alignment every `AlignedBuf` guarantees (the strictest common `O_DIRECT`
/// requirement).
const BUF_ALIGN: usize = 4096;

impl AlignedBuf {
    /// A zero-filled buffer of `len` bytes, [`BUF_ALIGN`]-aligned.
    pub fn zeroed(len: usize) -> Self {
        assert!(len > 0, "AlignedBuf must hold at least one byte");
        let layout = std::alloc::Layout::from_size_align(len, BUF_ALIGN)
            .expect("page-sized allocations are representable");
        // SAFETY: layout is non-zero-sized; alloc_zeroed either succeeds or
        // we invoke the allocator's handler.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        AlignedBuf {
            ptr: std::ptr::NonNull::new(ptr).expect("non-null checked above"),
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is a live, aligned allocation of `len` bytes for the
        // lifetime of self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as_slice, mutable — self holds the only handle.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout =
            std::alloc::Layout::from_size_align(self.len, BUF_ALIGN).expect("layout round-trips");
        // SAFETY: allocated with alloc_zeroed at exactly this layout.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), layout) };
    }
}

impl std::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let head = self.as_slice().iter().take(8).copied().collect::<Vec<_>>();
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .field("head", &head)
            .finish()
    }
}

/// Size of an `AlignedBuf` and the storage page.
pub const PAGE_SIZE: usize = 8192;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_four_kibi_aligned_and_zeroed() {
        for _ in 0..16 {
            let mut buf = AlignedBuf::zeroed(PAGE_SIZE);
            assert_eq!(
                buf.as_slice().as_ptr() as usize % BUF_ALIGN,
                0,
                "O_DIRECT requires 4 KiB alignment"
            );
            assert_eq!(buf.len(), PAGE_SIZE);
            assert!(buf.as_slice().iter().all(|&b| b == 0));
            buf.as_mut_slice()[0] = 0xAB;
            assert_eq!(buf.as_slice()[0], 0xAB);
        }
    }

    #[test]
    fn buffers_have_independent_storage() {
        let mut a = AlignedBuf::zeroed(PAGE_SIZE);
        let b = AlignedBuf::zeroed(PAGE_SIZE);
        a.as_mut_slice()[100] = 1;
        assert_eq!(b.as_slice()[100], 0, "no aliasing between buffers");
    }
}
