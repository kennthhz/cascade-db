//! The seam between the page store and redo. The store owns everything
//! around the pages — ingest, the in-memory cache, the doublewrite batch,
//! the manifest — and is deliberately blind to what a page *means*. What a
//! record does to which page is [`RedoPlan`]'s, and the page store ships
//! one plan: [`HeapInsertRedo`], the heap-insert family over
//! `cascade-redo`.
//!
//! The seam exists for one reason: the contract crate's conformance suite
//! drives records whose cells are a test shape, not `cascade-page` cells
//! (§4.10 — `cascade-storage-api` never depends on `cascade-redo`). With
//! the plan injected, the *same* store code that serves real heap pages
//! runs that suite, so the suite exercises the store — catch-up, as-of
//! reads, `TooOld`, `BeyondDurable`, scope administration — and not a
//! parallel test implementation of it. Real redo has its own tests: the §10
//! differential and crash suites.

use cascade_page::{PageView, SLOTS_BASE};
use cascade_redo::{apply_record, ApplyOutcome, RedoError, SegmentFill};
use cascade_storage_api::{CorruptionSite, PageKey, StoreError};
use cascade_types::{LocalBoxFuture, Lsn, PageNo, PAGE_SIZE, ScopeId, SegmentId};
use cascade_wal_format::WalRecord;

/// One page as the plan sees it: the image, and its page LSN — the END LSN
/// of the last record applied to it (S17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageData {
    /// Exactly `PAGE_SIZE` bytes.
    pub image: Vec<u8>,
    pub page_lsn: Lsn,
}

impl PageData {
    /// A never-written page: zeros, page LSN 0 (§4.5 promise 4).
    pub fn zero() -> Self {
        PageData {
            image: vec![0u8; PAGE_SIZE],
            page_lsn: Lsn(0),
        }
    }
}

/// The store's page set for one scope, as offered to a plan during one
/// record's apply. Every method copies the page out and back: nothing
/// borrows into the store across an `await`, and the store's state lock is
/// never held while a plan is running.
///
/// A plan runs under the scope's ingest gate, so it is the only writer:
/// a `get` → modify → `put` sequence cannot interleave with another
/// writer's.
pub trait PageSet {
    /// The scope whose pages these are.
    fn scope(&self) -> ScopeId;

    /// The highest-numbered page the store holds for `segment`, in memory
    /// or on disk. Under sequential fill that is the segment's open page.
    fn high_page(&self, segment: SegmentId) -> Option<PageNo>;

    /// A page's current contents, from memory or from disk; `None` for a
    /// page no record has ever touched.
    fn get<'a>(&'a self, key: PageKey) -> LocalBoxFuture<'a, Result<Option<PageData>, StoreError>>;

    /// Stores the page, dirty: the next flush writes it.
    fn put<'a>(
        &'a self,
        key: PageKey,
        page: PageData,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;
}

/// What a log record does to a scope's pages. Implemented over
/// `cascade-redo`; a test shape lives in the conformance test.
///
/// The contract, which every plan must keep:
///
/// * `start` is the record's start LSN. A record whose start is below a
///   page's page LSN is already in that page and must not be applied
///   again (S17).
/// * A record that touches no page returns `Ok(())` having done nothing.
/// * A malformed record is an error, never a silent skip: ingest stops
///   there and the scope's ingest fails loudly.
/// * Records are handed over in LSN order, each exactly once.
///
/// `rec` is borrowed for as long as the returned future runs: a plan may
/// need the record after an await (a page read from disk), and the record
/// must still be there.
pub trait RedoPlan: Send + Sync {
    fn apply<'a>(
        &'a self,
        pages: &'a mut dyn PageSet,
        rec: &'a WalRecord,
        start: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>>;
}

/// The heap-insert family (§12.4) over `cascade-redo`: WAL type 5
/// (`HeapInsert`) appended to its segment's pages under the sequential-fill
/// rule. Transaction markers, the closed-timestamp heartbeat and the HLC
/// high-water record are page-neutral and inert here. Everything else —
/// `LegacyInsert`, a row image from before the page format, and the
/// reserved bulk-load and unlogged-marker families — is a record this plan
/// cannot apply and is not known to touch no page, and is refused rather
/// than skipped.
///
/// **Where routing resumes.** The record names a segment, not a page, so
/// the plan asks the store for the segment's highest page and reads that
/// page's header as a fill cursor. The cursor is therefore derived from
/// the pages the store actually holds — never kept as state beside them —
/// which is what makes it right after a restart: the open page's header on
/// disk *is* the cursor.
///
/// The open page is read through `cascade-page`'s full validation, not a
/// header-only peek. A damaged `free_offset` would otherwise route every
/// later insert to a fresh page — a quiet mis-route — where it must refuse.
pub struct HeapInsertRedo;

impl HeapInsertRedo {
    pub const fn new() -> Self {
        HeapInsertRedo
    }
}

impl Default for HeapInsertRedo {
    fn default() -> Self {
        HeapInsertRedo
    }
}

fn image_of(page: &PageData) -> &[u8; PAGE_SIZE] {
    page.image
        .as_slice()
        .try_into()
        .expect("a page image is PAGE_SIZE bytes")
}

fn image_mut_of(page: &mut PageData) -> &mut [u8; PAGE_SIZE] {
    page.image
        .as_mut_slice()
        .try_into()
        .expect("a page image is PAGE_SIZE bytes")
}

/// Names the law a redo refusal broke, in the contract's own errors.
fn redo_error(e: RedoError, scope: ScopeId, at: Lsn, page: PageKey) -> StoreError {
    match e {
        // The record passed its CRC but does not hold a page-format cell:
        // only a buggy encoder produces that, and it is damage at `at`.
        RedoError::CorruptCell | RedoError::Unplaceable(_) => StoreError::Corruption {
            what: CorruptionSite::LogFrame { scope, at },
        },
        RedoError::Page(_) => StoreError::Corruption {
            what: CorruptionSite::Page { scope, page },
        },
        RedoError::UnsupportedRecord(what) => StoreError::Unsupported(what),
    }
}

impl RedoPlan for HeapInsertRedo {
    fn apply<'a>(
        &'a self,
        pages: &'a mut dyn PageSet,
        rec: &'a WalRecord,
        start: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let (segment, cell) = match rec {
                WalRecord::HeapInsert { segment, cell, .. } => (segment, cell),
                // A pre-page-format row image is a page change this plan
                // cannot apply. Treating it as inert would let the manifest
                // advance past a row that was never materialized, so it is
                // refused: the runtime replays those logs, the page store
                // does not.
                WalRecord::LegacyInsert { .. } => {
                    return Err(StoreError::Unsupported(
                        "LegacyInsert has no page redo: its logs are replayed by the runtime, not the page store",
                    ))
                }
                // Page-neutral by nature: transaction markers and the
                // scope-wide housekeeping records.
                WalRecord::BeginTxn { .. }
                | WalRecord::CommitTxn { .. }
                | WalRecord::AbortTxn { .. }
                | WalRecord::ClosedTsHeartbeat { .. }
                | WalRecord::HlcHighWater { .. } => return Ok(()),
                // Reserved families that are not page-neutral by nature —
                // a bulk load puts data on pages — and whose payload shapes
                // do not exist yet. Skipped, they would let the manifest
                // advance past a page change that was never applied.
                WalRecord::BulkLoad | WalRecord::UnloggedMarker => {
                    return Err(StoreError::Unsupported(
                        "a reserved record family (bulk load, unlogged marker) has no page apply yet",
                    ))
                }
            };
            let scope = pages.scope();
            let page_no = match pages.high_page(*segment) {
                None => 0,
                Some(high) => {
                    let open_key = PageKey {
                        segment: *segment,
                        page_no: high,
                    };
                    let open = pages.get(open_key).await?.ok_or(StoreError::Unsupported(
                        "fill cursor: the segment's open page is unreadable",
                    ))?;
                    // Validated whole: header bounds and every cell.
                    let view = PageView::parse(image_of(&open))
                        .map_err(|e| redo_error(RedoError::Page(e), scope, start, open_key))?;
                    let free_offset = SLOTS_BASE + 2 * view.slot_count() + view.free_bytes();
                    // The pure question: this store keeps no cursor, the
                    // page's own header is the state.
                    SegmentFill::from_page_header(high, view.slot_count() as u16, free_offset as u16)
                        .would_route(cell.len())
                    .map_err(|e| redo_error(e, scope, start, open_key))?
                }
            };
            let key = PageKey {
                segment: *segment,
                page_no,
            };
            let mut page = match pages.get(key).await? {
                Some(p) => p,
                None => PageData::zero(),
            };
            match apply_record(image_mut_of(&mut page), rec, start)
                .map_err(|e| redo_error(e, scope, start, key))?
            {
                ApplyOutcome::Applied { end } => {
                    page.page_lsn = end;
                    pages.put(key, page).await
                }
                // Already in the page (S17). Ingest resumes at the
                // materialized LSN, so a healthy store never lands here;
                // treating it as the no-op redo says it is keeps a re-read
                // harmless.
                ApplyOutcome::Skipped => Ok(()),
                ApplyOutcome::NotAPageRecord => Ok(()),
                // The cursor said this page had room and the page said
                // otherwise: the two disagree, which no correct store
                // produces. Loud, never a guess.
                ApplyOutcome::SpillRequired => Err(StoreError::Unsupported(
                    "sequential fill: the routing cursor and the page disagree on free space",
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_page::{encode_cell, PageView};
    use cascade_types::{TxnId, VersionWord};
    use cascade_wal_format::encoded_len;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory `PageSet` — just enough store for the plan's tests.
    struct MemPages {
        scope: ScopeId,
        pages: Mutex<HashMap<PageKey, PageData>>,
        high: Mutex<HashMap<SegmentId, PageNo>>,
    }

    impl MemPages {
        fn new() -> Self {
            MemPages {
                scope: ScopeId(1),
                pages: Mutex::new(HashMap::new()),
                high: Mutex::new(HashMap::new()),
            }
        }

        fn snapshot(&self) -> HashMap<PageKey, PageData> {
            self.pages.lock().unwrap().clone()
        }
    }

    impl PageSet for MemPages {
        fn scope(&self) -> ScopeId {
            self.scope
        }

        fn high_page(&self, segment: SegmentId) -> Option<PageNo> {
            self.high.lock().unwrap().get(&segment).copied()
        }

        fn get<'a>(
            &'a self,
            key: PageKey,
        ) -> LocalBoxFuture<'a, Result<Option<PageData>, StoreError>> {
            Box::pin(async move { Ok(self.pages.lock().unwrap().get(&key).cloned()) })
        }

        fn put<'a>(
            &'a self,
            key: PageKey,
            page: PageData,
        ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async move {
                if page.page_lsn.0 > 0 {
                    self.high
                        .lock()
                        .unwrap()
                        .entry(key.segment)
                        .and_modify(|h| *h = (*h).max(key.page_no))
                        .or_insert(key.page_no);
                }
                self.pages.lock().unwrap().insert(key, page);
                Ok(())
            })
        }
    }

    fn insert(xid: u64, segment: u32, payload: &[u8]) -> WalRecord {
        WalRecord::HeapInsert {
            xid,
            segment: SegmentId(segment),
            cell: encode_cell(VersionWord::uncommitted(TxnId(xid)), payload).expect("fits"),
        }
    }

    fn page(segment: u32, page_no: u32) -> PageKey {
        PageKey {
            segment: SegmentId(segment),
            page_no,
        }
    }

    /// A 3000-byte payload is a 3026-byte cell plus its 2-byte slot; two
    /// fill an 8160-byte page (6056 used) and a third does not (9084).
    const PER_PAGE: usize = 2;

    #[test]
    fn sequential_fill_spills_whole_cells_across_pages() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let mut pages = MemPages::new();
            let mut at = Lsn(0);
            for i in 0..7u64 {
                let rec = insert(i, 1, &[b'x'; 3000]);
                plan.apply(&mut pages, &rec, at).await.expect("applies");
                at = Lsn(at.0 + encoded_len(&rec) as u64);
            }
            let snap = pages.snapshot();
            assert_eq!(pages.high_page(SegmentId(1)), Some(3), "pages 0..=3");
            assert_eq!(snap.len(), 4);
            for page_no in 0..4u32 {
                let view = PageView::parse(image_of(&snap[&page(1, page_no)])).expect("valid");
                let want = if page_no < 3 { PER_PAGE } else { 1 };
                assert_eq!(view.slot_count(), want, "page {page_no}");
            }
            // A different segment fills from its own page 0.
            let rec = insert(9, 2, b"other");
            plan.apply(&mut pages, &rec, at).await.expect("applies");
            assert_eq!(pages.high_page(SegmentId(2)), Some(0));
            assert_eq!(pages.high_page(SegmentId(1)), Some(3), "segment 1 untouched");
        });
    }

    #[test]
    fn page_neutral_records_are_inert() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let mut pages = MemPages::new();
            for rec in [
                WalRecord::BeginTxn { xid: 1 },
                WalRecord::CommitTxn {
                    xid: 1,
                    commit_ts: cascade_types::Hlc(1),
                },
                WalRecord::AbortTxn { xid: 1 },
                WalRecord::ClosedTsHeartbeat { closed_ts: 1 },
                WalRecord::HlcHighWater { max_issued: 1 },
            ] {
                plan.apply(&mut pages, &rec, Lsn(0)).await.expect("inert");
            }
            assert!(pages.snapshot().is_empty());
            assert!(pages.high.lock().unwrap().is_empty());
        });
    }

    /// A bulk load puts data on pages. A plan that reported it as touching
    /// nothing would let the store's manifest advance past a page change
    /// that was never applied, so it is refused until its apply exists.
    #[test]
    fn reserved_page_families_are_refused_not_inert() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let mut pages = MemPages::new();
            for rec in [WalRecord::BulkLoad, WalRecord::UnloggedMarker] {
                assert!(
                    matches!(
                        plan.apply(&mut pages, &rec, Lsn(0)).await,
                        Err(StoreError::Unsupported(_))
                    ),
                    "{rec:?}"
                );
            }
            assert!(pages.snapshot().is_empty());
        });
    }

    /// A record already in the page (S17) is a no-op: the page decides,
    /// not the cursor.
    #[test]
    fn a_record_below_the_page_lsn_is_skipped() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let mut pages = MemPages::new();
            let rec = insert(1, 1, b"row");
            plan.apply(&mut pages, &rec, Lsn(100)).await.expect("applies");
            let before = pages.snapshot();
            assert_eq!(
                before[&page(1, 0)].page_lsn,
                Lsn(100 + encoded_len(&rec) as u64)
            );
            // The same record again at the same start: below the page LSN.
            plan.apply(&mut pages, &rec, Lsn(100)).await.expect("no error");
            assert_eq!(pages.snapshot(), before, "unchanged");
        });
    }

    #[test]
    fn a_malformed_cell_refuses_before_touching_a_page() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let mut pages = MemPages::new();
            let mut rec = insert(1, 1, b"row");
            if let WalRecord::HeapInsert { cell, .. } = &mut rec {
                cell[..2].copy_from_slice(&3u16.to_le_bytes()); // the length prefix lies
            }
            let err = plan.apply(&mut pages, &rec, Lsn(0)).await;
            assert!(
                matches!(
                    err,
                    Err(StoreError::Corruption {
                        what: CorruptionSite::LogFrame { scope: ScopeId(1), at: Lsn(0) }
                    })
                ),
                "got {err:?}"
            );
            assert!(pages.snapshot().is_empty(), "no page was created");
        });
    }

    #[test]
    fn a_legacy_insert_is_unsupported_not_skipped() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let mut pages = MemPages::new();
            let rec = WalRecord::LegacyInsert {
                xid: 1,
                table: SegmentId(1),
                id: 2,
                val: "v".into(),
            };
            assert!(matches!(
                plan.apply(&mut pages, &rec, Lsn(0)).await,
                Err(StoreError::Unsupported(_))
            ));
            assert!(pages.snapshot().is_empty());
        });
    }

    /// The resume law at the plan level: a plan handed only the open page
    /// — all a restarted store holds — continues the fill exactly where an
    /// uninterrupted run left off.
    #[test]
    fn routing_resumes_from_the_open_page_alone() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let record = insert(1, 1, &[b'y'; 3000]);
            let len = encoded_len(&record) as u64;

            // Uninterrupted: five records, two to a page, over pages 0..=2.
            let mut whole = MemPages::new();
            for i in 0..5u64 {
                plan.apply(&mut whole, &record, Lsn(i * len))
                    .await
                    .expect("applies");
            }
            // Resumed: the store holds only page 0, at its final state.
            let mut resumed = MemPages::new();
            let restored = whole.snapshot()[&page(1, 0)].clone();
            resumed.put(page(1, 0), restored).await.expect("restored");
            for i in PER_PAGE as u64..5 {
                plan.apply(&mut resumed, &record, Lsn(i * len))
                    .await
                    .expect("applies");
            }
            assert_eq!(resumed.snapshot(), whole.snapshot(), "resume == uninterrupted");
        });
    }

    /// A structurally damaged open page is refused as page corruption,
    /// naming the page. A header-only read would miss this: the version and
    /// kind are intact, only `free_offset` is impossible.
    #[test]
    fn a_damaged_open_page_refuses_naming_the_page() {
        futures_lite::future::block_on(async {
            let plan = HeapInsertRedo::new();
            let mut pages = MemPages::new();
            let first = insert(1, 1, b"row");
            plan.apply(&mut pages, &first, Lsn(0)).await.expect("applies");

            let mut damaged = pages.snapshot()[&page(1, 0)].clone();
            let img: &mut [u8; PAGE_SIZE] = damaged
                .image
                .as_mut_slice()
                .try_into()
                .expect("page-sized");
            img[6..8].copy_from_slice(&0u16.to_le_bytes()); // free_offset below the slot array
            pages.put(page(1, 0), damaged).await.expect("stored");

            let next = insert(2, 1, b"next row");
            let err = plan.apply(&mut pages, &next, Lsn(500)).await;
            assert!(
                matches!(
                    err,
                    Err(StoreError::Corruption {
                        what: CorruptionSite::Page {
                            scope: ScopeId(1),
                            page: damaged_key,
                        }
                    }) if damaged_key == page(1, 0)
                ),
                "got {err:?}"
            );
            assert_eq!(pages.snapshot().len(), 1, "no fresh page was mis-routed into");
        });
    }
}
