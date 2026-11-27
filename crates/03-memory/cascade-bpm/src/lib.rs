//! Userspace Buffer Pool Manager.
//!
//! See `README.md` and `AI/CONTEXT.md`.
//!
//! ## The cross-core pool (T6.2, ADR-0003)
//!
//! The production design (§8.1) is **one BPM per NUMA, shared by all cores
//! on that NUMA**: `Send + Sync`, a sharded page table, per-frame latching,
//! and frames from an in-crate `mbind` mapping so a boot on the pool's node
//! binds every page to it (I4a). The BPM owns no store: every read call
//! takes `&impl PageSource` — the caller's own reactor-local handle, which
//! is what makes §8.1's "no file handle across reactors" rule structural
//! (nothing can touch a page without naming a source, and the only sources
//! in scope are the caller's).
//!
//! Frame access is the ADR-0003 item-3 model: one `std::Mutex` per frame
//! guards the page bytes AND the frame's metadata (page id, scope, dirty
//! bit, page_lsn), so every snapshot and every mutation is a short,
//! bounded critical section that can never span an await (the I11 story by
//! construction). Guards hold a **copy-out snapshot** (an owned 8 KiB
//! `AlignedBuf`) — one memcpy per touch, accepted, PG-style page
//! granularity — so a guard is `'static`: ownable across awaits and across
//! threads, with no borrow of the pool.
//!
//! Pin protocol (the v1 latch; the OLC read-write latch lands with the
//! btree): the low 30 bits of a per-frame `AtomicU32` are the shared pin
//! count (readers `fetch_add`), bit 31 is the exclusive/writer flag
//! (`page_exclusive` CASes `0 → 0x8000_0001`), bit 30 is the CLAIM flag that
//! reserves a frame for a load-in-progress or an eviction. The flag words
//! of a successful reader pin, a successful writer pin, and a claim are
//! mutually exclusive single-word CAS states — no interleaving lets two of
//! them through — which is what makes snapshot coherence hold without any
//! lock across a memcpy: every byte mutation happens under EXCL (a writer
//! guard's closure) or CLAIM (a loader's fill, an eviction's reset), and
//! those refuse every pin that could observe a torn image. Shared pins
//! refuse (fail fast, `Contended`) while a claim or an exclusive pin is
//! held — the caller owns the retry (the heap's append-vs-scan progress
//! lives at that level, not here).
//!
//! ## The cache rules (storage M8, §12.5 — the log is the only durable
//! write)
//!
//! The pool is a **read cache over `PageSource`, never a write-back**: the
//! log is the only durable write on the commit path, and only the page
//! store writes pages. Compute changes a cached page by *applying* its
//! mini-transaction group to the frame
//! ([`PageGuardMut::write_logged`], which stamps the page's LSN with the
//! group's end boundary, S17) and appending the same bytes to the log —
//! the page store materializes the page independently by replaying the
//! log. Nothing here ever writes a page back, so there is no flush path,
//! no dirty-writeback, and no checkpoint.
//!
//! - **The miss path reads at the scope's durable LSN** (§4.5): the pool
//!   asks its [`DurableLsn`] view for the number and fetches the page
//!   exactly as of it. The image's own page LSN — the end of the last
//!   record applied (S17) — is what the frame remembers.
//! - **A page is never evicted while its page LSN is above the durable
//!   LSN** (§12.5): the page store may not have its changes yet. Once
//!   durability covers the stamp, dropping the page loses nothing — the
//!   store rebuilds it — so a durable-covered dirty page is dropped in
//!   place, with no write.
//! - **Unlogged changes are never evicted in v1**: a frame modified
//!   through plain `write` (no stamp) cannot be rebuilt from the log, so
//!   eviction would lose the image outright. Such a frame is skipped by
//!   the eviction ladder and refused by [`Bpm::evict_page`] — the caller's
//!   contract (like PG's unlogged tables) with eviction taking the crash
//!   semantics' place: the cost shows up as pool pressure, the honest
//!   backpressure §12.5 leaves as an open question.
//!
//! Eviction (the greedy ladder, ADR-0003 item 6) claims the frame first —
//! pins refused, so no one can mutate it — then drops: a clean victim, or
//! a durable-covered one. `PoolFull` only when every frame is pinned,
//! claimed, or undurable — which is what makes "a table larger than the
//! pool" survivable for scans, and what makes a lagging durable LSN the
//! real pressure signal.
//!
//! The durable-LSN view is injected at construction: the pool never
//! depends on the log store — the runtime glues the two together.

use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex, OnceLock, RwLock, Weak,
    },
};

use cascade_storage_api::{DurableLsn, OpCtx, PageKey, PageSource, StoreError};
use cascade_types::{AlignedBuf, DatabaseId, Lsn, PageId, ScopeId, PAGE_SIZE};

// --- placement (ADR-0003 item 5: the frozen hash lives here) -----------------

/// FNV-1a 64 over the three `PageId` fields (I3 — the key is
/// `(DatabaseId, SegmentId, PageNo)`, never a file path), each fixed-width
/// little-endian in a frozen order. The algorithm and layout are *frozen*:
/// a page's home pool must not move across builds, processes, or restarts,
/// so this is deliberately not `std`'s `RandomState`-keyed `Hash` (whose
/// seed differs per process). Golden literals in `tests/pool.rs` pin the
/// contract; the runtime's `pools.rs` wrapper carries the parity goldens.
pub fn page_hash(page_id: &PageId) -> u64 {
    fn mix(h: &mut u64, bytes: &[u8]) {
        for b in bytes {
            *h ^= u64::from(*b);
            *h = h.wrapping_mul(0x0100_0000_01b3); // 0x100000001b3, the FNV-1a 64 prime
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    mix(&mut h, &page_id.db.0.to_le_bytes());
    mix(&mut h, &page_id.segment.0.to_le_bytes());
    mix(&mut h, &page_id.page_no.to_le_bytes());
    h
}

/// FR-22's cross-affinity placement: `hash(PageId) mod num_numas` — a
/// single direct lookup, no miss-probing (§8.1). Panics on `num_numas ==
/// 0` (a boot with no pools never routes pages).
pub fn home_numa(page_id: &PageId, num_numas: usize) -> usize {
    assert!(num_numas > 0, "placement needs at least one pool");
    (page_hash(page_id) % num_numas as u64) as usize
}

// --- pool memory (ADR-0003 item 4: in-crate mbind frames) --------------------

/// One pool's page RAM: `frames × PAGE_SIZE` bytes in a single private
/// mapping with a `PAGE_SIZE`-aligned base, mirroring the runtime's
/// pools.rs mapping discipline — one VMA, owned from `mmap` to `munmap`,
/// directly usable as page images (O_DIRECT-ready). This is the BPM's own
/// memory, deliberately NOT the `PoolRegistry` mapping: the demo heap
/// writes bench pages into the registry's frames by raw address, and BPM
/// frames there would alias them (ADR-0003 item 4).
struct PageRam {
    /// The address `mmap` returned — `munmap` needs exactly this.
    raw: *mut libc::c_void,
    total: usize,
    /// `PAGE_SIZE`-aligned base; frame `i` lives at `base + i*PAGE_SIZE`.
    base: *mut u8,
}

// SAFETY: the pointer targets this struct's own mapping, which is stable
// for the pool's lifetime (dropped only by `Drop`, after the last frame —
// and therefore the last guard — is gone). The bytes are only ever
// dereferenced under the owning frame's mutex or while a pin word that
// excludes mutators is held, which is what makes the raw access sound
// under `Send + Sync`.
unsafe impl Send for PageRam {}
unsafe impl Sync for PageRam {}

impl PageRam {
    fn map(frames: usize) -> io::Result<Self> {
        let len = frames * PAGE_SIZE;
        let total = len + PAGE_SIZE; // slack for the alignment bump
        // SAFETY: anonymous private mapping; the only derefs are under the
        // per-frame mutex, and `Drop` unmaps exactly `raw`/`total`.
        let region = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if region == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = ((region as usize) + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        Ok(PageRam {
            raw: region,
            total,
            base: base as *mut u8,
        })
    }

    /// Binds future page faults of this mapping to NUMA node `numa`
    /// (`mbind(MPOL_BIND | MPOL_F_STATIC_NODES)`) — the placement mechanism
    /// (I4a: pools are backed by NUMA-local memory regardless of which
    /// reactor faults a page in). `false` = the kernel refused (node
    /// absent — WSL2 and kin — or memoryless); the caller degrades to
    /// first-touch with a warning, exactly like pools.rs.
    fn bind_node(&self, numa: u32) -> bool {
        if numa >= 64 {
            // The nodemask is one word; no host this targets has 64+ nodes.
            return false;
        }
        let mask: libc::c_ulong = 1 << numa;
        // The libc crate does not wrap `mbind` on all targets; the syscall
        // is stable (Linux ≥ 2.6) and takes exactly these six arguments.
        // SAFETY: base/len describe this struct's own live mapping.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_mbind,
                self.base as *mut libc::c_void,
                self.total as libc::c_ulong,
                libc::MPOL_BIND,
                &mask as *const libc::c_ulong,
                64usize,
                libc::MPOL_F_STATIC_NODES,
            )
        };
        rc == 0
    }

    /// Byte pointer of frame `idx`. SAFETY (callers): `idx < frames` —
    /// indices come from the frame array — and no concurrent byte
    /// mutation (frame mutex held, or a pin word excluding mutators).
    fn frame_ptr(&self, idx: u32) -> *mut u8 {
        // SAFETY: `idx * PAGE_SIZE + PAGE_SIZE <= len <= total` for every
        // index the frame array can produce.
        unsafe { self.base.add(idx as usize * PAGE_SIZE) }
    }
}

impl Drop for PageRam {
    fn drop(&mut self) {
        // SAFETY: `raw`/`total` are exactly what `mmap` handed out; this is
        // the only drop.
        unsafe { libc::munmap(self.raw, self.total) };
    }
}

// --- per-database accounting --------------------------------------------------

/// Per-database page counters (§6.1): *live pin counts* per database,
/// incremented on every pin and decremented on every guard drop — real
/// accounting on actual pin/unpin (invariant 3), never sampled.
///
/// No enforcement in v1; governance (`cascade-governance`) consumes these.
/// One mutex (not sharded) — pin/unpin critical sections are single-map
/// entry updates; escalate to shards on measured contention, mirroring the
/// page-table decision's escalation clause.
#[derive(Debug, Default)]
pub struct PerDbAccounting {
    pins: Mutex<HashMap<DatabaseId, u64>>,
}

impl PerDbAccounting {
    pub fn on_pin(&self, db: DatabaseId, _page: PageId) {
        let mut pins = self.lock();
        *pins.entry(db).or_insert(0) += 1;
    }

    pub fn on_unpin(&self, db: DatabaseId, _page: PageId) {
        let mut pins = self.lock();
        if let Some(count) = pins.get_mut(&db) {
            *count = count.saturating_sub(1);
        }
    }

    /// Live pins held by this pool for `db` right now.
    pub fn pinned(&self, db: DatabaseId) -> u64 {
        self.lock().get(&db).copied().unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<DatabaseId, u64>> {
        // Poison tolerance: a panicking guard must not wedge the pool.
        self.pins.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// --- config + errors -------------------------------------------------------------

/// Pool sizing. `max_frames` is the pool's whole capacity; frames freed by
/// eviction are reused, so it is also the high-water mark. `cores_on_numa`
/// sizes the page-table shard count (`next_pow2(2 × cores)`, ADR-0003
/// item 1 — the sharding is internal and unfrozen). `numa` binds the frame
/// mapping to that node (`mbind`, ADR-0003 item 4); `None` or a refused
/// bind degrades to first-touch with a warning (I4a's warn-and-degrade).
#[derive(Debug, Clone, Copy)]
pub struct BpmConfig {
    pub max_frames: u32,
    pub cores_on_numa: usize,
    pub numa: Option<u32>,
}

#[derive(Debug)]
pub enum BpmError {
    /// Every frame is pinned or claimed, or the only candidates are
    /// undurable, and the pool genuinely cannot serve another page right
    /// now — the durable LSN lagging is the honest pressure signal
    /// (§12.5's open question).
    PoolFull {
        max_frames: u32,
    },
    /// The shared pin count would overflow — a pin leak at scale, not a
    /// transient condition.
    PinOverflow(PageId),
    /// The pin protocol refused: the page is exclusively pinned (a writer
    /// guard is alive) or claimed (a load or eviction holds it). The caller
    /// retries at its own level — the BPM never spins.
    Contended(PageId),
    /// The page source refused or failed — the page is not resident.
    Storage(StoreError),
}

impl From<StoreError> for BpmError {
    fn from(e: StoreError) -> Self {
        BpmError::Storage(e)
    }
}

// --- the page table (ADR-0003 item 1) --------------------------------------------

/// Hand-rolled sharded page table: `PageId → frame index`. `PageId → shard`
/// is the frozen placement hash, so a `get_or_insert` under ONE shard's
/// lock contains the double-install race entirely — no CAS loops, no epoch
/// machinery. Sharding is internal and deliberately NOT frozen (only the
/// placement hash is); escalate to flurry only on measured contention.
struct ShardedTable {
    shards: Box<[RwLock<HashMap<PageId, u32>>]>,
    shard_mask: u64,
}

impl ShardedTable {
    fn new(cores_on_numa: usize) -> Self {
        let count = (2 * cores_on_numa.max(1)).next_power_of_two();
        let shards = (0..count)
            .map(|_| RwLock::new(HashMap::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        ShardedTable {
            shards,
            shard_mask: (count - 1) as u64,
        }
    }

    fn shard(&self, page_id: &PageId) -> &RwLock<HashMap<PageId, u32>> {
        &self.shards[(page_hash(page_id) & self.shard_mask) as usize]
    }

    fn read_lock(
        &self,
        page_id: &PageId,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<PageId, u32>> {
        self.shard(page_id)
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn write_lock(
        &self,
        page_id: &PageId,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<PageId, u32>> {
        self.shard(page_id)
            .write()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, page_id: &PageId) -> Option<u32> {
        self.read_lock(page_id).get(page_id).copied()
    }

    /// Publish or lose: exactly one concurrent loader inserts; the loser
    /// learns the winner's frame.
    fn get_or_insert(&self, page_id: PageId, idx: u32) -> u32 {
        let mut shard = self.write_lock(&page_id);
        *shard.entry(page_id).or_insert(idx)
    }

    /// Unmaps — the caller holds the frame's claim, which is what excludes
    /// every other unmapper; the `expect` documents that.
    fn remove(&self, page_id: &PageId, idx: u32) {
        let removed = self.write_lock(page_id).remove(page_id);
        assert_eq!(removed, Some(idx), "unmap under claim: the table must agree");
    }

    fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.read().unwrap_or_else(|e| e.into_inner()).len())
            .sum()
    }
}

// --- frames -----------------------------------------------------------------------

/// Pin-word layout (the v1 latch, ADR-0003 item 3): low 30 bits = shared
/// count; bit 30 = CLAIM (a load or eviction holds the frame); bit 31 =
/// EXCL (an exclusive/writer guard is alive).
const PIN_EXCL: u32 = 1 << 31;
const PIN_CLAIM: u32 = 1 << 30;
const PIN_COUNT_MASK: u32 = PIN_CLAIM - 1;

/// Why a shared pin was refused (mapped to a `BpmError` with the real
/// `PageId` by the caller — the frame doesn't know its page id yet).
enum PinRefusal {
    /// An exclusive pin or a claim is live.
    Busy,
    /// The 30-bit count is exhausted.
    Overflow,
}

/// Everything mutable about a resident page, under one lock with its
/// bytes. `page_lsn` is the end LSN of the last record applied to the
/// cached image (S17) — stamped by `write_logged` on the apply path and
/// carried in from the fetched image on the load path; it is what the
/// §12.5 eviction rule compares against the scope's durable LSN. `dirty`
/// now means exactly "the image carries changes the log may not cover":
/// true after any `write`; for a stamped page it is redundant with the
/// stamp and kept for the unlogged case and diagnostics.
#[derive(Default)]
struct FrameInner {
    /// `None` while the frame is unclaimed cargo (never resident).
    page_id: Option<PageId>,
    /// The scope the page belongs to — set at fill; the eviction rule
    /// looks up the *victim's own* scope's durable LSN.
    scope: Option<ScopeId>,
    dirty: bool,
    /// The end LSN of the last record applied to the cached image (S17);
    /// `None` = the image has never been written (fresh zeros).
    page_lsn: Option<Lsn>,
}

struct Frame {
    /// Bytes + metadata under one lock: snapshots and mutations serialize
    /// here. Bounded (one memcpy), never across an await.
    inner: Mutex<FrameInner>,
    pin: AtomicU32,
}

impl Frame {
    fn new() -> Self {
        Frame {
            inner: Mutex::new(FrameInner::default()),
            pin: AtomicU32::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FrameInner> {
        // Poison tolerance: a panicking write closure must not wedge the
        // frame forever. The page may be mid-mutation (the log protects
        // durability; the memory image stays whatever the panic left).
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pin_word(&self) -> u32 {
        self.pin.load(Ordering::Relaxed)
    }

    /// Shared pin: `fetch_add` refusing on EXCL/CLAIM (fail fast — the
    /// caller's protocol reports `Contended` and the access method owns
    /// the retry). The RMW loop cannot interleave with a writer's
    /// `CAS(0 → EXCL|1)` or a claim's `CAS(0 → CLAIM)`: exactly one of the
    /// three wins the word, which is the snapshot-coherence guarantee.
    fn try_shared_pin(&self) -> Result<(), PinRefusal> {
        loop {
            let word = self.pin_word();
            if word & (PIN_EXCL | PIN_CLAIM) != 0 {
                return Err(PinRefusal::Busy);
            }
            if word & PIN_COUNT_MASK == PIN_COUNT_MASK {
                return Err(PinRefusal::Overflow);
            }
            if self
                .pin
                .compare_exchange_weak(word, word + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// Exclusive pin: CAS `0 → EXCL|1`. Any existing pin (reader or
    /// another writer) or claim refuses it.
    fn try_exclusive_pin(&self) -> bool {
        self.pin
            .compare_exchange(0, PIN_EXCL | 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Claim (load or eviction reservation): CAS `0 → CLAIM`.
    fn try_claim(&self) -> bool {
        self.pin
            .compare_exchange(0, PIN_CLAIM, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn release_shared(&self) {
        // Underflow is impossible by construction — the caller pins before
        // releasing — but a wrap here would silently corrupt pin
        // accounting, so it is defended rather than assumed.
        if self
            .pin
            .fetch_update(Ordering::Release, Ordering::Relaxed, |p| {
                p.checked_sub(1)
            })
            .is_err()
        {
            cascade_telemetry::log(
                cascade_telemetry::Level::Warn,
                "bpm_pin_underflow",
                &[],
            );
        }
    }

    fn release_exclusive(&self) {
        let prev = self.pin.swap(0, Ordering::Release);
        debug_assert_eq!(prev & PIN_EXCL, PIN_EXCL, "exclusive release on a non-writer");
        debug_assert_eq!(prev & PIN_COUNT_MASK, 1, "exclusive guard co-resident with pins");
    }

    fn release_claim(&self) {
        let prev = self.pin.swap(0, Ordering::Release);
        debug_assert_eq!(prev & PIN_CLAIM, PIN_CLAIM, "claim release on a non-claimant");
        debug_assert_eq!(prev & PIN_COUNT_MASK, 0, "claimed frame carried pins");
    }
}

// --- the pool ----------------------------------------------------------------------

/// One buffer pool (the per-NUMA case of FR-22). `Send + Sync`: shared by
/// every core on its NUMA. Owns NO store and never writes a page anywhere:
/// reads go through the caller's own `&impl PageSource` per call (§8.1's
/// no-file-handle-across-reactors rule, made structural), and the log is
/// the only durable write (storage M8, §12.5).
pub struct Bpm {
    config: BpmConfig,
    ram: PageRam,
    frames: Box<[Frame]>,
    /// Bump allocation: the next never-used frame.
    bump: AtomicU32,
    /// Frames freed by eviction; the loader claims one out of here.
    free: Mutex<Vec<u32>>,
    table: ShardedTable,
    /// The scope's durable LSN, as published by the log store — the miss
    /// path reads at it and the eviction rule defers to it (§12.5).
    durable: Arc<dyn DurableLsn>,
    accounting: PerDbAccounting,
    /// Whether the kernel accepted the node binding for the frame mapping
    /// (false = first-touch degrade; observable for the bare-metal check).
    node_bound: bool,
    /// Weak self-handle set by [`Bpm::arced`] — the only constructor — so
    /// guards can own a strong handle without any `Arc` pointer games
    /// (F3's "no unsafe outside the mapping" promise).
    self_ref: OnceLock<Weak<Bpm>>,
}

impl Bpm {
    /// Pre-allocates `config.max_frames` frames of page RAM (the pool's
    /// own `mbind` mapping — ADR-0003 item 4) and the sharded page table.
    /// `durable` is the scope's published durable-LSN view — load-bearing
    /// on every miss and every eviction, so it is a constructor argument,
    /// not an afterthought.
    fn new(config: BpmConfig, accounting: PerDbAccounting, durable: Arc<dyn DurableLsn>) -> Self {
        let ram = PageRam::map(config.max_frames as usize)
            .expect("pool mapping reserves (mmap failure is a boot-stopper)");
        let node_bound = match config.numa {
            Some(numa) => {
                let bound = ram.bind_node(numa);
                if !bound {
                    cascade_telemetry::log(
                        cascade_telemetry::Level::Warn,
                        "bpm_placement_fallback",
                        &[
                            ("numa", numa.to_string().as_str()),
                            ("policy", "first-touch (kernel has no such NUMA node)"),
                        ],
                    );
                }
                bound
            }
            None => false,
        };
        let frames = (0..config.max_frames).map(|_| Frame::new()).collect::<Vec<_>>();
        Bpm {
            config,
            ram,
            frames: frames.into_boxed_slice(),
            bump: AtomicU32::new(0),
            free: Mutex::new(Vec::new()),
            table: ShardedTable::new(config.cores_on_numa),
            durable,
            accounting,
            node_bound,
            self_ref: OnceLock::new(),
        }
    }

    /// The canonical constructor: build inside `Arc::new` and wire the
    /// pool's self-handle. Every guard recovers a strong handle through
    /// it, so a guard can be held across awaits and threads.
    pub fn arced(
        config: BpmConfig,
        accounting: PerDbAccounting,
        durable: Arc<dyn DurableLsn>,
    ) -> Arc<Bpm> {
        let bpm = Arc::new(Bpm::new(config, accounting, durable));
        let _ = bpm.self_ref.set(Arc::downgrade(&bpm));
        bpm
    }

    pub fn accounting(&self) -> &PerDbAccounting {
        &self.accounting
    }

    /// False when the frame mapping degraded to first-touch (no NUMA
    /// configured, or the kernel refused the bind).
    pub fn node_bound(&self) -> bool {
        self.node_bound
    }

    /// Base address of the frame mapping — the placement-diagnostic hook
    /// the bare-metal `numa_maps` test resolves (the runtime's pool test
    /// does the same against `PoolRegistry` memory).
    pub fn ram_addr(&self) -> usize {
        self.ram.base as usize
    }

    /// Guards are `'static` via an owned pool handle recovered from the
    /// `arced` self-reference.
    fn clone_arc(&self) -> Arc<Bpm> {
        self.self_ref
            .get()
            .expect("a live pool reference implies Bpm::arced built it")
            .upgrade()
            .expect("the caller's pool handle outlives this call — the guard's Arc is sound")
    }

    /// FR-23: hit → pin; miss → read through the caller's own page source
    /// at the scope's durable LSN (§4.5) → insert (exactly once,
    /// cross-core) → pin. The returned guard owns an 8 KiB snapshot —
    /// `'static`, ownable across awaits and threads — and unpins on drop,
    /// including through a panic.
    pub async fn page<S: PageSource>(
        &self,
        source: &S,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
        page_id: PageId,
    ) -> Result<PageGuard, BpmError> {
        // Hit: pinned under the shard read lock, which is what excludes a
        // concurrent eviction's unmap (it needs the write lock) between
        // the lookup and the pin — the frame cannot change occupants
        // under a pinned reader.
        if let Some(hit) = self.shared_hit(page_id) {
            return hit;
        }

        // Miss: the read runs with no locks held (siblings and sibling
        // reactors proceed freely), at the durable LSN — the leader's read
        // point (§4.5, §12.5).
        let at_lsn = self.durable.durable_lsn(scope);
        let image = source
            .get_page(
                ctx,
                scope,
                PageKey {
                    segment: page_id.segment,
                    page_no: page_id.page_no,
                },
                at_lsn,
            )
            .await?;

        // Re-check: another reactor may have loaded the same page while we
        // read. Pages are inserted exactly once per pool.
        if let Some(hit) = self.shared_hit(page_id) {
            return hit;
        }

        let idx = self.acquire_frame()?;
        let frame = &self.frames[idx as usize];
        debug_assert_eq!(
            frame.pin_word(),
            PIN_CLAIM,
            "acquire_frame hands out claimed, table-absent frames only"
        );

        // Fill under the claim: the frame is table-absent (unobservable to
        // hits and victim scans) until the publish below. The frame
        // remembers the IMAGE's page LSN (S17), not the read point — that
        // is what the eviction rule compares against durability.
        {
            let mut inner = frame.lock();
            inner.page_id = Some(page_id);
            inner.scope = Some(scope);
            inner.dirty = false;
            inner.page_lsn = (image.page_lsn.0 != 0).then_some(image.page_lsn);
            self.copy_into_frame(idx, &image.buf);
        }
        let winner = self.table.get_or_insert(page_id, idx);
        if winner != idx {
            // Another loader won the publish race; its frame is resident
            // (still claimed until its conversion — a pin here may refuse,
            // which is the protocol's fail-fast, not a torn read).
            self.abandon_frame(idx);
            return match self.shared_hit(page_id) {
                Some(hit) => hit,
                None => Err(BpmError::Contended(page_id)),
            };
        }
        // Convert our claim into our shared pin atomically: a reader that
        // beat us to the table entry saw CLAIM and refused — it can never
        // pin between publish and this swap, so the swap cannot clobber a
        // concurrent pin.
        frame.pin.swap(1, Ordering::AcqRel);
        self.accounting.on_pin(page_id.db, page_id);
        Ok(PageGuard {
            bpm: self.clone_arc(),
            idx,
            page_id,
            snapshot: image.buf,
        })
    }

    /// The exclusive-pinned form of [`page`](Bpm::page): CAS `0 → EXCL|1`,
    /// so it succeeds only on an unpinned, unclaimed frame — two writers,
    /// or a writer against readers/claims, refuse with
    /// [`BpmError::Contended`]. The caller owns the retry (the heap's
    /// tail-append protocol bumps to the next page on contention rather
    /// than spinning here — that is the starvation-freedom story, and it
    /// lives at the AM level by design).
    pub async fn page_exclusive<S: PageSource>(
        &self,
        source: &S,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
        page_id: PageId,
    ) -> Result<PageGuardMut, BpmError> {
        if let Some(hit) = self.exclusive_hit(page_id) {
            return hit;
        }

        // Miss: load, then re-check (another loader may have published
        // while we read) before occupying a frame of our own.
        let at_lsn = self.durable.durable_lsn(scope);
        let image = source
            .get_page(
                ctx,
                scope,
                PageKey {
                    segment: page_id.segment,
                    page_no: page_id.page_no,
                },
                at_lsn,
            )
            .await?;
        if let Some(hit) = self.exclusive_hit(page_id) {
            return hit;
        }

        let idx = self.acquire_frame()?;
        let frame = &self.frames[idx as usize];
        // Fill under the claim — same discipline as `page`.
        {
            let mut inner = frame.lock();
            inner.page_id = Some(page_id);
            inner.scope = Some(scope);
            inner.dirty = false;
            inner.page_lsn = (image.page_lsn.0 != 0).then_some(image.page_lsn);
            self.copy_into_frame(idx, &image.buf);
        }
        let winner = self.table.get_or_insert(page_id, idx);
        if winner != idx {
            self.abandon_frame(idx);
            return match self.exclusive_hit(page_id) {
                Some(hit) => hit,
                None => Err(BpmError::Contended(page_id)),
            };
        }
        frame.pin.swap(PIN_EXCL | 1, Ordering::AcqRel);
        self.accounting.on_pin(page_id.db, page_id);
        Ok(PageGuardMut {
            bpm: self.clone_arc(),
            idx,
            page_id,
            snapshot: image.buf,
        })
    }

    /// Frame acquisition for a loader (the minimal greedy eviction, ADR-0003
    /// item 6): a freed frame, else a fresh one, else a clean unpinned
    /// victim, else a durable-covered unpinned victim dropped in place —
    /// no write, no I/O, nothing to wait on (§12.5: dropping a page whose
    /// page LSN is at or below the durable LSN loses nothing). `PoolFull`
    /// only when every frame is pinned, claimed, or undurable. The
    /// returned frame is CLAIMED and table-absent: the loader fills and
    /// publishes it under that claim.
    fn acquire_frame(&self) -> Result<u32, BpmError> {
        if let Some(idx) = self.take_free() {
            return Ok(idx);
        }
        let slot = self.bump.fetch_add(1, Ordering::Relaxed);
        if slot < self.config.max_frames {
            let frame = &self.frames[slot as usize];
            // A fresh bump frame is unobservable (table-absent, page_id
            // None), so nothing else can have claimed it.
            if frame.try_claim() {
                return Ok(slot);
            }
            debug_assert!(false, "fresh bump frame was claimed by someone else");
        } else {
            // Pool exhausted: roll the bump back and try victims.
            self.bump.fetch_sub(1, Ordering::Relaxed);
        }
        if self.unmap_one_clean_victim() {
            if let Some(idx) = self.take_free() {
                return Ok(idx);
            }
        }
        if self.unmap_one_durable_victim() {
            if let Some(idx) = self.take_free() {
                return Ok(idx);
            }
        }
        Err(BpmError::PoolFull {
            max_frames: self.config.max_frames,
        })
    }

    /// Pops a freed frame and claims it. Free-list frames are always at
    /// pin 0 (`unmap_claimed` releases before pushing), so the claim
    /// cannot fail; a failure would mean corruption, and skipping the
    /// frame keeps the ladder moving.
    fn take_free(&self) -> Option<u32> {
        loop {
            let idx = self.free.lock().unwrap_or_else(|e| e.into_inner()).pop()?;
            if self.frames[idx as usize].try_claim() {
                return Some(idx);
            }
            debug_assert!(false, "free-list frame was not at pin 0");
        }
    }

    /// The §12.5 eviction rule over one frame's metadata: a page whose
    /// page LSN is at or below its scope's durable LSN loses nothing when
    /// dropped — every record for it below the stamp is in the durable
    /// prefix, and the page store serves it again at the durable LSN.
    /// Unlogged changes (dirty with no stamp) are never evictable in v1 —
    /// the log cannot rebuild them, so an eviction would lose the image
    /// outright. `scope` is `None` only for unoccupied cargo (never
    /// scanned).
    fn evictable(&self, scope: Option<ScopeId>, dirty: bool, page_lsn: Option<Lsn>) -> bool {
        match (dirty, page_lsn) {
            (true, None) => false, // unlogged changes: not rebuildable
            (_, Some(stamp)) => match scope {
                Some(scope) => stamp <= self.durable.durable_lsn(scope),
                None => false, // defensive: an occupied frame always has a scope
            },
            (false, None) => true, // never modified since load
        }
    }

    /// Scans for an unpinned clean resident frame, claims it, and unmaps
    /// it straight onto the free list (`true` = one frame freed). Clean
    /// victims are the ladder's cheap rung: no rule to check beyond the
    /// re-verify. The claim exists so the re-verify (the metadata could
    /// move between peek and claim) and the unmap are exclusive.
    fn unmap_one_clean_victim(&self) -> bool {
        for (idx, frame) in self.frames.iter().enumerate() {
            {
                let inner = frame.lock();
                if inner.page_id.is_none() || inner.dirty {
                    continue;
                }
            }
            if !frame.try_claim() {
                continue;
            }
            // Re-verify under the claim: the dirty bit may have moved
            // between the unlocked peek and the claim.
            let page_id = {
                let inner = frame.lock();
                if inner.dirty || inner.page_id.is_none() {
                    frame.release_claim();
                    continue;
                }
                inner.page_id.unwrap()
            };
            self.unmap_claimed(idx as u32, page_id);
            return true;
        }
        false
    }

    /// Scans for an unpinned DURABLE-COVERED dirty victim and drops it in
    /// place (`true` = one frame freed). This is M8's replacement for the
    /// write-back rung: a page whose stamp the durable LSN covers needs
    /// no write — the page store rebuilds it from the log — so eviction
    /// is an unmap. A victim above the durable LSN (or carrying unlogged
    /// changes) is skipped, never written anywhere: the pool applies
    /// backpressure instead (§12.5's open question).
    fn unmap_one_durable_victim(&self) -> bool {
        for (idx, frame) in self.frames.iter().enumerate() {
            {
                // Cheap peek: skip clean frames — the durable rung only
                // takes dirty ones (clean ones went to the rung above).
                let inner = frame.lock();
                if inner.page_id.is_none() || !inner.dirty {
                    continue;
                }
            }
            if !frame.try_claim() {
                continue;
            }
            // Re-verify under the claim: the metadata may have moved
            // between the unlocked peek and the claim.
            let page_id = {
                let inner = frame.lock();
                match inner.page_id {
                    Some(p)
                        if self.evictable(inner.scope, inner.dirty, inner.page_lsn) =>
                    {
                        p
                    }
                    _ => {
                        frame.release_claim();
                        continue;
                    }
                }
            };
            self.unmap_claimed(idx as u32, page_id);
            return true;
        }
        false
    }

    /// Unmaps a claimed frame and returns it to the free list.
    fn unmap_claimed(&self, idx: u32, page_id: PageId) {
        self.table.remove(&page_id, idx);
        {
            let mut inner = self.frames[idx as usize].lock();
            inner.page_id = None;
            inner.scope = None;
            inner.dirty = false;
            inner.page_lsn = None;
        }
        self.frames[idx as usize].release_claim();
        self.free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(idx);
    }

    /// Releases a loader's claim without publishing (publish-race loser).
    fn abandon_frame(&self, idx: u32) {
        let frame = &self.frames[idx as usize];
        {
            let mut inner = frame.lock();
            inner.page_id = None;
            inner.scope = None;
            inner.dirty = false;
            inner.page_lsn = None;
        }
        frame.release_claim();
        self.free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(idx);
    }

    /// Evict one page (named victim — the automatic selection is the
    /// loader's greedy path). The page is DROPPED, not written: an
    /// eviction of a page whose stamp is at or below the durable LSN loses
    /// nothing (§12.5), and no code path in this crate writes a page
    /// anywhere. `Ok(false)` = refused (pinned, a load in flight, above
    /// the durable LSN, or unlogged changes present — a claim requires
    /// the pin word fully zero); `Ok(true)` = the page is gone (or was
    /// never resident — idempotent).
    pub fn evict_page(&self, page_id: PageId) -> Result<bool, BpmError> {
        let Some(idx) = self.table.get(&page_id) else {
            return Ok(true);
        };
        let frame = &self.frames[idx as usize];
        if !frame.try_claim() {
            return Ok(false); // pinned, exclusively held, or loading
        }
        let evictable = {
            let inner = frame.lock();
            inner.page_id == Some(page_id)
                && self.evictable(inner.scope, inner.dirty, inner.page_lsn)
        };
        if !evictable {
            frame.release_claim();
            return Ok(false);
        }
        self.unmap_claimed(idx, page_id);
        Ok(true)
    }

    // --- pin helpers ----------------------------------------------------------------

    /// Hit path for readers: pin under the shard read lock (which blocks
    /// a concurrent unmap), snapshot, release the lock. `None` = not
    /// resident (the caller proceeds to its miss path).
    fn shared_hit(&self, page_id: PageId) -> Option<Result<PageGuard, BpmError>> {
        let shard = self.table.read_lock(&page_id);
        let idx = *shard.get(&page_id)?;
        let frame = &self.frames[idx as usize];
        if let Err(refusal) = frame.try_shared_pin() {
            return Some(Err(match refusal {
                PinRefusal::Busy => BpmError::Contended(page_id),
                PinRefusal::Overflow => BpmError::PinOverflow(page_id),
            }));
        }
        let snapshot = self.snapshot_frame(idx);
        self.accounting.on_pin(page_id.db, page_id);
        drop(shard);
        Some(Ok(PageGuard {
            bpm: self.clone_arc(),
            idx,
            page_id,
            snapshot,
        }))
    }

    /// Hit path for writers: the exclusive CAS under the shard read lock,
    /// same unmap-exclusion argument as [`Bpm::shared_hit`].
    fn exclusive_hit(&self, page_id: PageId) -> Option<Result<PageGuardMut, BpmError>> {
        let shard = self.table.read_lock(&page_id);
        let idx = *shard.get(&page_id)?;
        let frame = &self.frames[idx as usize];
        if !frame.try_exclusive_pin() {
            return Some(Err(BpmError::Contended(page_id)));
        }
        let snapshot = self.snapshot_frame(idx);
        self.accounting.on_pin(page_id.db, page_id);
        drop(shard);
        Some(Ok(PageGuardMut {
            bpm: self.clone_arc(),
            idx,
            page_id,
            snapshot,
        }))
    }

    // --- helpers -----------------------------------------------------------------

    /// Copies the frame's bytes out (8 KiB memcpy — the accepted cost).
    /// SAFETY (callers): `idx < max_frames` (indices come from the frame
    /// array), and no concurrent byte mutation — guaranteed either by
    /// holding the frame mutex, or by a pin word that excludes mutators
    /// (every byte mutation happens under EXCL or CLAIM; a successful
    /// shared/exclusive/claim pin and the mutators' word transitions are
    /// mutually exclusive single-word RMWs).
    fn snapshot_frame(&self, idx: u32) -> AlignedBuf {
        let mut buf = AlignedBuf::zeroed(PAGE_SIZE);
        // SAFETY: see above — idx is in range and mutators are excluded.
        unsafe {
            let src = std::slice::from_raw_parts(self.ram.frame_ptr(idx), PAGE_SIZE);
            buf.as_mut_slice().copy_from_slice(src);
        }
        buf
    }

    /// SAFETY (callers): as [`Bpm::snapshot_frame`], and the caller holds
    /// the frame's claim (loader fill) — claims exclude all pins.
    fn copy_into_frame(&self, idx: u32, data: &AlignedBuf) {
        debug_assert_eq!(data.as_slice().len(), PAGE_SIZE);
        // SAFETY: idx in range; the claim excludes concurrent access.
        unsafe {
            let dst = std::slice::from_raw_parts_mut(self.ram.frame_ptr(idx), PAGE_SIZE);
            dst.copy_from_slice(data.as_slice());
        }
    }

    // --- diagnostics -------------------------------------------------------------

    /// Live pin count for a page (`0` if not resident). Diagnostic — the
    /// value is only as fresh as the moment of the read.
    pub fn pin_count(&self, page_id: &PageId) -> u32 {
        self.table
            .get(page_id)
            .map(|idx| self.frames[idx as usize].pin_word() & PIN_COUNT_MASK)
            .unwrap_or(0)
    }

    /// Whether the resident image carries changes (`None` if not
    /// resident).
    pub fn is_dirty(&self, page_id: &PageId) -> Option<bool> {
        self.table
            .get(page_id)
            .map(|idx| self.frames[idx as usize].lock().dirty)
    }

    /// The end LSN of the last record applied to the resident image
    /// (S17): `Some(None)` = resident but never written, `None` = not
    /// resident.
    pub fn page_lsn(&self, page_id: &PageId) -> Option<Option<Lsn>> {
        self.table
            .get(page_id)
            .map(|idx| self.frames[idx as usize].lock().page_lsn)
    }

    /// Frames currently carrying a page.
    pub fn frames_in_use(&self) -> u32 {
        self.table.len() as u32
    }
}

// --- guards ---------------------------------------------------------------------------

/// RAII shared pin on one resident page (FR-23, invariant 5): released on
/// drop, including through a panic. Owns a copy-out snapshot taken at pin
/// time — reads see a coherent image regardless of concurrent writers, and
/// the guard borrows nothing from the pool (ownable across awaits and
/// threads).
#[must_use = "the pin lasts exactly as long as the guard is alive — bind it"]
pub struct PageGuard {
    bpm: Arc<Bpm>,
    idx: u32,
    page_id: PageId,
    snapshot: AlignedBuf,
}

impl PageGuard {
    /// The page this guard pins.
    pub fn page_id(&self) -> PageId {
        self.page_id
    }
}

impl std::ops::Deref for PageGuard {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.snapshot.as_slice()
    }
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        self.bpm.frames[self.idx as usize].release_shared();
        self.bpm.accounting.on_unpin(self.page_id.db, self.page_id);
    }
}

/// RAII exclusive pin (the writer guard): `page_exclusive`'s product. The
/// exclusive pin is held for the guard's whole life, so every mutation is
/// a short critical section and the pin protocol (not a lock held across
/// an await) is what other readers coordinate against.
#[must_use = "the exclusive pin lasts exactly as long as the guard is alive — bind it"]
pub struct PageGuardMut {
    bpm: Arc<Bpm>,
    idx: u32,
    page_id: PageId,
    snapshot: AlignedBuf,
}

impl PageGuardMut {
    /// The page this guard pins.
    pub fn page_id(&self) -> PageId {
        self.page_id
    }

    /// Runs `f` with mutable access to the real page image, marking the
    /// page dirty. `f` is synchronous: the frame mutex is held exactly for
    /// the call and can never span an await (the I11 discipline by
    /// construction). The guard's snapshot is refreshed afterwards, so its
    /// own reads see its own writes.
    ///
    /// Unlogged modification is the caller's contract (like PG's unlogged
    /// tables): the image keeps the change, but the page store cannot
    /// rebuild it, so this page will refuse eviction until it is dropped
    /// by an explicit decision of the layer above (a temp store, later).
    pub fn write<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let frame = &self.bpm.frames[self.idx as usize];
        let mut inner = frame.lock();
        inner.dirty = true;
        // SAFETY: idx < max_frames; the EXCL pin excludes every other
        // mutator, and the frame mutex excludes concurrent snapshots.
        let result = unsafe {
            let bytes =
                std::slice::from_raw_parts_mut(self.bpm.ram.frame_ptr(self.idx), PAGE_SIZE);
            f(bytes)
        };
        self.snapshot = self.bpm.snapshot_frame(self.idx);
        result
    }

    /// The logged form of [`write`](PageGuardMut::write): the caller
    /// passes the WAL end boundary covering this modification — reserved
    /// *before* this call, so a crash can never leave a page stamped with
    /// a record that does not exist. The stamp is the page's LSN (S17:
    /// the end of the last record applied); the §12.5 eviction rule reads
    /// it against the scope's durable LSN, and the page store — replaying
    /// the same log — materializes the same image independently. The
    /// caller's duty (the mini-transaction rule, §12.5): the group's
    /// bytes are in the log *before or with* this apply reaching the
    /// frame — the log is the only durable write.
    pub fn write_logged<R>(&mut self, wal_end: Lsn, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let frame = &self.bpm.frames[self.idx as usize];
        let mut inner = frame.lock();
        inner.dirty = true;
        // LSN 0 is the not-stamped sentinel (a reserved record precedes
        // every real stamp, so boundaries are ≥ 1).
        if wal_end.0 != 0 {
            inner.page_lsn = Some(wal_end);
        }
        // SAFETY: as `write`.
        let result = unsafe {
            let bytes =
                std::slice::from_raw_parts_mut(self.bpm.ram.frame_ptr(self.idx), PAGE_SIZE);
            f(bytes)
        };
        self.snapshot = self.bpm.snapshot_frame(self.idx);
        result
    }
}

impl std::ops::Deref for PageGuardMut {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.snapshot.as_slice()
    }
}

impl Drop for PageGuardMut {
    fn drop(&mut self) {
        self.bpm.frames[self.idx as usize].release_exclusive();
        self.bpm.accounting.on_unpin(self.page_id.db, self.page_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_types::SegmentId;

    #[test]
    fn accounting_tracks_live_pins_per_db() {
        let acc = PerDbAccounting::default();
        let db = DatabaseId(1);
        let page = PageId {
            db,
            segment: SegmentId(0),
            page_no: 0,
        };
        assert_eq!(acc.pinned(db), 0);
        acc.on_pin(db, page);
        acc.on_pin(db, page);
        assert_eq!(acc.pinned(db), 2);
        acc.on_unpin(db, page);
        assert_eq!(acc.pinned(db), 1);
        acc.on_unpin(db, page);
        acc.on_unpin(db, page); // saturating: no wrap on stray unpin
        assert_eq!(acc.pinned(db), 0);
    }

    /// The pool is Send + Sync — the whole point of T6.2. (The M1 slice
    /// asserted the negative.)
    #[test]
    fn pool_is_cross_core() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Bpm>();
        assert_send_sync::<PageGuard>();
        assert_send_sync::<PageGuardMut>();
        assert_send_sync::<PerDbAccounting>();
    }

    /// The placement hash: golden literals computed independently of the
    /// crate (FNV-1a over the 12 little-endian key bytes) — the SAME
    /// literals the runtime's pools.rs parity test pins. A refactored hash
    /// or a changed key-field order breaks this, which is the point: a
    /// page's pool may never move across restarts.
    #[test]
    fn home_numa_matches_the_frozen_goldens() {
        let key = |db: u32, seg: u32, no: u32| PageId {
            db: DatabaseId(db),
            segment: SegmentId(seg),
            page_no: no,
        };
        assert_eq!(home_numa(&key(1, 0, 0), 3), 2);
        assert_eq!(home_numa(&key(1, 0, 1), 3), 1);
        assert_eq!(home_numa(&key(12345, 678, 9), 3), 1);
        // Every key field matters (I3): the triple, not a path.
        let base = page_hash(&key(7, 1, 2));
        assert_ne!(page_hash(&key(8, 1, 2)), base);
        assert_ne!(page_hash(&key(7, 2, 2)), base);
        assert_ne!(page_hash(&key(7, 1, 3)), base);
    }
}
