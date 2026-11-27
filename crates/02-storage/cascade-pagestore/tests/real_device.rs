//! The page store on the real glommio `O_DIRECT` device. Every other
//! suite runs on the in-memory fake; this one runs the whole stack —
//! log store, page store, recovery — on `GlommioBlockDevice` over a real
//! temp directory, because the fake cannot answer for the parts that only
//! exist against a real device: alignment of every padded write, the
//! record-block read-modify-write, files created and reopened through the
//! actual filesystem namespace.
//!
//! The workload is small and the crash cuts are absent — crash semantics
//! stay with the fake and, one day, the bare-metal gate (T5.7). What is
//! pinned here is that everything the other suites verify also *works*
//! when the device is real.
//!
//! The futures are awaited directly inside the executor (never
//! `block_on`ed within it): a nested `block_on` parks the thread, the
//! reactor stops ticking, and the `DmaFile` completions never arrive.

mod common;

use std::sync::Arc;

use cascade_blockdev::GlommioBlockDevice;
use cascade_logstore::LogStore;
use cascade_pagestore::{Config, HeapInsertRedo, PageStore};
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::log_source::LogSource;
use cascade_storage_api::page_source::PageSource;
use cascade_storage_api::scope_admin::{LogScopes, PageScopes};
use cascade_storage_api::types::AppendMeta;
use cascade_storage_api::{OpCtx, PageKey, TenantContext};
use cascade_types::{DatabaseId, Epoch, Lsn};

use common::{group, Workload, KEY, SCOPE};

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("pagestore-real")
        .make()
        .expect("executor builds")
}

/// Every page the reference holds, compared against the replay — the §10
/// differential law, awaited inline.
async fn verify(pages: &PageStore<GlommioBlockDevice, HeapInsertRedo>, wl: &Workload) -> usize {
    let span = cascade_telemetry::Span::noop();
    let ctx = OpCtx::new(TenantContext::for_database(DatabaseId(0)), &span);
    for key in wl.reference.keys() {
        let page = PageKey {
            segment: key.0,
            page_no: key.1,
        };
        let want = wl.reference.image(key).expect("a reference page");
        let got = pages
            .get_page(&ctx, SCOPE, page, wl.reference.end)
            .await
            .unwrap_or_else(|e| panic!("page {page:?}: {e:?}"));
        assert_eq!(
            got.page_lsn,
            common::Reference::page_lsn_of(want),
            "page {page:?}: page LSN"
        );
        assert!(
            got.buf.as_slice() == want,
            "page {page:?} differs from the replay on the real device"
        );
    }
    wl.reference.page_count()
}

#[test]
fn the_page_store_materializes_and_serves_on_the_real_device() {
    let dir = std::env::temp_dir().join(format!(
        "cascade-pagestore-real-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the temp dir is made");

    // The workload's reference replay is kept outside the executors: it is
    // pure, and both phases need it.
    let mut wl = Workload::new(0xCAFE_5EED);

    // Phase 1: write, materialize, verify — all on the real device.
    {
        let dev = Arc::new(GlommioBlockDevice::new(dir.clone()));
        reactor().run(async {
            let log = LogStore::open(dev.clone()).await.expect("the log store opens");
            let source: Arc<dyn LogSource> = log.clone();
            let pages = PageStore::open(dev, source, HeapInsertRedo::new(), Config::default())
                .await
                .expect("the page store opens");
            let span = cascade_telemetry::Span::noop();
            let ctx = OpCtx::new(TenantContext::for_database(DatabaseId(0)), &span);
            pages.attach(&ctx, SCOPE, None, KEY).await.expect("attaches");
            log.create_scope(&ctx, SCOPE, Epoch(1), KEY).await.expect("creates");
            log.open_writer(&ctx, SCOPE, Epoch(1)).await.expect("opens writer");

            for xid in 1..=40u64 {
                let rows = wl.random_rows();
                let (bytes, records) = group(wl.at.0, xid, &rows);
                let meta = AppendMeta {
                    max_commit_ts: None,
                    writer_durable: Lsn(0),
                };
                log.append(&ctx, SCOPE, Epoch(1), wl.at, &bytes, meta)
                    .await
                    .expect("appends");
                wl.at = log
                    .wait_durable(&ctx, SCOPE, Lsn(wl.at.0 + bytes.len() as u64))
                    .await
                    .expect("flushes");
                wl.reference.apply(&records);
            }
            pages.catch_up(&ctx, SCOPE, wl.reference.end).await.expect("ingests");
            pages.flush(&ctx, SCOPE).await.expect("flushes");
            let compared = verify(&pages, &wl).await;
            assert!(compared >= 20, "pages compared on the real device: {compared}");
        });
    }

    // Phase 2: a fresh executor reopens both stores — the log store's §5.7
    // scan and the page store's recovery run against the real files — and
    // every page is still exactly the replay.
    {
        let dev = Arc::new(GlommioBlockDevice::new(dir.clone()));
        reactor().run(async {
            let log = LogStore::open(dev.clone()).await.expect("the log store reopens");
            let materialized_before = {
                let source: Arc<dyn LogSource> = log.clone();
                let pages = PageStore::open(
                    dev.clone(),
                    source,
                    HeapInsertRedo::new(),
                    Config::default(),
                )
                    .await
                    .expect("the page store reopens");
                let materialized = pages
                    .materialized_lsn(SCOPE)
                    .expect("the scope survived the reopen");
                let compared = verify(&pages, &wl).await;
                assert!(compared >= 20, "pages compared after the reopen: {compared}");
                materialized
            };
            assert!(
                materialized_before == wl.reference.end,
                "the manifest names the flushed log end: {materialized_before:?}"
            );

            // And the real device keeps taking writes through the reopened
            // stores — the open page's header on disk routes the
            // continuation. The scope is already attached here: it came
            // back with the manifest, and attach would refuse it.
            let source: Arc<dyn LogSource> = log.clone();
            let pages = PageStore::open(dev.clone(), source, HeapInsertRedo::new(), Config::default())
                .await
                .expect("opens a second time");
            let span = cascade_telemetry::Span::noop();
            let ctx = OpCtx::new(TenantContext::for_database(DatabaseId(0)), &span);
            log.open_writer(&ctx, SCOPE, Epoch(1)).await.expect("writer");
            for xid in 1000..1015u64 {
                let rows = wl.random_rows();
                let (bytes, records) = group(wl.at.0, xid, &rows);
                let meta = AppendMeta {
                    max_commit_ts: None,
                    writer_durable: Lsn(0),
                };
                log.append(&ctx, SCOPE, Epoch(1), wl.at, &bytes, meta)
                    .await
                    .expect("appends");
                wl.at = log
                    .wait_durable(&ctx, SCOPE, Lsn(wl.at.0 + bytes.len() as u64))
                    .await
                    .expect("flushes");
                wl.reference.apply(&records);
            }
            pages.catch_up(&ctx, SCOPE, wl.reference.end).await.expect("ingests");
            pages.flush(&ctx, SCOPE).await.expect("flushes");
            let compared = verify(&pages, &wl).await;
            assert!(compared >= 25, "pages compared after the continuation: {compared}");
        });
    }

    std::fs::remove_dir_all(&dir).expect("the temp dir is cleaned");
}
