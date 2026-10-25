//! Userspace Buffer Pool Manager.
//!
//! See `README.md` and `AI/CONTEXT.md`.
//!
//! ## M1 debug slice (T1.9): one pool, one reactor
//!
//! The production design (§8.1) is one BPM per NUMA shared by all cores on
//! that NUMA, with a concurrent page table and sharded eviction. This slice
//! lands the single-pool case of FR-22 on one reactor — everything is
//! `!Send`: a fixed frame array, a `RefCell` page table, no eviction
//! (`PoolFull` instead, since eviction is deferred to M3), no cross-reactor
//! sharing. The *shapes* already match the production contract so the M3
//! generalization is additive: atomic pin count + dirty bit on every frame
//! (FR-23), RAII [`PageGuard`] that cannot outlive a pin even through a
//! panic, and all I/O through [`PageStore`] on the calling reactor's own
//! ring (invariant 4: the BPM owns no file handles).
//!
//! Lookup-vs-insert within one reactor: sibling tasks run at await points,
//! so the miss path re-checks the page table after its awaited read — a
//! page two tasks race to load is inserted exactly once and both pin the
//! same frame.

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use cascade_storage::PageStore;
use cascade_types::{DatabaseId, PageId};

/// Per-database page counters (§6.1): *live pin counts* per database,
/// incremented by [`Bpm::page`] and decremented by guard drops — real
/// accounting on actual pin/unpin (invariant 3), never sampled.
///
/// No enforcement in v1; governance (`cascade-governance`) consumes these.
#[derive(Debug, Default)]
pub struct PerDbAccounting {
    pins: RefCell<HashMap<DatabaseId, u64>>,
}

impl PerDbAccounting {
    pub fn on_pin(&self, db: DatabaseId, _page: PageId) {
        *self.pins.borrow_mut().entry(db).or_insert(0) += 1;
    }

    pub fn on_unpin(&self, db: DatabaseId, _page: PageId) {
        if let Some(pins) = self.pins.borrow_mut().get_mut(&db) {
            *pins = pins.saturating_sub(1);
        }
    }

    /// Live pins held by this pool for `db` right now.
    pub fn pinned(&self, db: DatabaseId) -> u64 {
        self.pins.borrow().get(&db).copied().unwrap_or(0)
    }
}

/// Pool sizing. Eviction (M3) will make `max_frames` a high-water mark;
/// today it is the pool's whole capacity.
#[derive(Debug, Clone, Copy)]
pub struct BpmConfig {
    pub max_frames: u32,
}

#[derive(Debug)]
pub enum BpmError {
    /// Every frame is in use and eviction does not exist yet (M3).
    PoolFull {
        max_frames: u32,
    },
    /// The frame's pin count would overflow — a pin leak at scale, not a
    /// transient condition.
    PinOverflow(PageId),
    Storage(cascade_storage::StorageError),
}

impl From<cascade_storage::StorageError> for BpmError {
    fn from(e: cascade_storage::StorageError) -> Self {
        BpmError::Storage(e)
    }
}

/// A resident page's payload.
struct SlotState {
    page_id: PageId,
    data: cascade_types::AlignedBuf,
}

/// One pool slot. `state` is `None` until filled; a filled slot is never
/// emptied in M1 (no eviction).
///
/// `UnsafeCell` because the pool hands out guards through `&Bpm` (slot
/// fill happens on the same thread before the slot becomes reachable via
/// the page table). The mutation soundness discipline is the *exclusive
/// pin*: `write()` asserts `pin == 1`, and every read slice borrows a
/// guard, so an exclusive pin means no other reference into the page is
/// live (multiple guards ⇒ pin ≥ 2). Per-page latching replaces this debug
/// contract when the pool goes cross-core (§8.1).
struct Frame {
    state: UnsafeCell<Option<SlotState>>,
    /// FR-23: atomic pin count + dirty bit — atomics even though this slice
    /// is single-reactor, so the M3 cross-core pool keeps the frame shape.
    pin: AtomicU32,
    dirty: AtomicBool,
}

impl Frame {
    fn empty() -> Self {
        Frame {
            state: UnsafeCell::new(None),
            pin: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
        }
    }

    /// SAFETY: the slot is filled exactly once, before the page table maps
    /// to it (single-reactor, no await between fill and map).
    unsafe fn state(&self) -> &SlotState {
        (*self.state.get()).as_ref().expect("slot is filled")
    }

    fn pin_count(&self) -> u32 {
        self.pin.load(Ordering::Relaxed)
    }

    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }
}

/// One buffer pool (the single-NUMA case of FR-22). Owns a [`PageStore`] and
/// is itself reactor-bound: `Bpm<CoreStorage>` is `!Send + !Sync`.
pub struct Bpm<S: PageStore> {
    store: S,
    config: BpmConfig,
    /// Fixed slot array — frame addresses are stable for the pool's life,
    /// which is what lets [`PageGuard`] hand out plain slices.
    frames: Box<[Frame]>,
    /// Bump allocation: the next never-used slot. No free list in M1 —
    /// nothing is ever evicted.
    next_slot: Cell<u32>,
    table: RefCell<HashMap<PageId, u32>>,
    accounting: PerDbAccounting,
}

impl<S: PageStore> Bpm<S> {
    /// Pre-allocates `config.max_frames` empty slots (page memory is the
    /// pool's RAM — "backed by pre-allocated Buffer Pool RAM").
    pub fn new(store: S, config: BpmConfig, accounting: PerDbAccounting) -> Self {
        let frames = (0..config.max_frames)
            .map(|_| Frame::empty())
            .collect::<Vec<_>>();
        Bpm {
            store,
            config,
            frames: frames.into_boxed_slice(),
            next_slot: Cell::new(0),
            table: RefCell::new(HashMap::new()),
            accounting,
        }
    }

    pub fn accounting(&self) -> &PerDbAccounting {
        &self.accounting
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// FR-23: hit → pin; miss → read through this reactor's own store on
    /// its own ring → insert → pin. The returned guard unpins on drop —
    /// including through a panic.
    pub async fn page(&self, page_id: PageId) -> Result<PageGuard<'_, S>, BpmError> {
        if let Some(&idx) = self.table.borrow().get(&page_id) {
            return self.pin_slot(page_id, idx);
        }

        // Miss. Fast capacity reject (the authoritative check is at insert
        // — the pool may fill while we read).
        if self.next_slot.get() >= self.config.max_frames {
            return Err(BpmError::PoolFull {
                max_frames: self.config.max_frames,
            });
        }

        // No borrow is held across the read: sibling tasks on this reactor
        // run at the await point.
        let data = self.store.read_page(page_id).await?;

        // Re-check: a sibling may have loaded the same page while we read.
        // Pages are inserted exactly once per pool.
        if let Some(&idx) = self.table.borrow().get(&page_id) {
            return self.pin_slot(page_id, idx);
        }

        let slot = self.next_slot.get();
        if slot >= self.config.max_frames {
            return Err(BpmError::PoolFull {
                max_frames: self.config.max_frames,
            });
        }
        // SAFETY: an empty slot is filled exactly once, here, before the
        // page table makes it reachable; single-reactor, and no await
        // between this write and the insert below.
        unsafe {
            *self.frames[slot as usize].state.get() = Some(SlotState { page_id, data });
        }
        self.next_slot.set(slot + 1);
        self.table.borrow_mut().insert(page_id, slot);
        self.pin_slot(page_id, slot)
    }

    /// Pins a resident slot (overflow-checked) and hands out the guard.
    fn pin_slot(&self, page_id: PageId, idx: u32) -> Result<PageGuard<'_, S>, BpmError> {
        let old = self.frames[idx as usize]
            .pin
            .fetch_add(1, Ordering::Relaxed);
        if old == u32::MAX {
            // Revert before reporting — the frame must not stay pinned.
            self.frames[idx as usize]
                .pin
                .fetch_sub(1, Ordering::Relaxed);
            return Err(BpmError::PinOverflow(page_id));
        }
        self.accounting.on_pin(page_id.db, page_id);
        Ok(PageGuard { bpm: self, idx })
    }

    /// Live pin count for a page (`0` if not resident). Diagnostics/tests.
    pub fn pin_count(&self, page_id: &PageId) -> u32 {
        self.table
            .borrow()
            .get(page_id)
            .map(|&idx| self.frames[idx as usize].pin_count())
            .unwrap_or(0)
    }

    /// Dirty bit for a resident page (`None` if not resident). Nothing
    /// flushes in M1 — the bit is the writeback gate's input (M3).
    pub fn is_dirty(&self, page_id: &PageId) -> Option<bool> {
        self.table
            .borrow()
            .get(page_id)
            .map(|&idx| self.frames[idx as usize].is_dirty())
    }

    /// Slots carrying a page right now (never decreases in M1).
    pub fn frames_in_use(&self) -> u32 {
        self.next_slot.get()
    }

    fn release(&self, idx: u32, page_id: PageId) {
        // Underflow is impossible by construction — a guard exists only
        // because pin_slot incremented — but a wrap here would silently
        // corrupt pin accounting, so it is defended rather than assumed.
        if self.frames[idx as usize]
            .pin
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| p.checked_sub(1))
            .is_err()
        {
            cascade_telemetry::log(
                cascade_telemetry::Level::Warn,
                "bpm_pin_underflow",
                &[("page", format!("{page_id:?}").as_str())],
            );
        }
        self.accounting.on_unpin(page_id.db, page_id);
    }
}

/// RAII pin on one resident page (FR-23, invariant 5): the pin lasts
/// exactly as long as the guard — dropped normally or unwound through a
/// panic, it is released. Reads go through `Deref`; writes go through
/// [`write`](PageGuard::write), which requires the *exclusive* pin and
/// marks the page dirty. (The writeback gate itself is M3's
/// WAL-before-data rule; today the bit is observable, nothing flushes.)
#[must_use = "the pin lasts exactly as long as the guard is alive — bind it"]
pub struct PageGuard<'a, S: PageStore> {
    bpm: &'a Bpm<S>,
    idx: u32,
}

impl<S: PageStore> PageGuard<'_, S> {
    fn frame(&self) -> &Frame {
        &self.bpm.frames[self.idx as usize]
    }

    /// The page this guard pins.
    pub fn page_id(&self) -> PageId {
        // SAFETY: a guard exists only over a filled slot.
        unsafe { self.frame().state() }.page_id
    }

    /// Runs `f` with mutable access to the page image, marking the page
    /// dirty. Requires the exclusive pin — with any second guard alive
    /// (pin ≥ 2) this refuses, which is what makes the shared-`&Bpm`
    /// mutation sound (see [`Frame`]).
    ///
    /// The `&mut` is confined to `f`, a synchronous closure: it can never
    /// be held across an await point, where a sibling task could pin the
    /// same page and read it. The scoping, not just the assertion, carries
    /// the soundness argument — a returned `&mut [u8]` could outlive the
    /// moment the pin check was true.
    pub fn write<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        assert_eq!(
            self.frame().pin_count(),
            1,
            "page writes require the exclusive pin"
        );
        self.frame().dirty.store(true, Ordering::Relaxed);
        // SAFETY: the exclusive-pin assertion proves no other reference
        // into this page is live (every read slice borrows a guard; no
        // second guard exists), and `f` is sync — the borrow cannot escape
        // to a point where a second pin could be taken.
        let data = unsafe { &mut (*self.frame().state.get()).as_mut().expect("filled").data };
        f(data.as_mut_slice())
    }
}

impl<S: PageStore> std::ops::Deref for PageGuard<'_, S> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: shared read of a filled slot — overlapping shared
        // borrows are fine, and writers are excluded by the exclusive-pin
        // contract (see [`Frame`]).
        unsafe { self.frame().state() }.data.as_slice()
    }
}

impl<S: PageStore> Drop for PageGuard<'_, S> {
    fn drop(&mut self) {
        let page_id = self.page_id();
        self.bpm.release(self.idx, page_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_tracks_live_pins_per_db() {
        let acc = PerDbAccounting::default();
        let db = DatabaseId(1);
        let page = PageId {
            db,
            segment: cascade_types::SegmentId(0),
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
}
