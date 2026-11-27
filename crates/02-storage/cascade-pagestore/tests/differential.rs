//! §10's differential page test: for a random workload, every page the
//! store serves at an LSN must equal the page built by replaying the same
//! log from empty through `cascade-redo` — byte for byte, page LSN
//! included. The reference is built by the harness with nothing from the
//! store, and compared at every checkpoint.
//!
//! The store under test is squeezed: a cache of three pages and a dirty
//! bound of six, so pages are evicted, re-read from disk, flushed by
//! backpressure and overwritten many times over.

mod common;

use std::sync::Arc;

use cascade_pagestore::FlushOutcome;
use cascade_storage_api::fake::MemBlockDevice;
use cascade_storage_api::page_source::PageSource;
use cascade_types::{Lsn, SegmentId};

use common::*;

/// Small enough that pages cycle through disk; checked below that the
/// workload still spans many pages and all four segments.
const CACHE: usize = 3;
const DIRTY: usize = 6;

#[test]
fn served_pages_equal_the_replayed_log() {
    let node = boot(Arc::new(MemBlockDevice::new()), squeezed(CACHE, DIRTY));
    create_scope(&node, SCOPE);
    let mut wl = Workload::new(0xD1FF_E7E7);

    let mut compared = 0usize;
    for _ in 0..12 {
        for _ in 0..50 {
            wl.push_random(&node, SCOPE).expect("appends");
        }
        wl.checkpoint();
        compared += verify(&node, SCOPE, &wl.reference, wl.reference.end);
        verify_unwritten(&node, SCOPE, &wl.reference);
    }

    // The workload is what the test says it is.
    assert!(
        wl.reference.page_count() >= 60,
        "many pages: {}",
        wl.reference.page_count()
    );
    for segment in 1..=SEGMENTS {
        assert!(
            wl.reference.high(SegmentId(segment)).is_some(),
            "segment {segment} was written"
        );
    }
    assert!(compared >= 12 * 30, "pages compared at checkpoints: {compared}");

    // As-of reads, differentially: every past checkpoint is either
    // exactly itself or `TooOld`.
    let checked = verify_history(&node, SCOPE, &wl.reference);
    assert!(checked >= 12 * 30, "as-of reads checked: {checked}");

    // Nobody called `flush` in this test. The manifest moved anyway —
    // ingest flushed at its dirty bound — and never past what was ingested.
    let stats = node.pages.stats(SCOPE).expect("the scope");
    assert!(stats.materialized > Lsn(0), "ingest flushed on its own");
    assert!(stats.materialized <= stats.ingested);
    assert_eq!(stats.ingested, wl.reference.end);
    assert!(
        stats.cached_pages <= CACHE + stats.dirty_pages,
        "the cache bound holds: {stats:?}"
    );
}

/// A restarted store resumes exactly where its last flush stopped. The
/// pages on disk are the open pages' headers, and every later insert must
/// be routed as the uninterrupted run routed it — which is the whole
/// argument for deriving the fill cursor from the page and keeping it
/// nowhere else.
///
/// Rounds alternate between a clean flush and a crash with pages still
/// unflushed, so each restart meets a manifest somewhere else.
#[test]
fn a_restarted_store_resumes_where_its_flush_stopped() {
    let dev = Arc::new(MemBlockDevice::new());
    let config = squeezed(CACHE, DIRTY);
    let mut wl = Workload::new(0x5EED_5EED);

    for round in 0..8u64 {
        let node = boot(dev.clone(), config.clone());
        if round == 0 {
            create_scope(&node, SCOPE);
        }
        for _ in 0..25 {
            wl.push_random(&node, SCOPE).expect("appends");
        }
        catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
        if round % 2 == 0 {
            flush(&node, SCOPE).expect("flushes");
        }
        drop(node);
        dev.crash(round + 1);
    }

    let node = boot(dev.clone(), config);
    let materialized = node.pages.materialized_lsn(SCOPE).expect("the scope survived");
    assert!(materialized <= wl.reference.end, "the manifest names a real point");
    let compared = verify(&node, SCOPE, &wl.reference, wl.reference.end);
    assert!(compared >= 40, "pages compared after the restarts: {compared}");
    let stats = node.pages.stats(SCOPE).unwrap();
    assert_eq!(stats.ingested, wl.reference.end);
    assert!(stats.materialized >= materialized, "the manifest never moves backwards");

    // And it keeps going: one more stretch through the restarted store.
    for _ in 0..40 {
        wl.push_random(&node, SCOPE).expect("appends");
    }
    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
    flush(&node, SCOPE).expect("flushes");
    verify(&node, SCOPE, &wl.reference, wl.reference.end);
}

/// What `flush` reports is what it did, in terms a caller can act on.
#[test]
fn flush_reports_what_it_did() {
    let node = boot(Arc::new(MemBlockDevice::new()), no_backpressure(64));
    create_scope(&node, SCOPE);
    let mut wl = Workload::new(7);

    // Nothing appended, nothing ingested.
    assert!(matches!(flush(&node, SCOPE), Ok(FlushOutcome::Clean)));

    // Rows appended but not yet ingested: the page store has not seen
    // them, so for it there is nothing to write.
    for _ in 0..5 {
        wl.push_random(&node, SCOPE).expect("appends");
    }
    assert!(
        matches!(flush(&node, SCOPE), Ok(FlushOutcome::Clean)),
        "nothing is dirty before ingest"
    );

    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
    assert!(matches!(
        flush(&node, SCOPE),
        Ok(FlushOutcome::Flushed { pages, materialized })
            if pages == wl.reference.page_count() && materialized == wl.reference.end
    ));
    assert!(
        matches!(flush(&node, SCOPE), Ok(FlushOutcome::Clean)),
        "a second flush has nothing to do"
    );

    // Records that touch no page move the restart point without writing one.
    for _ in 0..3 {
        wl.push(&node, SCOPE, &[]).expect("appends a commit-only group");
    }
    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
    assert!(matches!(
        flush(&node, SCOPE),
        Ok(FlushOutcome::Advanced { materialized }) if materialized == wl.reference.end
    ));
    assert!(
        matches!(node.pages.materialized_lsn(SCOPE), Ok(m) if m == wl.reference.end),
        "the manifest names the ingested LSN"
    );
}
