//! The page store passes the contract crate's `PageSource` suite (§4.5).
//!
//! The suite drives records whose cells are a test shape — `[page_no u32]
//! [content]` — because `cascade-storage-api` never depends on
//! `cascade-redo` (§4.10). So the store under test is the real store with
//! two test-only pieces around it:
//!
//! * `ToyPlan`: the suite's own routing and apply rules, as a `RedoPlan`.
//! * `Eager`: materializes after every read.
//!
//! The second one needs a word. The suite asserts `materialized_lsn` right
//! after a `get_page`. The real store materializes when asked — on its own,
//! that assertion fails for a store that is merely lazy. `Eager` makes the
//! store eager, and the suite then exercises the real path at every step:
//! catch-up, the doublewrite batch, the manifest, `TooOld`, `BeyondDurable`
//! and scope administration, all on the real store's code. Real redo has
//! its own suites (`differential.rs`, `crash.rs`).

mod common;

use std::sync::Arc;

use cascade_logstore::LogStore;
use cascade_pagestore::{Config, PageData, PageSet, PageStore, RedoPlan};
use cascade_storage_api::conformance::{page_source_suite, PageStoreHarness};
use cascade_storage_api::fake::MemBlockDevice;
use cascade_storage_api::log_source::LogSource;
use cascade_storage_api::page_source::PageSource;
use cascade_storage_api::scope_admin::PageScopes;
use cascade_storage_api::types::Lineage;
use cascade_storage_api::{
    CorruptionSite, KeyRef, OpCtx, PageImage, PageKey, ReaderId, StoreError,
};
use cascade_types::{LocalBoxFuture, Lsn, ScopeId};
use cascade_wal_format::{encoded_len, WalRecord};
use futures_lite::future::block_on;

/// The suite's own redo, as a `RedoPlan`. It mirrors
/// `conformance::{toy_route, toy_apply}`: a HEAP_INSERT's cell is
/// `[page_no u32][content]` for its record's segment; apply writes the
/// content at offset 0 and stamps the page LSN, idempotently by LSN (§12.4).
struct ToyPlan;

impl RedoPlan for ToyPlan {
    fn apply<'a>(
        &'a self,
        pages: &'a mut dyn PageSet,
        rec: &'a WalRecord,
        start: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let WalRecord::HeapInsert { segment, cell, .. } = rec else {
                return Ok(());
            };
            let scope = pages.scope();
            if cell.len() < 4 {
                return Err(StoreError::Corruption {
                    what: CorruptionSite::LogFrame {
                        scope,
                        at: start,
                    },
                });
            }
            let page_no = u32::from_le_bytes(cell[..4].try_into().expect("4 bytes"));
            let key = PageKey {
                segment: *segment,
                page_no,
            };
            let mut page = match pages.get(key).await? {
                Some(page) => page,
                None => PageData::zero(),
            };
            if page.page_lsn > start {
                return Ok(()); // already applied (S17)
            }
            let content = &cell[4..];
            page.image[..content.len()].copy_from_slice(content);
            page.page_lsn = Lsn(start.0 + encoded_len(rec) as u64);
            pages.put(key, page).await
        })
    }
}

/// The real store, materializing after every read.
struct Eager(Arc<PageStore<MemBlockDevice, ToyPlan>>);

impl PageSource for Eager {
    fn get_page<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        page: PageKey,
        at_lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<PageImage, StoreError>> {
        Box::pin(async move {
            let image = self.0.get_page(ctx, scope, page, at_lsn).await?;
            self.0.flush(ctx, scope).await?;
            Ok(image)
        })
    }

    fn materialized_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        self.0.materialized_lsn(scope)
    }

    fn oldest_readable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        self.0.oldest_readable_lsn(scope)
    }

    fn set_read_floor<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        reader: ReaderId,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        self.0.set_read_floor(ctx, scope, reader, lsn)
    }
}

impl PageScopes for Eager {
    fn attach<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        lineage: Option<Lineage>,
        key: KeyRef,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        self.0.attach(ctx, scope, lineage, key)
    }

    fn detach<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        self.0.detach(ctx, scope)
    }
}

/// One real log store and one real page store, on one device.
struct Harness {
    dev: Arc<MemBlockDevice>,
    report_retention: bool,
}

impl PageStoreHarness for Harness {
    type Log = LogStore<MemBlockDevice>;
    type Store = Eager;

    fn create(&mut self) -> (Arc<Self::Log>, Arc<Self::Store>) {
        // A fresh pair means a fresh device: the suite attaches a scope a
        // reopened device would already hold.
        self.dev = Arc::new(MemBlockDevice::new());
        let log = common::open_log(self.dev.clone());
        let source: Arc<dyn LogSource> = log.clone();
        let config = Config {
            report_retention: self.report_retention,
            ..Config::default()
        };
        let pages = block_on(PageStore::open(
            self.dev.clone(),
            source,
            ToyPlan,
            config,
        ))
        .expect("the page store opens");
        (log, Arc::new(Eager(pages)))
    }
}

#[test]
fn the_real_store_passes_the_page_source_suite() {
    let mut harness = Harness {
        dev: Arc::new(MemBlockDevice::new()),
        report_retention: false,
    };
    block_on(page_source_suite(&mut harness));
}

/// The same suite with the store reporting its materialized LSN to the log
/// store as its retention value (§6.3): every read the suite makes still
/// succeeds, because the store reports only what its manifest already
/// names.
#[test]
fn the_real_store_passes_the_page_source_suite_reporting_retention() {
    let mut harness = Harness {
        dev: Arc::new(MemBlockDevice::new()),
        report_retention: true,
    };
    block_on(page_source_suite(&mut harness));
}
