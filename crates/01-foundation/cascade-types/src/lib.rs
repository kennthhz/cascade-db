//! Shared primitive types for Cascade DB.
//!
//! See `README.md` and `AI/CONTEXT.md` for the design contract.

use std::time::Instant;

use cascade_telemetry::Span;

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

/// A writing scope (storage-layer-design.md §4.2): the unit of log order,
/// durability and ownership — a database in v1 (architecture D10). Scope
/// IDs are never reused (S18): a dropped scope's ID is retired for good and
/// new ones come from counters that survive restarts. Reuse would let stale
/// data come back — a truncated table's old page served where the new table
/// expects zeros, or a dropped scope's old log frames attached to a new
/// scope that was handed the recycled ID (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId(pub u64);

/// Ownership epoch over a scope (§12.2): every record of a scope belongs to
/// exactly one epoch, epochs only increase along the LSN, and an
/// `open_writer` fences every older epoch (`LogSink` promise 1). Zero is
/// never issued — [`Epoch::FIRST`] is the first — so an all-zero record
/// field is distinguishable from a real epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Epoch(pub u64);

impl Epoch {
    /// The first epoch ever issued for a scope. Zero itself is reserved.
    pub const FIRST: Epoch = Epoch(1);

    /// The epoch for `raw`, refusing the reserved zero. Panics — issuing 0
    /// would defeat the fencing invariants; a programmer error, not a
    /// runtime condition (the same constructor-guard class as
    /// [`VersionWord::committed`]).
    pub fn issued(raw: u64) -> Self {
        assert!(raw != 0, "epoch 0 is reserved and never issued (§4.2)");
        Epoch(raw)
    }

    /// The next epoch after this one — the ownership bump a reopen or
    /// handoff takes (§12.2).
    pub fn next(self) -> Self {
        Epoch(self.0 + 1)
    }
}

/// A storage node in the managed service (§4.2). The v1 single server has
/// exactly one; `cascade-storage-remote` names replicas with it later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

// -----------------------------------------------------------------------------
// Sequence numbers
// -----------------------------------------------------------------------------

/// Log Sequence Number — monotonic byte offset within a database's WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

/// Transaction ID, per database (storage-layer-design.md §9, M0: the
/// storage contracts name it `TxnId`; the pre-M0 name stays available as
/// the [`Xid`] alias).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxnId(pub u64);

/// The pre-M0 name of [`TxnId`], kept until existing call sites migrate.
/// Deliberately not yet `#[deprecated]`: the attribute would put a warning
/// into every crate that still names `Xid`, and the workspace gate is
/// clippy-zero-warning. It lands together with that migration.
pub type Xid = TxnId;

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

// -----------------------------------------------------------------------------
// HLC packing (WP2 — the clock machinery itself is cascade-runtime's
// `txnmgr`; this crate stays data-only per its hard invariant 1)
// -----------------------------------------------------------------------------

impl Hlc {
    /// Width of the logical counter (the low bits).
    pub const LOGICAL_BITS: u32 = 16;

    /// The largest legal physical component: 2^47 ms past the HLC epoch
    /// (≈ year 6488) — beyond it a packed ts would set bit 63, the exact
    /// boundary [`VersionWord::committed`] guards.
    pub const PHYSICAL_MAX_MS: u64 = (1 << 47) - 1;

    /// Packs `physical_ms` and `logical` into the F1 encoding.
    ///
    /// Panics on a physical part past [`Hlc::PHYSICAL_MAX_MS`] — the same
    /// constructor-guard class as [`VersionWord::committed`]: a ts that
    /// would set version-word bit 63 must never exist, and the callers
    /// here (replay decode, clock internals) have no recovery path.
    pub fn from_parts(physical_ms: u64, logical: u16) -> Self {
        assert!(
            physical_ms <= Self::PHYSICAL_MAX_MS,
            "HLC physical ms {physical_ms} past PHYSICAL_MAX_MS ({}) — \
             would set version-word bit 63 (WP1 F1 epoch contract)",
            Self::PHYSICAL_MAX_MS
        );
        Hlc((physical_ms << Self::LOGICAL_BITS) | logical as u64)
    }

    /// The physical component: milliseconds since the HLC epoch.
    pub const fn physical_ms(self) -> u64 {
        self.0 >> Self::LOGICAL_BITS
    }

    /// The logical counter within the physical millisecond.
    pub const fn logical(self) -> u64 {
        self.0 & ((1 << Self::LOGICAL_BITS) - 1)
    }
}


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

    /// The word an in-flight txn stamps: tagged with its TxnId.
    ///
    /// Panics on `xid ≥ 2^63` — the top bit is the tag and must stay free
    /// (a programmer error, not a runtime condition).
    pub fn uncommitted(xid: TxnId) -> Self {
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
    pub fn xid(&self) -> TxnId {
        assert!(self.is_uncommitted(), "xid() on a non-uncommitted word");
        TxnId(self.0 & !Self::UNCOMMITTED_TAG)
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
// Operation context (storage-layer-design.md §4.2, step M0)
// -----------------------------------------------------------------------------

/// Which tenant and database an operation acts on. Every storage-contract
/// call carries one (S4 / Rule 8), and every I/O submission is tagged with
/// it for per-database accounting (§4.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TenantContext {
    /// The database the operation touches. In v1 every database is one
    /// scope, which also holds its catalog (§12.2).
    pub database: DatabaseId,
    /// The governing tenant, when one exists. In v1 a tenant typically is
    /// one database, so single-server use leaves this `None`.
    pub tenant: Option<TenantId>,
}

impl TenantContext {
    /// The context for a lone database with no governing tenant — the v1
    /// single-server shape.
    pub const fn for_database(database: DatabaseId) -> Self {
        TenantContext {
            database,
            tenant: None,
        }
    }
}

/// The context every storage-contract call takes explicitly (S4): who the
/// caller acts for, under which tracing span, by when it must finish. Cheap
/// to construct and copy — hot-path functions forward it unchanged.
#[derive(Debug, Clone, Copy)]
pub struct OpCtx<'a> {
    pub tenant: TenantContext,
    pub span: &'a Span,
    /// `None` means no caller deadline; components check it at their
    /// natural await points (Rule 8).
    pub deadline: Option<Instant>,
}

impl<'a> OpCtx<'a> {
    /// A context with no deadline.
    pub fn new(tenant: TenantContext, span: &'a Span) -> Self {
        OpCtx {
            tenant,
            span,
            deadline: None,
        }
    }

    /// Builder: sets the caller deadline (Rule 8).
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
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

// -----------------------------------------------------------------------------
// Futures (storage-layer-design.md §4.1)
// -----------------------------------------------------------------------------

/// A boxed future that stays on the calling core (S3): storage-contract
/// futures are polled to completion where they were created, so the trait
/// object carries no `Send` bound. The contracts' async methods return this
/// shape — one heap allocation per call, on paths where I/O dominates.
pub type LocalBoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

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
        let uncommitted = VersionWord::uncommitted(TxnId(u64::MAX >> 1)); // max 63-bit
        assert!(uncommitted.is_uncommitted());
        assert!(!uncommitted.is_empty());
        assert_eq!(uncommitted.xid(), TxnId(u64::MAX >> 1));
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
            assert!(VersionWord::uncommitted(TxnId(xid)).is_uncommitted());
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
        VersionWord::uncommitted(TxnId(1u64 << 63));
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
        let _ = VersionWord::uncommitted(TxnId(7)).commit_ts();
    }

    // ---- scopes, epochs and the operation context (storage M0) ----

    #[test]
    fn scope_ids_order_hash_and_copy() {
        let a = ScopeId(1);
        let b = ScopeId(2);
        assert!(b > a, "scope IDs order: counters that survive restarts (S18)");
        let copied = a; // Copy — every core holds the scope by value
        assert_eq!(copied, a);

        let mut set = std::collections::HashSet::new();
        set.insert(a);
        set.insert(b);
        set.insert(ScopeId(1));
        assert_eq!(set.len(), 2, "ScopeId hashes by value");
    }

    #[test]
    fn epochs_start_at_one_and_increase() {
        assert_eq!(Epoch::FIRST, Epoch(1), "epoch 0 is reserved, never issued");
        assert_eq!(Epoch::issued(41).next(), Epoch(42));
        assert!(Epoch::issued(7) > Epoch::FIRST);
    }

    #[test]
    #[should_panic(expected = "epoch 0 is reserved")]
    fn epoch_issued_refuses_the_reserved_zero() {
        let _ = Epoch::issued(0);
    }

    #[test]
    fn node_ids_are_plain_u32_handles() {
        assert_eq!(NodeId(3), NodeId(3));
        assert_ne!(NodeId(3), NodeId(4));
    }

    #[test]
    fn txnid_is_the_new_name_and_xid_still_compiles() {
        let t = TxnId(9);
        let x: Xid = t; // same type through the alias — old call sites keep working
        assert_eq!(x, TxnId(9));
        let legacy = TxnId(5); // the old construction form, unchanged
        assert_eq!(VersionWord::uncommitted(legacy).xid(), TxnId(5));
        assert_eq!(VersionWord::uncommitted(TxnId(3)).xid(), TxnId(3));
    }

    #[test]
    fn op_ctx_carries_tenant_span_and_deadline() {
        use std::time::{Duration, Instant};

        let span = cascade_telemetry::Span::noop();
        let ctx = OpCtx::new(TenantContext::for_database(DatabaseId(1)), &span);
        assert_eq!(ctx.tenant, TenantContext::for_database(DatabaseId(1)));
        assert_eq!(ctx.tenant.tenant, None);
        assert!(ctx.deadline.is_none());

        let ctx = ctx.with_deadline(Instant::now() + Duration::from_secs(1));
        assert!(ctx.deadline.is_some());

        let forwarded = ctx; // Copy — hot-path functions forward it unchanged
        assert_eq!(forwarded.tenant, ctx.tenant);
    }

    #[test]
    fn tenant_context_can_carry_a_governing_tenant() {
        let ctx = TenantContext {
            database: DatabaseId(2),
            tenant: Some(TenantId(7)),
        };
        assert_eq!(ctx.tenant, Some(TenantId(7)));
    }

    #[test]
    fn local_box_future_is_not_send() {
        // S3: storage-contract futures stay on the calling core. If this
        // assertion fails to compile, LocalBoxFuture grew a Send bound.
        static_assertions::assert_not_impl_any!(LocalBoxFuture<'static, ()>: Send);
    }
}
