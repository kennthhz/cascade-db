//! The redo contract (storage-layer-design.md §12.4): the apply functions
//! shared by leaders, followers and page stores. The contract is a one-way
//! door with four laws:
//!
//! 1. **Self-contained per page.** A record applies using only the page
//!    image it is handed and its own bytes — no other pages, no engine
//!    state, no clocks, no randomness.
//! 2. **Deterministic.** The same page image plus the same record always
//!    produce the same bytes.
//! 3. **Versioned.** Record types are the frozen `cascade-wal-format` type
//!    bytes; the page format is `cascade-page`'s locked layout. Every
//!    component keeps applying every version it has ever accepted.
//! 4. **Idempotent by LSN (S17).** A record applies only if its start LSN
//!    is at or above the page LSN, which is the END LSN of the last record
//!    applied. Anything below is already reflected — [`ApplyOutcome::Skipped`],
//!    before the payload is even validated (an already-applied record
//!    cannot hurt the page, whatever its bytes say now).
//!
//! ## M7 scope: the heap-insert family
//!
//! The log carries [`WalRecord::HeapInsert`] (type 5): `[segment u32][cell
//! bytes verbatim as cascade-page lays them down]`. It names a *segment*,
//! not a page — the type-5 payload is frozen, so no retrofit can add a
//! page number. Routing is therefore fixed by law here, in one place:
//!
//! **The sequential-fill rule.** A segment's inserts fill page 0, then
//! page 1, then page 2, … each page exactly as full as [`cascade_page`]'s
//! append arithmetic allows, spilling whole cells (a cell never splits).
//! Given LSN order, the rule assigns every insert of a segment to exactly
//! one page — which is what lets a page store route records to pages, let
//! replay-from-empty reproduce every page byte for byte (the §10
//! differential test), and let recovery resume a cursor from a
//! materialized page's header.
//!
//! The rule binds the heap access method too (heap design H9, decided
//! 2026-11-26): the heap places every insert where the fill cursor says,
//! advancing the cursor in LSN order, so that the leader's live layout is
//! the replayed layout. That matters because the page store serves an
//! evicted page back to the leader in replayed layout — a leader that laid
//! rows out differently would find them moved.
//!
//! Two operations, for the two kinds of caller:
//!
//! * [`SegmentFill::route`] **advances** the cursor. It is the authority
//!   for an engine that keeps a cursor across records and calls it once
//!   per applied insert.
//! * [`SegmentFill::would_route`] only answers. It is for a caller that
//!   holds no cursor and derives one from the page it is about to append
//!   to — the page store, for which the open page's header *is* the state.
//!
//! Three answers for everything else, and the difference between them is
//! deliberate:
//!
//! * **Page-neutral by nature** — BEGIN, COMMIT, ABORT, the closed-timestamp
//!   heartbeat, the HLC high-water record — come back
//!   [`ApplyOutcome::NotAPageRecord`] and change nothing: only their LSN
//!   advance matters to ingest.
//! * [`WalRecord::LegacyInsert`] (type 4) refuses loudly: its row image
//!   predates the page format, and the mapping into cells is the heap row
//!   codec's (cascade-heap, unbuilt). Replay of pre-T1.3 logs into *pages*
//!   lands with that codec; the runtime's own recovery keeps handling
//!   legacy records for the debug substrate.
//! * The reserved families **bulk load (type 8)** and **unlogged marker
//!   (type 9)** refuse too. Their payload shapes do not exist yet, and
//!   they are not page-neutral by nature — a bulk load puts data on pages.
//!   Guessing that they touch nothing would let a store advance past a page
//!   change it never applied.

use cascade_page::{
    parse_cell, is_initialized, PageError, PageMut, PageView, CELL_FIXED, HEADER_LEN, SLOTS_BASE,
};
use cascade_types::{Lsn, PageNo, PAGE_SIZE};
use cascade_wal_format::{encoded_len, WalRecord};
use thiserror::Error;

/// Why an apply refused. Every variant names a law or a format rule — a
/// redo error is never "something went wrong", it is always *what*.
#[derive(Debug, Error)]
pub enum RedoError {
    /// The record's cell bytes are not a valid `cascade-page` cell (only a
    /// buggy encoder can produce this; WAL CRC already passed).
    #[error("redo record carries a cell that fails cascade-page's parse")]
    CorruptCell,
    /// The cell cannot fit even a fresh, empty page — the log carries a row
    /// no page of this format can ever hold. Producers must gate payloads
    /// at `MAX_CELL_LEN - CELL_FIXED`; the WAL frame's u16 ceiling
    /// is wider than that, so the gate is the producer's duty and this
    /// refusal is the loud backstop.
    #[error("cell of {0} bytes exceeds what any empty page can hold ({max})",
            max = MAX_CELL_LEN)]
    Unplaceable(usize),
    /// The page image refused `cascade-page`'s own validation — corruption
    /// reached the applier.
    #[error("page image refused: {0}")]
    Page(#[from] PageError),
    /// A record family whose page apply does not exist yet. Refusing loudly
    /// beats guessing (extend, never renumber — new families land here as
    /// their features do).
    #[error("no apply for this record family yet: {0}")]
    UnsupportedRecord(&'static str),
}

/// What one [`apply_record`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The record's change is in the page; the page LSN now holds `end`.
    Applied {
        /// END LSN of the record (its start plus its encoded length) — the
        /// value stamped into the page header (S17: the page LSN is an end).
        end: Lsn,
    },
    /// The record belongs to this segment but not to this page: the page is
    /// as full as the format allows. The page is untouched; per the
    /// sequential-fill rule the caller retries on the next page, which is
    /// fresh (or the caller asks [`SegmentFill::would_route`] where to go).
    SpillRequired,
    /// The record's start LSN is below the page LSN — already applied
    /// (S17). The payload was not validated or examined.
    Skipped,
    /// The record touches no page (txn markers, heartbeats, reserved
    /// families). Nothing changed; ingest still advances its LSN.
    NotAPageRecord,
}

/// Applies one record to one page image — the whole redo contract in one
/// function. `start` is the record's start LSN (its first byte's offset in
/// the scope's log).
///
/// The heap-insert path validates the cell structurally ([`parse_cell`]),
/// checks the page's free space with exactly `cascade-page`'s append
/// arithmetic, appends the cell verbatim (same bytes, same words), and
/// stamps the header's page LSN to the record's end. Fresh pages are
/// initialized in place; already-initialized pages go through
/// [`PageMut::wrap`], so a corrupt base image is refused here rather than
/// silently extended.
pub fn apply_record(
    page: &mut [u8; PAGE_SIZE],
    rec: &WalRecord,
    start: Lsn,
) -> Result<ApplyOutcome, RedoError> {
    match rec {
        WalRecord::HeapInsert { cell, .. } => {
            // S17 first, before any payload work: a record below the page
            // LSN is already reflected in this image. (Only a page record
            // can be "already applied": a record that touches no page is
            // inert whatever its LSN, and is reported as such below.)
            if start.0 < rd_u64(page, 8) {
                return Ok(ApplyOutcome::Skipped);
            }
            // Structural validation is parse_cell's (the WAL carries the
            // cell verbatim; its words are opaque here).
            let parsed = parse_cell(cell).map_err(|_| RedoError::CorruptCell)?;
            let cell_len = cell.len();
            if cell_len > MAX_CELL_LEN {
                return Err(RedoError::Unplaceable(cell_len));
            }
            // The fits check mirrors PageMut::append_cell's arithmetic
            // exactly (the law test pins the two together): the cell lands
            // at `free_offset - cell_len`, and that must not collide with
            // the slot array's next entry: its 2 bytes count, because this
            // append writes it (cascade-page's rule counts it too).
            //
            // A never-written page is an empty one. Its header is all
            // zeros, which must not be read as "no free space". An
            // initialized page is validated *before* its header is
            // reasoned about: a damaged page must be refused here, not
            // have its impossible header read as "full".
            let initialized = is_initialized(page);
            let (slot_count, free_offset) = if initialized {
                PageView::parse(page)?;
                (rd_u16(page, 4) as usize, rd_u16(page, 6) as usize)
            } else {
                (0, PAGE_SIZE)
            };
            let fits =
                free_offset >= cell_len
                    && free_offset - cell_len >= SLOTS_BASE + 2 * slot_count + 2;
            if !fits {
                return Ok(ApplyOutcome::SpillRequired);
            }
            let mut pm = if initialized {
                PageMut::wrap(page)?
            } else {
                PageMut::init(page)
            };
            pm.append_cell(parsed.xmin, parsed.payload)?;
            let end = Lsn(start.0 + encoded_len(rec) as u64);
            pm.set_page_lsn(end.0);
            Ok(ApplyOutcome::Applied { end })
        }
        // Legacy row images predate the page format; mapping them into
        // cells is cascade-heap's row codec (not built). Loud refusal.
        WalRecord::LegacyInsert { .. } => Err(RedoError::UnsupportedRecord(
            "LegacyInsert (type 4): page-level replay lands with cascade-heap's row codec",
        )),
        // Page-neutral by nature: transaction markers, and the scope-wide
        // housekeeping records.
        WalRecord::BeginTxn { .. }
        | WalRecord::CommitTxn { .. }
        | WalRecord::AbortTxn { .. }
        | WalRecord::ClosedTsHeartbeat { .. }
        | WalRecord::HlcHighWater { .. } => Ok(ApplyOutcome::NotAPageRecord),
        // Reserved families whose payload shapes do not exist yet, and
        // that are not page-neutral by nature: a bulk load puts data on
        // pages. Guessing that they touch nothing would let a store
        // advance past a page change it never applied, so they are refused
        // until their apply exists.
        WalRecord::BulkLoad => Err(RedoError::UnsupportedRecord(
            "BulkLoad (type 8): no page apply exists until its payload shape does",
        )),
        WalRecord::UnloggedMarker => Err(RedoError::UnsupportedRecord(
            "UnloggedMarker (type 9): no page apply exists until its payload shape does",
        )),
    }
}

/// The largest cell any page of this format can hold — a fresh page's whole
/// tail minus the 2-byte slot entry the cell's own append writes. Producers
/// gate here; [`apply_record`] refuses louder past it.
pub const MAX_CELL_LEN: usize = PAGE_SIZE - HEADER_LEN - 2;

/// The sequential-fill cursor (§12.4's routing law, see the module doc).
/// One per segment, held by whoever ingests that segment's records; a
/// value is a pure function of the insert sequence, which is what makes
/// routes reproducible across ingest, recovery and replay.
///
/// The free-space arithmetic is `cascade-page`'s append arithmetic with no
/// page bytes involved — only the open page's `slot_count` and
/// `free_offset` — so a cursor can resume from a materialized page's
/// header ([`SegmentFill::from_page_header`]) without reading any cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentFill {
    open_page: PageNo,
    slot_count: u16,
    free_offset: u16,
}

impl SegmentFill {
    /// A fresh segment: page 0, empty.
    pub fn start() -> Self {
        SegmentFill {
            open_page: 0,
            slot_count: 0,
            free_offset: PAGE_SIZE as u16,
        }
    }

    /// Resumes from a materialized page's header — the recovery shape. The
    /// caller passes the *open* page (the highest initialized page of the
    /// segment; sequential fill guarantees the initialized pages form a
    /// prefix) and its header's `slot_count` / `free_offset`.
    pub fn from_page_header(open_page: PageNo, slot_count: u16, free_offset: u16) -> Self {
        debug_assert!(slot_count as usize <= (PAGE_SIZE - SLOTS_BASE) / 2);
        debug_assert!(free_offset as usize <= PAGE_SIZE);
        SegmentFill {
            open_page,
            slot_count,
            free_offset,
        }
    }

    /// Where the next insert belongs — and the cursor moves there: past
    /// full pages, then the cell is appended to the page it lands on. This
    /// is the sequential-fill rule's single authority (heap design H9): an
    /// engine that keeps a cursor across records calls it once per applied
    /// insert, in LSN order, and afterwards the cursor equals that page's
    /// header. Its `free_offset` drops by the cell's bytes and its slot
    /// count grows by one: that is how the page format accounts for a cell
    /// — the slot entry's 2 bytes are the slot count's business, and
    /// [`SegmentFill::fits`] counts them.
    pub fn route(&mut self, cell_len: usize) -> Result<PageNo, RedoError> {
        let page = self.would_route(cell_len)?;
        while self.open_page < page {
            self.open_page = self.open_page.saturating_add(1);
            self.slot_count = 0;
            self.free_offset = PAGE_SIZE as u16;
        }
        self.slot_count += 1;
        self.free_offset -= cell_len as u16;
        Ok(page)
    }

    /// Where the next insert of `cell_len` bytes would belong: the open
    /// page, or the next one if the cell would not fit it (whole cells only
    /// — a cell never splits). A pure question: the cursor does not move.
    ///
    /// This is for a caller that holds no cursor of its own and derives one
    /// from the page it is about to append to — the page store asks this,
    /// then lets the page's own header be the state. A caller that *does*
    /// keep a cursor across records needs [`SegmentFill::route`]: asking
    /// this and never advancing sends every cell to the same page.
    pub fn would_route(&self, cell_len: usize) -> Result<PageNo, RedoError> {
        if cell_len > MAX_CELL_LEN {
            return Err(RedoError::Unplaceable(cell_len));
        }
        let mut probe = *self;
        while !fits(cell_len, probe.slot_count, probe.free_offset) {
            probe.open_page = probe.open_page.saturating_add(1);
            probe.slot_count = 0;
            probe.free_offset = PAGE_SIZE as u16;
        }
        Ok(probe.open_page)
    }

    /// The cursor's state as a page header would hold it — the inverse of
    /// [`SegmentFill::from_page_header`], for tests and assertions.
    pub fn as_header(&self) -> (PageNo, u16, u16) {
        (self.open_page, self.slot_count, self.free_offset)
    }
}

/// Mirrors `PageMut::append_cell`'s space rule: the cell sits at
/// `free_offset - cell_len`, which must stay at or above where the next
/// slot entry goes plus that entry's 2 bytes (the append writes it).
fn fits(cell_len: usize, slot_count: u16, free_offset: u16) -> bool {
    let fo = free_offset as usize;
    let sc = slot_count as usize;
    fo >= cell_len && fo - cell_len >= SLOTS_BASE + 2 * sc + 2
}

fn rd_u16(buf: &[u8; PAGE_SIZE], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn rd_u64(buf: &[u8; PAGE_SIZE], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

const _: () = assert!(CELL_FIXED == 26);

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_page::{encode_cell, NextWord};
    use cascade_types::{TxnId, VersionWord};

    fn cell(xid: u64, payload: &[u8]) -> Vec<u8> {
        encode_cell(VersionWord::uncommitted(TxnId(xid)), payload).expect("test cell fits")
    }

    fn ins(xid: u64, payload: &[u8]) -> WalRecord {
        WalRecord::HeapInsert {
            xid,
            segment: cascade_types::SegmentId(1),
            cell: cell(xid, payload),
        }
    }

    fn reference_page(inserts: &[(&WalRecord, Lsn)]) -> [u8; PAGE_SIZE] {
        // The reference construction: the format's own primitives, no redo
        // involved. Byte equality with apply_record's output is the law.
        let mut img = [0u8; PAGE_SIZE];
        let mut pm = PageMut::init(&mut img);
        for (rec, start) in inserts {
            if let WalRecord::HeapInsert { cell, .. } = rec {
                let p = parse_cell(cell).expect("reference parses");
                pm.append_cell(p.xmin, p.payload).expect("reference fits");
                pm.set_page_lsn(start.0 + encoded_len(rec) as u64);
            }
        }
        img
    }

    #[test]
    fn first_insert_initializes_and_matches_the_reference() {
        let mut img = [0u8; PAGE_SIZE];
        let rec = ins(7, b"row one");
        let out = apply_record(&mut img, &rec, Lsn(0x1000)).expect("applies");
        assert_eq!(
            out,
            ApplyOutcome::Applied {
                end: Lsn(0x1000 + encoded_len(&rec) as u64)
            }
        );
        let want = reference_page(&[(&rec, Lsn(0x1000))]);
        assert_eq!(img, want, "apply_record reproduces the reference bytes");
        let v = cascade_page::PageView::parse(&img).expect("image parses");
        assert_eq!(v.page_lsn(), 0x1000 + encoded_len(&rec) as u64);
        assert_eq!(v.slot_count(), 1);
    }

    #[test]
    fn applied_lsn_is_the_record_end_even_inside_a_group() {
        // The conformance suite pins this shape: an insert followed by a
        // COMMIT closes the group, and the page LSN is the *insert's* end —
        // the commit touches no page.
        let mut img = [0u8; PAGE_SIZE];
        let rec = ins(1, b"x");
        let ins_end = Lsn(encoded_len(&rec) as u64);
        assert_eq!(
            apply_record(&mut img, &rec, Lsn(0)).expect("applies"),
            ApplyOutcome::Applied { end: ins_end }
        );
        let commit = WalRecord::CommitTxn {
            xid: 1,
            commit_ts: cascade_types::Hlc(5),
        };
        assert_eq!(
            apply_record(&mut img, &commit, ins_end).expect("commit is inert"),
            ApplyOutcome::NotAPageRecord
        );
        assert_eq!(
            cascade_page::PageView::parse(&img).expect("parses").page_lsn(),
            ins_end.0,
            "the commit left the page LSN at the insert's end (S17)"
        );
    }

    #[test]
    fn idempotent_skip_is_below_page_lsn_and_skips_validation() {
        let mut img = [0u8; PAGE_SIZE];
        let rec = ins(1, b"once");
        let end = Lsn(encoded_len(&rec) as u64);
        apply_record(&mut img, &rec, Lsn(0)).expect("first apply");
        // At the boundary it applies again (S17: "at or above").
        assert_eq!(
            apply_record(&mut img, &rec, end).expect("equal LSN applies"),
            ApplyOutcome::Applied {
                end: Lsn(end.0 + encoded_len(&rec) as u64)
            }
        );
        // Strictly below: skipped, and the bytes are not even parsed — a
        // hand-mangled cell in an already-applied record cannot hurt.
        let mut stale = ins(1, b"stale");
        if let WalRecord::HeapInsert { cell, .. } = &mut stale {
            cell[..2].copy_from_slice(&999u16.to_le_bytes()); // corrupt cell
        }
        assert_eq!(
            apply_record(&mut img, &stale, Lsn(0)).expect("below page LSN skips"),
            ApplyOutcome::Skipped
        );
    }

    #[test]
    fn spill_leaves_the_page_byte_identical() {
        let mut img = [0u8; PAGE_SIZE];
        // Fill the page with zero-payload cells until it is exactly full
        // (291 cells, per cascade-page's boundary test), then ask for one
        // more.
        let fill = ins(1, b"");
        let step = encoded_len(&fill) as u64;
        let mut at = Lsn(0);
        for _ in 0..291 {
            let end = Lsn(at.0 + step);
            assert_eq!(
                apply_record(&mut img, &fill, at).expect("fill cell"),
                ApplyOutcome::Applied { end }
            );
            at = end;
        }
        let before = img;
        assert_eq!(
            apply_record(&mut img, &fill, at).expect("spill is an outcome, not an error"),
            ApplyOutcome::SpillRequired,
            "291 zero-payload cells leave 12 free bytes; a 28-byte cell (26 + its slot) no longer fits"
        );
        assert_eq!(img, before, "a spill must not touch the page");
    }

    #[test]
    fn spill_then_fresh_page_accepts_the_same_cell() {
        // The pagestore's retry contract: SpillRequired on page N, the same
        // record applies on a fresh page N+1.
        let mut full = [0u8; PAGE_SIZE];
        let fill = ins(1, b"");
        let step = encoded_len(&fill) as u64;
        let mut at = Lsn(0);
        while let ApplyOutcome::Applied { end } =
            apply_record(&mut full, &fill, at).expect("fill")
        {
            at = end;
        }
        let mut next = [0u8; PAGE_SIZE];
        assert_eq!(
            apply_record(&mut next, &fill, at).expect("fresh page takes it"),
            ApplyOutcome::Applied {
                end: Lsn(at.0 + step)
            }
        );
    }

    #[test]
    fn unplaceable_cell_refuses_even_on_a_fresh_page() {
        let mut img = [0u8; PAGE_SIZE];
        let huge = ins(1, &vec![b'x'; MAX_CELL_LEN - CELL_FIXED + 1]);
        assert!(matches!(
            apply_record(&mut img, &huge, Lsn(0)),
            Err(RedoError::Unplaceable(_))
        ));
        assert!(
            img == [0u8; PAGE_SIZE],
            "a refused record must not touch the page"
        );
    }

    #[test]
    fn corrupt_cell_refused_before_the_page_is_touched() {
        let mut img = [0u8; PAGE_SIZE];
        let mut bad = cell(1, b"row");
        bad[..2].copy_from_slice(&3u16.to_le_bytes()); // len prefix lies
        let rec = WalRecord::HeapInsert {
            xid: 1,
            segment: cascade_types::SegmentId(1),
            cell: bad,
        };
        assert!(matches!(
            apply_record(&mut img, &rec, Lsn(0)),
            Err(RedoError::CorruptCell)
        ));
        assert_eq!(img, [0u8; PAGE_SIZE]);
    }

    #[test]
    fn corrupt_base_image_refused_by_wrap() {
        // An initialized page with a mangled header must refuse, not extend.
        let mut img = reference_page(&[(&ins(1, b"a"), Lsn(10))]);
        img[6] = 0; // free_offset pulled under the slot array
        img[7] = 0;
        let rec = ins(2, b"b");
        assert!(matches!(
            apply_record(&mut img, &rec, Lsn(100)),
            Err(RedoError::Page(_))
        ));
    }

    /// The reserved families are not page-neutral by nature: a bulk load
    /// puts data on pages. Until their payload shapes exist they are
    /// refused — never reported as touching nothing, which would let a
    /// store advance past a page change it never applied.
    #[test]
    fn reserved_page_families_are_refused_not_inert() {
        let mut img = reference_page(&[(&ins(1, b"kept"), Lsn(10))]);
        let before = img;
        for rec in [WalRecord::BulkLoad, WalRecord::UnloggedMarker] {
            assert!(
                matches!(
                    apply_record(&mut img, &rec, Lsn(100)),
                    Err(RedoError::UnsupportedRecord(_))
                ),
                "{rec:?}"
            );
        }
        assert_eq!(img, before, "nothing changed");
    }

    #[test]
    fn legacy_insert_refuses_loudly() {
        let mut img = [0u8; PAGE_SIZE];
        let rec = WalRecord::LegacyInsert {
            xid: 1,
            table: cascade_types::SegmentId(1),
            id: 2,
            val: "v".into(),
        };
        assert!(matches!(
            apply_record(&mut img, &rec, Lsn(0)),
            Err(RedoError::UnsupportedRecord(_))
        ));
    }

    #[test]
    fn no_page_records_are_inert() {
        let mut img = reference_page(&[(&ins(1, b"kept"), Lsn(10))]);
        let before = img;
        for rec in [
            WalRecord::BeginTxn { xid: 2 },
            WalRecord::CommitTxn {
                xid: 2,
                commit_ts: cascade_types::Hlc(1),
            },
            WalRecord::AbortTxn { xid: 2 },
            WalRecord::ClosedTsHeartbeat { closed_ts: 1 },
            WalRecord::HlcHighWater { max_issued: 1 },
        ] {
            assert_eq!(
                apply_record(&mut img, &rec, Lsn(100)).expect("inert"),
                ApplyOutcome::NotAPageRecord,
                "{rec:?}"
            );
        }
        assert_eq!(img, before, "inert records change nothing");
    }

    /// The differential kernel: driving a random insert stream through
    /// `SegmentFill::route` + `apply_record` must produce exactly the pages
    /// a naive replay produces (route says page N; naive replay lands the
    /// same record on page N). This is the law the pagestore's routing and
    /// the §10 differential test both stand on.
    #[test]
    fn fill_cursor_routing_matches_naive_replay() {
        let mut s = 0x5EED_CAFE_F00D_F00Du64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut pages: Vec<[u8; PAGE_SIZE]> = Vec::new();
        let mut fill = SegmentFill::start();
        let mut at = Lsn(0);
        for i in 0..2000u64 {
            // Payload sizes that straddle the spill boundary often.
            let len = match next() % 4 {
                0 => (next() % 64) as usize,
                1 => (next() % 512) as usize,
                2 => (next() % 4000) as usize,
                _ => (next() % 7900) as usize,
            };
            let rec = ins(i, &vec![b'r'; len]);
            let cell_len = encoded_len(&rec) - cascade_wal_format::HEADER_LEN
                - cascade_wal_format::CRC_LEN
                - 4; // minus segment field: the cell itself
            // Routing says where it goes.
            let routed = fill.route(cell_len).expect("placeable");
            // Extend the naive replay to that page and apply.
            while pages.len() <= routed as usize {
                pages.push([0u8; PAGE_SIZE]);
            }
            let page = &mut pages[routed as usize];
            let out = apply_record(page, &rec, at).expect("routed page accepts");
            assert!(
                matches!(out, ApplyOutcome::Applied { .. }),
                "a routed record must fit its page; got {out:?}"
            );
            at = Lsn(at.0 + encoded_len(&rec) as u64);
        }
        // And the cursor's final header state equals the open page's header.
        let (open, slots, free) = fill.as_header();
        let last = &pages[open as usize];
        assert_eq!(rd_u16(last, 4), slots, "cursor slot_count == open page header");
        assert_eq!(rd_u16(last, 6), free, "cursor free_offset == open page header");
        assert!(rd_u64(last, 8) > 0, "open page holds records");
    }

    /// `would_route` answers "where would it go" and moves nothing; `route`
    /// answers and advances — the authority an engine calls once per
    /// applied insert (heap design H9). A driver that kept a cursor but
    /// only ever asked `would_route` would send every cell to the same
    /// page, because the cursor would never learn the page was filling —
    /// which is exactly what the kernel test above caught.
    #[test]
    fn would_route_only_answers_and_route_advances() {
        // `RedoError` wraps the page format's error, which is not
        // comparable: compare the answers, not the results.
        let ask = |r: Result<PageNo, RedoError>| r.expect("placeable");
        let cell = 3026usize; // a 3000-byte payload's cell
        let mut cursor = SegmentFill::start();
        assert_eq!(ask(cursor.would_route(cell)), 0);
        assert_eq!(cursor, SegmentFill::start(), "would_route moved nothing");
        assert_eq!(ask(cursor.route(cell)), 0, "the first cell joins page 0");
        assert_eq!(
            cursor.as_header(),
            (0, 1, (PAGE_SIZE - cell) as u16),
            "one more slot; the cell region starts where the cell begins"
        );
        assert_eq!(ask(cursor.route(cell)), 0);
        assert_eq!(
            ask(cursor.would_route(cell)),
            1,
            "the third does not fit page 0"
        );
        assert_eq!(
            cursor.as_header(),
            (0, 2, (PAGE_SIZE - 2 * cell) as u16),
            "…and would_route still left the cursor alone"
        );
        assert_eq!(ask(cursor.route(cell)), 1);
        assert_eq!(
            cursor.as_header(),
            (1, 1, (PAGE_SIZE - cell) as u16),
            "a fresh page 1 holding one cell"
        );
        let too_big = MAX_CELL_LEN + 1;
        assert!(
            matches!(cursor.would_route(too_big), Err(RedoError::Unplaceable(n)) if n == too_big)
        );
        assert!(matches!(cursor.route(too_big), Err(RedoError::Unplaceable(n)) if n == too_big));
        assert_eq!(
            cursor.as_header(),
            (1, 1, (PAGE_SIZE - cell) as u16),
            "a refused cell reserved nothing"
        );
    }

    /// The recovery law: a cursor resumed from a materialized open page's
    /// header routes the continuation exactly where an uninterrupted replay
    /// would have. This is the omit-rule's foundation — inserts below the
    /// open page's LSN are already in the image, and everything from there
    /// continues seamlessly.
    #[test]
    fn resumed_cursor_continues_an_uninterrupted_replay() {
        let mut s = 0x40FA_5EEDu64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        // Replay A: one uninterrupted stream over many pages.
        let mut pages_a: Vec<[u8; PAGE_SIZE]> = Vec::new();
        let mut fill_a = SegmentFill::start();
        let mut at = Lsn(0);
        let mut records: Vec<(WalRecord, Lsn)> = Vec::new();
        for i in 0..800u64 {
            let len = (next() % 3000) as usize;
            let rec = ins(i, &vec![b'k'; len]);
            let routed = fill_a
                .route(encoded_len(&rec) - cascade_wal_format::HEADER_LEN - cascade_wal_format::CRC_LEN - 4)
                .expect("placeable");
            while pages_a.len() <= routed as usize {
                pages_a.push([0u8; PAGE_SIZE]);
            }
            apply_record(&mut pages_a[routed as usize], &rec, at).expect("applies");
            let len = encoded_len(&rec) as u64;
            records.push((rec, at));
            at = Lsn(at.0 + len);
        }
        // "Materialize" through page K: recovery sees pages 0..=K on disk.
        let k = 3usize;
        let open = pages_a[k];
        let (slot_count, free_offset, open_lsn) = (rd_u16(&open, 4), rd_u16(&open, 6), rd_u64(&open, 8));
        // Replay B: pages 0..=K restored from disk, cursor resumed from the
        // open page's header, remaining records re-applied.
        let mut pages_b: Vec<[u8; PAGE_SIZE]> = pages_a[..=k].to_vec();
        let mut fill_b = SegmentFill::from_page_header(k as PageNo, slot_count, free_offset);
        for (rec, start) in records.iter().skip_while(|(_, start)| start.0 < open_lsn) {
            let routed = fill_b
                .route(encoded_len(rec) - cascade_wal_format::HEADER_LEN - cascade_wal_format::CRC_LEN - 4)
                .expect("placeable");
            while pages_b.len() <= routed as usize {
                pages_b.push([0u8; PAGE_SIZE]);
            }
            let out = apply_record(&mut pages_b[routed as usize], rec, *start).expect("applies");
            assert!(matches!(out, ApplyOutcome::Applied { .. }));
        }
        assert_eq!(pages_a, pages_b, "resume == uninterrupted replay");
        assert_eq!(fill_a, fill_b, "cursors converge");
    }

    /// `encode_cell`'s standalone bytes are what `append_cell` lays down
    /// (cascade-page pins this); apply_record rides that law — this pins
    /// the whole chain from a WAL cell to page bytes for one concrete
    /// record, including the untouched mid-page gap.
    #[test]
    fn cell_bytes_reach_the_page_verbatim() {
        let mut img = [0u8; PAGE_SIZE];
        let rec = ins(9, b"payload bytes");
        apply_record(&mut img, &rec, Lsn(0x2000)).expect("applies");
        let v = cascade_page::PageView::parse(&img).expect("parses");
        let c = v.cell(0).expect("slot 0");
        assert_eq!(c.xmin, VersionWord::uncommitted(TxnId(9)));
        assert_eq!(c.xmax, cascade_types::VersionWord::EMPTY);
        assert_eq!(c.next, NextWord::NONE);
        assert_eq!(c.payload, b"payload bytes");
    }
}
