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

/// Hybrid logical clock timestamp (HLD v2 §6): `48-bit ms << 16 | 16-bit
/// logical counter`.
///
/// The physical part counts **milliseconds since 2026-01-01T00:00:00Z** — a
/// custom epoch, required by the version-word contract ([`VersionWord`]): it
/// keeps bit 63 clear on every issued timestamp until roughly year 6488, so a
/// committed word can never collide with the uncommitted tag. WP2 owns the
/// clock machinery; this type pins only the wire encoding and the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc(pub u64);

/// Milliseconds between the Unix epoch and the HLC epoch (2026-01-01Z).
pub const HLC_EPOCH_UNIX_MS: u64 = 1_767_225_600_000;

/// 64-bit MVCC version word (HLD v2 §6, WP1 F1) — the field that sits in
/// every row header and index entry.
///
/// ```text
/// 0                       → EMPTY: absent deleter; never a valid creator
/// bit 63 = 1              → UNCOMMITTED: bits 0..62 = Xid
/// bit 63 = 0, word ≠ 0    → COMMITTED: word = commit_ts (packed Hlc)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionWord(pub u64);

impl VersionWord {
    /// The absent word: no deleter, reserved zero.
    pub const EMPTY: VersionWord = VersionWord(0);

    const UNCOMMITTED_TAG: u64 = 1 << 63;

    /// The word an in-flight txn stamps: tagged with its Xid.
    ///
    /// Panics on `xid ≥ 2^63` — the top bit is the tag and must stay free
    /// (a programmer error, not a runtime condition).
    pub fn uncommitted(xid: Xid) -> Self {
        assert!(
            xid.0 < Self::UNCOMMITTED_TAG,
            "xid {} overflows 63 bits — the top bit is the uncommitted tag",
            xid.0
        );
        VersionWord(xid.0 | Self::UNCOMMITTED_TAG)
    }

    /// The word a commit stamps: the commit_ts itself.
    ///
    /// Panics on `ts ≥ 2^63` — impossible with the HLC custom epoch (see
    /// [`HLC_EPOCH_UNIX_MS`]); this is the WP1/F1 invariant guard against a
    /// clock that ignored the epoch contract.
    pub fn committed(ts: Hlc) -> Self {
        assert!(
            ts.0 < Self::UNCOMMITTED_TAG,
            "commit_ts {:#018x} has bit 63 set — the HLC epoch guarantees it cannot; \
             a non-custom-epoch clock would corrupt the version-word contract",
            ts.0
        );
        VersionWord(ts.0)
    }

    /// Decodes any raw word. Page parse is structural; word semantics are
    /// the visibility layer's (WP1 F2, invariant 3) — so decode is total.
    pub const fn from_raw(word: u64) -> Self {
        VersionWord(word)
    }

    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub const fn is_uncommitted(&self) -> bool {
        self.0 & Self::UNCOMMITTED_TAG != 0
    }

    /// The owning txn of an uncommitted word. Panics on a committed or
    /// empty word — call sites branch on [`is_uncommitted`] first (misuse
    /// is a bug, not a runtime condition).
    pub fn xid(&self) -> Xid {
        assert!(self.is_uncommitted(), "xid() on a non-uncommitted word");
        Xid(self.0 & !Self::UNCOMMITTED_TAG)
    }

    /// The commit_ts of a committed word. Panics on an uncommitted or
    /// empty word, for the same reason as [`xid`].
    pub fn commit_ts(&self) -> Hlc {
        assert!(
            !self.is_uncommitted() && !self.is_empty(),
            "commit_ts() on a committed word only"
        );
        Hlc(self.0)
    }
}

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

    // ---- version word (WP1 F1) ----

    #[test]
    fn version_word_roundtrips_both_states() {
        let uncommitted = VersionWord::uncommitted(Xid(u64::MAX >> 1)); // max 63-bit
        assert!(uncommitted.is_uncommitted());
        assert!(!uncommitted.is_empty());
        assert_eq!(uncommitted.xid(), Xid(u64::MAX >> 1));
        assert_eq!(uncommitted.0, u64::MAX, "tag bit | all-ones xid");

        let committed = VersionWord::committed(Hlc(0x0123_4567_89AB_CDEF));
        assert!(!committed.is_uncommitted());
        assert_eq!(committed.commit_ts(), Hlc(0x0123_4567_89AB_CDEF));
        assert_eq!(committed.0, 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn version_word_bit63_discriminates_every_time() {
        // The tag must survive any xid low bits — including all-ones low bits
        // — and committed words must keep bit 63 clear for any legal ts.
        for xid in [1u64, 2, 0x1234_5678, u64::MAX >> 1, (u64::MAX >> 1) - 1] {
            assert!(VersionWord::uncommitted(Xid(xid)).is_uncommitted());
        }
        for ts in [1u64, 2, 0x0F0F_0F0F_0F0F_0F0F, u64::MAX >> 1] {
            assert!(!VersionWord::committed(Hlc(ts)).is_uncommitted());
        }
    }

    #[test]
    fn empty_word_is_only_zero() {
        assert!(VersionWord::EMPTY.is_empty());
        assert!(!VersionWord::EMPTY.is_uncommitted());
        assert_eq!(VersionWord::from_raw(0), VersionWord::EMPTY);
        // 0 is never issued as an HLC (physical part > 0 from the epoch), so
        // EMPTY can never alias a committed word — F1's rationale.
        assert_ne!(
            VersionWord::committed(Hlc(1)).0,
            VersionWord::EMPTY.0,
            "the first legal commit_ts must differ from EMPTY"
        );
    }

    #[test]
    #[should_panic(expected = "overflows 63 bits")]
    fn xid_overflow_is_refused() {
        VersionWord::uncommitted(Xid(1u64 << 63));
    }

    #[test]
    #[should_panic(expected = "bit 63 set")]
    fn commit_ts_with_tag_bit_is_refused() {
        VersionWord::committed(Hlc(1u64 << 63));
    }

    #[test]
    #[should_panic(expected = "non-uncommitted")]
    fn xid_accessor_refused_on_committed_word() {
        let _ = VersionWord::committed(Hlc(7)).xid();
    }

    #[test]
    #[should_panic(expected = "committed word only")]
    fn commit_ts_accessor_refused_on_uncommitted_word() {
        let _ = VersionWord::uncommitted(Xid(7)).commit_ts();
    }
}
