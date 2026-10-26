//! The perf demo's read substrate: `demo(id INT, payload TEXT)`, a
//! read-only heap of fixed-width rows living in the §10.1 step-5 NUMA
//! pools — populated once at boot, then only read.
//!
//! ## Placement is the point
//!
//! Every row's page goes through the same frozen placement contract as
//! any page in the system (T2.5): `page_hash(PageId) mod num_numas` picks
//! the pool, whose memory the kernel `mbind`ed to that node (T2.3 — or
//! first-touch placed, honestly reported, where the kernel refuses). A
//! point lookup is therefore *pure arithmetic*: `id → (page, slot) →
//! pool-resident address → decode`. No btree descent, no planner, no
//! disk — the demo compares PG's parse+plan+btree+heap path against this
//! substrate with the parse already identical (ADR 0002) and the rest
//! named precisely in the demo write-up.
//!
//! ## Slice scope, stated
//!
//! - One relation, fixed width, no MVCC, no updates: the demo's read path
//!   only. The real heap access method (`cascade-heap`) arrives with
//!   Phase 3; this module is the measurement instrument, not the engine.
//! - Page `p` of the relation occupies frame `p` of *whichever pool the
//!   placement hash picks* (frames 0..pages of every pool are reserved
//!   for it — nothing else lives in the pools yet, and the reservation
//!   is checked at populate time).
//! - Row layout: `[id: i32 LE][pad 4][payload: 120 bytes, NUL-padded]`,
//!   128 bytes/row, 64 rows per 8-KiB page. Ids are 1-based and dense.
//!
//! ## Thread discipline
//!
//! The heap is populated on the boot thread and immutable afterwards.
//! [`DemoHeap`] holds raw page addresses *into the registry's mappings*,
//! which live as long as the [`Runtime`](crate::Runtime) that owns them —
//! the same lifetime discipline as the pools themselves. It is
//! `Send + Sync` under exactly that discipline: shared read-only slices
//! into memory that is never freed while sessions can run.

use std::sync::Arc;

use cascade_types::{DatabaseId, PageId, SegmentId, PAGE_SIZE};

use crate::pools::PoolRegistry;

/// One row's footprint (id + padding + payload).
pub const ROW_BYTES: usize = 128;
/// Payload bytes per row (NUL-padded text).
const PAYLOAD_BYTES: usize = ROW_BYTES - 8;
/// Rows per 8-KiB page.
pub const ROWS_PER_PAGE: u32 = (PAGE_SIZE as u32) / ROW_BYTES as u32;
/// The demo relation's segment id in the placement key (arbitrary but
/// frozen — the hash must be stable across boots).
pub const DEMO_SEGMENT: SegmentId = SegmentId(0xDA7A); // 0xDA7A ≈ "DATA"

/// The heap handle: row count plus each page's placement.
pub struct DemoHeap {
    rows: u32,
    /// Page `p`'s first byte, inside the pool the hash chose for it.
    pages: Vec<usize>,
}

// SAFETY (discipline, documented in the module doc): the addresses point
// into PoolRegistry mappings that live as long as the Runtime; the heap
// is written once at boot and read-only after. Sharing across reactor
// threads is read-only access to stable memory.
unsafe impl Send for DemoHeap {}
unsafe impl Sync for DemoHeap {}

impl DemoHeap {
    /// How many pages `rows` needs.
    pub fn pages_for(rows: u32) -> u32 {
        rows.div_ceil(ROWS_PER_PAGE)
    }

    /// The payload text for row `id` — the demo's generator is part of
    /// the fixture (pgbench sees identical text from both sides).
    pub fn payload_for(id: u32) -> String {
        format!("payload-{id:08}")
    }

    /// Writes `rows` rows into the pools, page by page, each page placed
    /// by the frozen hash. Fails when the pools cannot hold the relation
    /// (config sizing problem, reported at boot rather than truncated).
    pub fn populate(
        rows: u32,
        db: DatabaseId,
        pools: &mut PoolRegistry,
    ) -> Result<DemoHeap, usize> {
        let pages = Self::pages_for(rows);
        // Frames 0..pages of every pool are the relation's (slice scope —
        // see the module doc). The tightest pool bound is the check.
        let frame_budget = pools.iter().map(|p| p.frames()).min().unwrap_or(0);
        if pages > frame_budget {
            return Err(frame_budget as usize);
        }

        let mut heap = DemoHeap { rows, pages: Vec::with_capacity(pages as usize) };
        let mut page_img = vec![0u8; PAGE_SIZE];
        for page_no in 0..pages {
            // Placement: the same hash any page of this relation would
            // get, forever (frozen algorithm, frozen segment id).
            let page_id = PageId { db, segment: DEMO_SEGMENT, page_no };
            let numa = pools
                .pool_for_page(&page_id)
                .expect("validated config pools every page")
                .numa();
            let pool = pools
                .pool_for_numa_mut(numa)
                .expect("the same registry holds the pool");
            for slot in 0..ROWS_PER_PAGE {
                let id = page_no * ROWS_PER_PAGE + slot + 1; // 1-based, dense
                if id > rows {
                    break;
                }
                let off = (slot as usize) * ROW_BYTES;
                page_img[off..off + 4].copy_from_slice(&id.to_le_bytes());
                let payload = Self::payload_for(id);
                let bytes = payload.as_bytes();
                page_img[off + 8..off + 8 + bytes.len()].copy_from_slice(bytes);
            }
            pool.memory_mut().write_frame(page_no, &page_img);
            heap.pages.push(pool.memory().addr() + (page_no as usize) * PAGE_SIZE);
            page_img.fill(0);
        }
        Ok(heap)
    }

    /// Row count.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// The raw row bytes for `id` (1-based), or `None` past the end.
    fn row_bytes(&self, id: u32) -> Option<&[u8; ROW_BYTES]> {
        if id == 0 || id > self.rows {
            return None;
        }
        let page = ((id - 1) / ROWS_PER_PAGE) as usize;
        let slot = ((id - 1) % ROWS_PER_PAGE) as usize;
        let addr = self.pages[page] + slot * ROW_BYTES;
        // SAFETY: `addr` is inside page `page` of a pool mapping that
        // outlives this borrow (module-doc discipline); `slot` is in
        // range by construction.
        Some(unsafe { &*(addr as *const [u8; ROW_BYTES]) })
    }

    /// A row's id and payload text. `None` past the populated range.
    pub fn row(&self, id: u32) -> Option<(i32, &str)> {
        let bytes = self.row_bytes(id)?;
        let stored = i32::from_le_bytes(bytes[0..4].try_into().expect("row layout"));
        debug_assert_eq!(stored, id as i32, "row slot must hold its own id");
        let payload = &bytes[8..8 + PAYLOAD_BYTES];
        let end = payload.iter().position(|&b| b == 0).unwrap_or(PAYLOAD_BYTES);
        Some((stored, std::str::from_utf8(&payload[..end]).expect("generated payload is ASCII")))
    }
}

/// `Arc` alias for the plumbing (workers clone per session).
pub type SharedDemoHeap = Arc<DemoHeap>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NumaPool, NumaWorkers};
    use crate::RuntimeConfig;

    /// A small two-NUMA layout so placement (and page→pool assignment)
    /// actually varies across pages.
    fn pools_2numa() -> PoolRegistry {
        let cfg = RuntimeConfig {
            housekeeper_cores: vec![0],
            worker_cores: vec![
                NumaWorkers { numa: 0, cores: vec![1, 2] },
                NumaWorkers { numa: 1, cores: vec![3] },
            ],
            numa_pools: vec![
                NumaPool { numa: 0, size_mb: 1 },
                NumaPool { numa: 1, size_mb: 1 },
            ],
            ..RuntimeConfig::default()
        };
        PoolRegistry::build(&cfg).expect("valid pools")
    }

    #[test]
    fn populate_then_point_read() {
        let mut pools = pools_2numa();
        // Enough rows to span many pages and cross the row-per-page
        // boundary at least twice.
        let rows = 3 * ROWS_PER_PAGE + 17;
        let heap = DemoHeap::populate(rows, DatabaseId(0), &mut pools).expect("fits");

        assert_eq!(heap.rows(), rows);
        // First, a page boundary, the last row.
        for id in [1, ROWS_PER_PAGE, ROWS_PER_PAGE + 1, rows] {
            let (rid, payload) = heap.row(id).expect("row present");
            assert_eq!(rid, id as i32, "slot holds its own id");
            assert_eq!(payload, DemoHeap::payload_for(id));
        }
        // Out of range: 0 and past the end.
        assert!(heap.row(0).is_none());
        assert!(heap.row(rows + 1).is_none());
        // Empty slots on the last (partial) page are not readable rows.
        assert!(heap.row(rows + 1).is_none());
    }

    #[test]
    fn pages_too_big_is_a_config_error() {
        let mut pools = pools_2numa();
        let budget = pools.iter().map(|p| p.frames()).min().unwrap();
        let err = match DemoHeap::populate(u32::MAX / 2, DatabaseId(0), &mut pools) {
            Err(err) => err,
            Ok(_) => panic!("oversized relation must not populate"),
        };
        assert_eq!(err, budget as usize, "reports the frame budget it hit");
    }

    #[test]
    fn pages_honor_the_placement_hash() {
        // The pages must live exactly where page_hash says — the demo
        // substrate's whole claim. Recompute the pool choice per page and
        // compare against where populate actually wrote (visible via the
        // read-back, which goes through the stored address).
        let mut pools = pools_2numa();
        let rows = ROWS_PER_PAGE * 5;
        let heap = DemoHeap::populate(rows, DatabaseId(0), &mut pools).expect("fits");
        for page_no in 0..DemoHeap::pages_for(rows) {
            let page_id = PageId { db: DatabaseId(0), segment: DEMO_SEGMENT, page_no };
            let pool = pools.pool_for_page(&page_id).expect("pooled");
            let expect_addr = pool.memory().addr() + (page_no as usize) * PAGE_SIZE;
            // First row of each page decodes through the stored address.
            let id = page_no * ROWS_PER_PAGE + 1;
            let got = heap.row_bytes(id).unwrap().as_ptr() as usize;
            let addr_ok = got / PAGE_SIZE == expect_addr / PAGE_SIZE && got % PAGE_SIZE == 0;
            assert!(addr_ok, "page {page_no} reads from its placed pool");
        }
    }
}
