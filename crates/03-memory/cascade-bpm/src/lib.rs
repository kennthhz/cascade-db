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
//!
//! ## T3.2: WAL-before-data writeback + eviction
//!
//! The writeback gate is this crate's half of the single most
//! load-bearing crash-safety invariant (wal invariant 1, FR-24): no dirty
//! page reaches the store before the WAL covering its latest modification
//! is durable. A logged write stamps the frame with the WAL end boundary
//! it was covered by ([`PageGuard::write_logged`]); [`Bpm::flush_page`]
//! waits on a [`WalDurability`] gate until that boundary is durable, and
//! only then writes — re-checking after every await, because a sibling
//! task may have re-modified the page (its LSN moved) or pinned it while
//! the gate wait parked. The gate is injected (`Bpm::set_gate`): the BPM
//! never depends on the WAL crate — the runtime glues the two together. A
//! stamped page with no gate attached refuses writeback outright; a dirty
//! page with no stamp is unlogged by construction (the caller's contract,
//! like PG's unlogged tables) and writes ungated. [`Bpm::evict_page`]
//! routes through the same gate, then reclaims the frame — eviction never
//! has a second writeback path.

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    collections::HashMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use cascade_storage::PageStore;
use cascade_types::{DatabaseId, Lsn, PageId};

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

/// Pool sizing. `max_frames` is the pool's whole capacity; frames freed by
/// [`Bpm::evict_page`] are reused, so it is also the high-water mark. The
/// automatic victim selection of §8.1 (sharded, hash-of-PageId) lands with
/// the concurrent BPM — today eviction names its target explicitly.
#[derive(Debug, Clone, Copy)]
pub struct BpmConfig {
    pub max_frames: u32,
}

#[derive(Debug)]
pub enum BpmError {
    /// Every frame is in use; the caller must evict (T3.2 gives it
    /// [`Bpm::evict_page`]) before loading more.
    PoolFull {
        max_frames: u32,
    },
    /// The frame's pin count would overflow — a pin leak at scale, not a
    /// transient condition.
    PinOverflow(PageId),
    /// The WAL-durability gate refused or failed — the writeback did not
    /// happen and the page stays dirty (WAL-before-data, FR-24).
    WalGate(String),
    Storage(cascade_storage::StorageError),
}

/// The WAL-durability oracle the writeback gate waits on (FR-24). The BPM
/// defines the contract and never depends on the WAL crate — the runtime
/// provides the impl over the per-DB `WalWriter`; tests provide fakes.
/// Object-safe by design (a pool holds `Rc<dyn WalDurability>`); reactor-
/// bound like everything else here, hence no `Send` bounds.
pub trait WalDurability {
    /// Highest durable LSN right now — the gate's fast path (skip the wait
    /// when coverage already holds).
    fn durable_lsn(&self) -> Lsn;

    /// Resolves when durability covers `lsn`. `Err` is a WAL failure: the
    /// writeback must not proceed, and the page stays dirty.
    fn wait_durable(
        &self,
        lsn: Lsn,
    ) -> Pin<Box<dyn Future<Output = Result<(), BpmError>> + '_>>;
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
    /// The WAL end boundary covering the page's latest logged modification
    /// (T3.2): writeback waits until the gate's durability reaches this.
    /// `None` = unlogged (fresh load, or only unlogged writes so far).
    page_lsn: Cell<Option<Lsn>>,
}

impl Frame {
    fn empty() -> Self {
        Frame {
            state: UnsafeCell::new(None),
            pin: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            page_lsn: Cell::new(None),
        }
    }

    /// SAFETY: the slot is filled exactly once, before the page table maps
    /// to it, and emptied only by [`Bpm::evict_page`] — which requires pin
    /// 0 (no guards alive) and unmaps first. Between those, every `&self`
    /// accessor here is reached only through a live guard.
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
    /// Bump allocation: the next never-used slot.
    next_slot: Cell<u32>,
    /// Slots freed by [`Bpm::evict_page`], claimed before the bump (T3.2).
    free: RefCell<Vec<u32>>,
    table: RefCell<HashMap<PageId, u32>>,
    /// The WAL-durability gate for writeback (T3.2). Injected, not
    /// constructed: the BPM never depends on the WAL crate.
    gate: RefCell<Option<Rc<dyn WalDurability>>>,
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
            free: RefCell::new(Vec::new()),
            table: RefCell::new(HashMap::new()),
            gate: RefCell::new(None),
            accounting,
        }
    }

    /// Attaches the WAL-durability gate (T3.2). Set once at boot, before
    /// any logged write can exist; overwriting is allowed for tests.
    pub fn set_gate(&self, gate: Rc<dyn WalDurability>) {
        *self.gate.borrow_mut() = Some(gate);
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

        // Miss. Fast capacity reject: no freed frame available and the
        // bump exhausted (the authoritative check is at insert — the pool
        // may fill while we read).
        if self.free.borrow().is_empty() && self.next_slot.get() >= self.config.max_frames {
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

        let slot = match self.free.borrow_mut().pop() {
            Some(freed) => freed,
            None => {
                let slot = self.next_slot.get();
                if slot >= self.config.max_frames {
                    return Err(BpmError::PoolFull {
                        max_frames: self.config.max_frames,
                    });
                }
                self.next_slot.set(slot + 1);
                slot
            }
        };
        // SAFETY: an empty slot is filled exactly once, here, before the
        // page table makes it reachable; single-reactor, and no await
        // between this write and the insert below. Freed slots are `None`
        // (eviction unmapped them while unpinned) and taken off the free
        // list exactly once.
        unsafe {
            *self.frames[slot as usize].state.get() = Some(SlotState { page_id, data });
        }
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

    /// Dirty bit for a resident page (`None` if not resident). The
    /// writeback gate's input (FR-24).
    pub fn is_dirty(&self, page_id: &PageId) -> Option<bool> {
        self.table
            .borrow()
            .get(page_id)
            .map(|&idx| self.frames[idx as usize].is_dirty())
    }

    /// The WAL boundary the page's latest logged modification was covered
    /// by (`None` = resident but unlogged, or not resident). Diagnostics
    /// and tests; the gate reads it through the frame directly.
    pub fn page_lsn(&self, page_id: &PageId) -> Option<Option<Lsn>> {
        self.table
            .borrow()
            .get(page_id)
            .map(|&idx| self.frames[idx as usize].page_lsn.get())
    }

    /// Writeback for one page (T3.2, FR-24): a dirty page is written
    /// through the store only after the WAL gate reports durability of the
    /// boundary its latest logged modification was stamped with — never
    /// before (wal invariant 1). Clean, absent, or pinned pages write
    /// nothing (`Ok(false)`); a written page clears its dirty bit unless
    /// the page was re-modified or re-pinned during the write's awaits, in
    /// which case the dirty bit stays (the newer image is still WAL-protected:
    /// its stamp only moved forward). Pinned pages are refused in this
    /// slice — the §8.1 in-pool sharded eviction handles pin pressure with
    /// the concurrent BPM.
    pub async fn flush_page(&self, page_id: PageId) -> Result<bool, BpmError> {
        let Some(&idx) = self.table.borrow().get(&page_id) else {
            return Ok(false);
        };
        if self.frames[idx as usize].pin_count() != 0 {
            return Ok(false);
        }
        // The gate loop: wait for the stamped boundary to become durable,
        // then re-check — a sibling task may have re-modified the page
        // while this wait parked (its stamp moved), or pinned it. Every
        // re-read happens fresh after the await; no borrow crosses one.
        let lsn = loop {
            let lsn = self.frames[idx as usize].page_lsn.get();
            if let Some(target) = lsn {
                let gate = self.gate.borrow().clone();
                let Some(gate) = gate else {
                    // A stamped page without a gate must never reach the
                    // store: fail loudly rather than write ahead of the WAL.
                    return Err(BpmError::WalGate(
                        "page carries a WAL boundary but no durability gate is attached".into(),
                    ));
                };
                if gate.durable_lsn() < target {
                    gate.wait_durable(target).await?;
                }
            }
            if self.frames[idx as usize].pin_count() != 0 {
                return Ok(false);
            }
            if self.frames[idx as usize].page_lsn.get() == lsn {
                break lsn;
            }
        };
        if !self.frames[idx as usize].is_dirty() {
            return Ok(false);
        }
        // Snapshot the image (sync — no await between the pin/Lsn re-checks
        // above and this copy), then write it.
        let snapshot = {
            // SAFETY: filled (mapped above), pin 0 checked, no guard alive.
            let data = unsafe { self.frames[idx as usize].state() }.data.as_slice();
            let mut buf = cascade_types::AlignedBuf::zeroed(data.len());
            buf.as_mut_slice().copy_from_slice(data);
            buf
        };
        self.store.write_page(page_id, snapshot).await?;
        // Clear dirty only if the page was untouched during the write's
        // awaits; a re-modified page keeps its dirty bit for the next pass.
        let frame = &self.frames[idx as usize];
        if frame.pin_count() == 0 && frame.page_lsn.get() == lsn {
            frame.dirty.store(false, Ordering::Relaxed);
        }
        Ok(true)
    }

    /// Evict one page (T3.2): dirty pages route through [`Bpm::flush_page`]
    /// — the same gate, so eviction cannot bypass WAL-before-data (the
    /// plan's "any second writeback path" review item) — then the frame is
    /// reclaimed for reuse. `Ok(false)` = refused (pinned); `Ok(true)` =
    /// the page is gone (or was never resident — idempotent). Automatic
    /// victim selection is the concurrent BPM's business (§8.1); this
    /// names its target.
    pub async fn evict_page(&self, page_id: PageId) -> Result<bool, BpmError> {
        if self.is_dirty(&page_id) == Some(true) {
            self.flush_page(page_id).await?;
        }
        // Post-await re-fetch: a sibling may have pinned or evicted during
        // the gate wait / store write above.
        let Some(&idx) = self.table.borrow().get(&page_id) else {
            return Ok(true);
        };
        if self.frames[idx as usize].pin_count() != 0 {
            return Ok(false);
        }
        self.table.borrow_mut().remove(&page_id);
        let frame = &self.frames[idx as usize];
        // SAFETY: pin 0 (checked) and unmapped (line above) — no guard can
        // name this frame, and single-reactor discipline means no task
        // between this write and the free-list push touches it.
        unsafe { *frame.state.get() = None };
        frame.page_lsn.set(None);
        frame.dirty.store(false, Ordering::Relaxed);
        self.free.borrow_mut().push(idx);
        Ok(true)
    }

    /// Slots carrying a page right now — claimed slots minus freed ones
    /// (eviction puts a frame on the free list; until it is claimed again
    /// it carries no page).
    pub fn frames_in_use(&self) -> u32 {
        self.next_slot.get() - self.free.borrow().len() as u32
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

    /// The logged form of [`write`](PageGuard::write) (T3.2, FR-24): the
    /// caller passes the WAL end boundary covering this modification —
    /// reserved *before* this call, so a crash mid-writeback protocol
    /// can never write a page whose record does not exist. Writeback
    /// ([`Bpm::flush_page`]) waits on the gate until durability reaches
    /// `wal_end`. Unlogged modification stays available via `write`.
    pub fn write_logged<R>(&mut self, wal_end: Lsn, f: impl FnOnce(&mut [u8]) -> R) -> R {
        assert_eq!(
            self.frame().pin_count(),
            1,
            "page writes require the exclusive pin"
        );
        self.frame().dirty.store(true, Ordering::Relaxed);
        self.frame().page_lsn.set(Some(wal_end));
        // SAFETY: identical scoping argument to `write`.
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
