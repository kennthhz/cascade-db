//! §10's crash tests for the page store (§6.3, §6.6). The log store and the
//! page store share one device; the device crashes at cut points; and
//! after every crash, every page the reopened store serves must equal the
//! replay of the acknowledged log.
//!
//! The cut points are not sampled. A flush is a protocol — batch, sync,
//! home files, sync, manifest, clear — so the tests count the page-store
//! operations one flush performs and then crash at *each* of them, both
//! before the operation (it is lost) and after it (it is written, and the
//! crash decides whether it survives). `MemBlockDevice::crash` keeps each
//! unsynced 4 KiB block with probability one half, so those cuts produce
//! torn 8 KiB pages, a record persisted without its image, a batch half in
//! the doublewrite area: every state recovery has to tell apart.

mod common;

use std::sync::{Arc, Mutex};

use cascade_pagestore::{Config, FlushOutcome, HeapInsertRedo, PageStore};
use cascade_storage_api::fake::MemBlockDevice;
use cascade_storage_api::log_source::{LogSource, LogSubscription};
use cascade_storage_api::page_source::PageSource;
use cascade_storage_api::types::LogChunk;
use cascade_storage_api::{HolderReport, OpCtx, RetentionHolder, StoreError};
use cascade_types::{Hlc, LocalBoxFuture, Lsn, ScopeId};
use futures_lite::future::block_on;

use common::*;

/// Groups in the first stretch of the workload.
const GROUPS: u64 = 70;
/// Groups appended after a first flush has landed.
const MORE: u64 = 50;

/// A store that caches four pages and never flushes on its own, so that a
/// test can arm exactly one flush.
fn plain() -> Config {
    no_backpressure(4)
}

/// A node with the first stretch appended and durable. Nothing is ingested
/// or flushed yet.
fn phase_one(dev: Arc<CutDevice>) -> (Node<CutDevice>, Workload) {
    let node = boot(dev.clone(), plain());
    create_scope(&node, SCOPE);
    let mut wl = Workload::new(0xC0FFEE);
    for _ in 0..GROUPS {
        wl.push_random(&node, SCOPE).expect("appends");
    }
    (node, wl)
}

/// A node whose first flush has landed and whose next flush will overwrite
/// pages that already exist on disk. Returns the LSN that first flush
/// materialized through.
fn phase_two(dev: Arc<CutDevice>) -> (Node<CutDevice>, Workload, Lsn) {
    let (node, mut wl) = phase_one(dev.clone());
    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
    flush(&node, SCOPE).expect("the first flush lands");
    let first = node.pages.materialized_lsn(SCOPE).expect("the scope");
    assert_eq!(first, wl.reference.end, "the first flush covers the first stretch");
    for _ in 0..MORE {
        wl.push_random(&node, SCOPE).expect("appends");
    }
    (node, wl, first)
}

/// How many page-store operations the flush under test performs when
/// nothing interrupts it.
fn flush_ops(second: bool) -> u64 {
    let dev = Arc::new(CutDevice::new(0, false));
    let (node, wl) = if second {
        let (node, wl, _) = phase_two(dev.clone());
        (node, wl)
    } else {
        phase_one(dev.clone())
    };
    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
    let before = dev.ops();
    flush(&node, SCOPE).expect("flushes");
    dev.ops() - before
}

/// What the cuts of one scenario put the recovery path through.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    /// Cuts at which `open` completed an interrupted batch.
    completed: u32,
    /// Cuts at which `open` found a home page it had to restore.
    repaired: u32,
}

/// Crashes at every operation of one flush and checks every reopening.
fn every_cut(second: bool) -> Seen {
    let ops = flush_ops(second);
    assert!(ops > 8, "a flush is a protocol, not a write: {ops} operations");

    let mut seen = Seen::default();
    for after in [false, true] {
        for cut in 1..=ops + 1 {
            let dev = Arc::new(CutDevice::new(cut.wrapping_mul(7919) ^ u64::from(after), after));
            let (node, mut wl, first) = if second {
                let (node, wl, first) = phase_two(dev.clone());
                (node, wl, first)
            } else {
                let (node, wl) = phase_one(dev.clone());
                (node, wl, Lsn(0))
            };
            catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
            assert!(!dev.crashed(), "nothing is armed yet");

            dev.arm(cut);
            let outcome = flush(&node, SCOPE);
            let at = format!("cut {cut} of {ops}, after={after}");
            if cut <= ops {
                assert!(dev.crashed(), "{at}: the cut must land");
                assert!(
                    matches!(outcome, Err(StoreError::Io(_))),
                    "{at}: the flush reports it, got {outcome:?}"
                );
            } else {
                assert!(!dev.crashed(), "{at}: nothing is armed past the last operation");
                assert!(
                    matches!(outcome, Ok(FlushOutcome::Flushed { .. })),
                    "{at}: got {outcome:?}"
                );
            }

            // The machine is dead; everything acknowledged is on disk.
            drop(node);
            dev.revive();
            let node = boot(dev.clone(), plain());
            let report = node.pages.open_report();
            seen.completed += u32::from(report.completed_batch);
            seen.repaired += u32::from(report.repaired_pages > 0);

            // The manifest names a point the pages really are a replay
            // through. A completed batch is the whole snapshot, so it
            // names the log end. Without one, the manifest either did not
            // move, or had already moved and the batch been cleared before
            // the crash — the cut landing on the protocol's last step.
            let materialized = node.pages.materialized_lsn(SCOPE).expect("the scope survived");
            if report.completed_batch {
                assert_eq!(
                    materialized, wl.reference.end,
                    "{at}: a completed batch is the whole snapshot"
                );
            } else {
                assert!(
                    materialized == first || materialized == wl.reference.end,
                    "{at}: the manifest is at its old point or its new one, not {materialized:?}"
                );
            }

            // The law: whatever the crash left, every page served is the
            // replay of the acknowledged log.
            let pages = verify_whole(&node, SCOPE, &wl.reference, wl.reference.end);
            assert_eq!(pages, wl.reference.page_count(), "{at}");
            let stats = node.pages.stats(SCOPE).unwrap();
            assert_eq!(stats.ingested, wl.reference.end, "{at}");
            assert!(stats.materialized <= stats.ingested, "{at}");

            // And the recovered store keeps working: more rows, a flush, a
            // clean crash, and still the replay.
            for _ in 0..20 {
                wl.push_random(&node, SCOPE).expect("appends");
            }
            catch_up(&node, SCOPE, wl.reference.end).expect("{at}: ingests after recovery");
            flush(&node, SCOPE).expect("{at}: flushes after recovery");
            assert!(
                matches!(node.pages.materialized_lsn(SCOPE), Ok(m) if m == wl.reference.end),
                "{at}"
            );
            drop(node);
            dev.plain_crash(cut + 1);
            dev.revive();
            let node = boot(dev.clone(), plain());
            verify_whole(&node, SCOPE, &wl.reference, wl.reference.end);
        }
    }
    seen
}

/// The first flush of a scope: the home files do not exist yet, so the
/// cuts are the batch being written and synced, the files being created
/// and filled, and the manifest being written.
#[test]
fn every_cut_of_the_first_flush_recovers() {
    let seen = every_cut(false);
    assert!(seen.completed > 0, "some cuts were completed by recovery: {seen:?}");
    assert!(seen.repaired > 0, "some cuts needed a home page restored: {seen:?}");
}

/// A later flush: it overwrites pages already on disk and rewrites record
/// blocks that hold other pages' records, so the cuts also produce torn
/// overwrites — half old, half new — which only the doublewrite copy can
/// set right.
#[test]
fn every_cut_of_a_later_flush_recovers() {
    let seen = every_cut(true);
    assert!(seen.completed > 0, "some cuts were completed by recovery: {seen:?}");
    assert!(seen.repaired > 0, "some cuts needed a home page restored: {seen:?}");
}

/// Two scopes share one device, one manifest and one doublewrite area. A
/// crash while one of them flushes must cost the other nothing: its
/// manifest entry does not move, its pages are untouched, and it resumes
/// where it was.
#[test]
fn a_crash_in_one_scope_costs_the_other_nothing() {
    const OTHER: ScopeId = ScopeId(8);

    // Two scopes with a first stretch each. Scope 7's flush has landed;
    // scope 8's is the one under test.
    fn setup(dev: Arc<CutDevice>) -> (Node<CutDevice>, Workload, Workload, Lsn) {
        let node = boot(dev.clone(), plain());
        create_scope(&node, SCOPE);
        create_scope(&node, OTHER);
        let (mut a, mut b) = (Workload::new(11), Workload::new(22));
        for _ in 0..40 {
            a.push_random(&node, SCOPE).expect("appends");
            b.push_random(&node, OTHER).expect("appends");
        }
        catch_up(&node, SCOPE, a.reference.end).expect("ingests");
        catch_up(&node, OTHER, b.reference.end).expect("ingests");
        flush(&node, SCOPE).expect("scope 7's flush lands");
        let a_end = a.reference.end;
        (node, a, b, a_end)
    }

    // How many operations scope 8's flush performs with nothing in its way.
    let ops = {
        let dev = Arc::new(CutDevice::new(0, false));
        let (node, _, _, _) = setup(dev.clone());
        let before = dev.ops();
        flush(&node, OTHER).expect("flushes");
        dev.ops() - before
    };
    assert!(ops > 8, "a flush is a protocol: {ops} operations");

    for after in [false, true] {
        for cut in (1..=ops + 1).step_by(3) {
            let dev = Arc::new(CutDevice::new(cut.wrapping_mul(7919) ^ u64::from(after), after));
            let (node, mut a, mut b, a_end) = setup(dev.clone());
            let a_pages = a.reference.page_count();
            let at = format!("cut {cut} of {ops}, after={after}");

            dev.arm(cut);
            let outcome = flush(&node, OTHER);
            assert_eq!(dev.crashed(), cut <= ops, "{at}: the cut lands iff one is armed");
            if cut > ops {
                assert!(
                    matches!(outcome, Ok(FlushOutcome::Flushed { .. })),
                    "{at}: got {outcome:?}"
                );
            }
            drop(node);
            dev.revive();

            let node = boot(dev.clone(), plain());
            // Scope 7: exactly where it was.
            assert!(
                matches!(node.pages.materialized_lsn(SCOPE), Ok(m) if m == a_end),
                "{at}: the other scope's manifest entry did not move"
            );
            assert_eq!(
                verify_whole(&node, SCOPE, &a.reference, a_end),
                a_pages,
                "{at}: the other scope's pages are untouched"
            );
            // Scope 8: whatever the crash left, it serves the replay.
            verify_whole(&node, OTHER, &b.reference, b.reference.end);

            // And both keep going together.
            for _ in 0..15 {
                a.push_random(&node, SCOPE).expect("appends");
                b.push_random(&node, OTHER).expect("appends");
            }
            catch_up(&node, SCOPE, a.reference.end).expect("ingests");
            catch_up(&node, OTHER, b.reference.end).expect("ingests");
            flush(&node, SCOPE).expect("{at}: scope 7 flushes");
            flush(&node, OTHER).expect("{at}: scope 8 flushes");
            drop(node);
            dev.plain_crash(cut + 1);
            dev.revive();
            let node = boot(dev.clone(), plain());
            verify_whole(&node, SCOPE, &a.reference, a.reference.end);
            verify_whole(&node, OTHER, &b.reference, b.reference.end);
        }
    }
}

/// The doublewrite header says how long it is, so a batch in flight is
/// still found and finished when the configured header size changed
/// between a crash and the restart. Taking the length from the
/// configuration instead would read the checksum from the wrong place,
/// mistake the batch for one that never synced, and leave its half-written
/// home pages ahead of the manifest — the state the whole design rules
/// out.
#[test]
fn a_batch_in_flight_survives_a_changed_header_size() {
    // Twenty home writes into a flush: the batch is synced, and its home
    // copies are part written.
    const CUT: u64 = 3 + 20;

    for (written_with, reopened_with, after) in
        [(1usize, 32, false), (32, 1, false), (1, 32, true), (32, 1, true)]
    {
        let at = format!("written with {written_with} block(s), reopened with {reopened_with}, after={after}");
        let dev = Arc::new(CutDevice::new(written_with as u64 * 7919 + u64::from(after), after));
        let node = boot(
            dev.clone(),
            Config {
                dw_header_blocks: written_with,
                // A one-block header describes 168 pages, so the dirty
                // bound must fit that.
                ..squeezed(4, 100)
            },
        );
        create_scope(&node, SCOPE);
        let mut wl = Workload::new(0xC0FFEE);
        for _ in 0..GROUPS {
            wl.push_random(&node, SCOPE).expect("appends");
        }
        catch_up(&node, SCOPE, wl.reference.end).expect("ingests");

        dev.arm(CUT);
        assert!(
            matches!(flush(&node, SCOPE), Err(StoreError::Io(_))),
            "{at}: the cut lands"
        );
        drop(node);
        dev.revive();

        let node = boot(
            dev.clone(),
            Config {
                dw_header_blocks: reopened_with,
                ..squeezed(4, 100)
            },
        );
        assert!(
            node.pages.open_report().completed_batch,
            "{at}: the batch is found whatever the header size is now"
        );
        assert!(
            matches!(node.pages.materialized_lsn(SCOPE), Ok(m) if m == wl.reference.end),
            "{at}: and finished"
        );
        verify_whole(&node, SCOPE, &wl.reference, wl.reference.end);

        // The store keeps flushing under the new size.
        for _ in 0..20 {
            wl.push_random(&node, SCOPE).expect("appends");
        }
        catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
        flush(&node, SCOPE).expect("{at}: flushes under the new size");
        drop(node);
        dev.plain_crash(CUT + 1);
        dev.revive();
        let node = boot(
            dev.clone(),
            Config {
                dw_header_blocks: reopened_with,
                ..squeezed(4, 100)
            },
        );
        verify_whole(&node, SCOPE, &wl.reference, wl.reference.end);
    }
}

/// A node whose first flush crashed `cut` page-store operations in: the
/// batch is synced and its home copies are part written. Returns the
/// workload; the machine is left dead.
fn crash_mid_flush(dev: Arc<CutDevice>, cut: u64) -> Workload {
    let node = boot(dev.clone(), plain());
    create_scope(&node, SCOPE);
    let mut wl = Workload::new(0xC0FFEE);
    for _ in 0..GROUPS {
        wl.push_random(&node, SCOPE).expect("appends");
    }
    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
    dev.arm(cut);
    assert!(
        matches!(flush(&node, SCOPE), Err(StoreError::Io(_))),
        "the cut lands"
    );
    drop(node);
    dev.revive();
    wl
}

/// Opens only the page store, without unwrapping: a cut armed in recovery
/// must come back as the error it is.
fn open_pages_fallibly(
    dev: Arc<CutDevice>,
) -> Result<Arc<PageStore<CutDevice, HeapInsertRedo>>, StoreError> {
    let log = open_log(dev.clone());
    let source: Arc<dyn LogSource> = log.clone();
    block_on(PageStore::open(
        dev,
        source,
        HeapInsertRedo::new(),
        plain(),
    ))
}

/// Recovery is itself interrupted by crashes, and must be safe to run again
/// on whatever it left. The batch stays in the doublewrite file until the
/// manifest naming it is durable, so a second `open` only redoes the part
/// that did not finish — and nothing is served before the batch is whole.
#[test]
fn a_crash_during_recovery_is_recovered_by_the_next_open() {
    const FIRST_CUT: u64 = 3 + 20;

    // How many page-store operations an uninterrupted recovery performs.
    let recovery_ops = {
        let dev = Arc::new(CutDevice::new(1, false));
        let _ = crash_mid_flush(dev.clone(), FIRST_CUT);
        let before = dev.ops();
        open_pages_fallibly(dev.clone()).expect("recovery completes");
        dev.ops() - before
    };
    assert!(
        recovery_ops > 4,
        "recovery is a protocol too: {recovery_ops} operations"
    );

    for after in [false, true] {
        for cut in (1..=recovery_ops + 1).step_by(2) {
            let at = format!("recovery cut {cut} of {recovery_ops}, after={after}");
            let dev = Arc::new(CutDevice::new(cut.wrapping_mul(31) ^ u64::from(after), after));
            let mut wl = crash_mid_flush(dev.clone(), FIRST_CUT);
            let end = wl.at;

            dev.arm(cut);
            let opened = open_pages_fallibly(dev.clone());
            assert_eq!(dev.crashed(), cut <= recovery_ops, "{at}: the cut lands iff one is armed");
            if cut <= recovery_ops {
                assert!(
                    matches!(opened, Err(StoreError::Io(_))),
                    "{at}: the interrupted open reports it, got {:?}",
                    opened.as_ref().err()
                );
            }
            drop(opened);
            dev.revive();

            // The next open finishes the job, and every page is the replay.
            let node = boot(dev.clone(), plain());
            assert!(
                matches!(node.pages.materialized_lsn(SCOPE), Ok(m) if m == end),
                "{at}: the batch is finished"
            );
            verify_whole(&node, SCOPE, &wl.reference, end);

            // And the store keeps going.
            for _ in 0..15 {
                wl.push_random(&node, SCOPE).expect("appends");
            }
            catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
            flush(&node, SCOPE).expect("{at}: flushes");
            drop(node);
            dev.plain_crash(cut + 1);
            dev.revive();
            let node = boot(dev.clone(), plain());
            verify_whole(&node, SCOPE, &wl.reference, wl.reference.end);
        }
    }
}

/// A long run with a cut armed at a random step and a random operation —
/// including operations past the end of the run, so some runs end in a
/// clean crash instead. Ingest flushes on its own here, so cuts land
/// inside backpressure flushes as well as explicit ones.
#[test]
fn random_cuts_across_a_long_run_always_recover() {
    let mut crashed = 0u32;
    let mut verified_pages = 0usize;
    for seed in 0..240u64 {
        let after = seed % 2 == 0;
        let dev = Arc::new(CutDevice::new(seed.wrapping_mul(2_654_435_761) | 1, after));
        let config = squeezed(3, 5);
        let node = boot(dev.clone(), config.clone());
        create_scope(&node, SCOPE);
        let mut wl = Workload::new(seed.wrapping_mul(94_696_979) | 1);

        let steps = 30 + seed % 50;
        let arm_at = seed % steps;
        let arm_ops = 1 + (seed * 13) % 40;
        let mut hit = false;
        for step in 0..steps {
            if step == arm_at {
                dev.arm(arm_ops);
            }
            let outcome = match step % 5 {
                0..=2 => wl.push_random(&node, SCOPE).map(|_| ()),
                3 => catch_up(&node, SCOPE, wl.reference.end),
                _ => flush(&node, SCOPE).map(|_| ()),
            };
            match outcome {
                Ok(()) => {}
                Err(StoreError::Io(_)) => {
                    assert!(dev.crashed(), "seed {seed}, step {step}: only a crash fails an operation");
                    hit = true;
                    break;
                }
                Err(e) => panic!("seed {seed}, step {step}: {e:?}"),
            }
        }
        let end = wl.reference.end;
        drop(node);
        if hit {
            crashed += 1;
        } else {
            // Nothing in flight: the power simply goes out.
            dev.plain_crash(seed + 1);
        }
        dev.revive();

        let node = boot(dev.clone(), config);
        verified_pages += verify_whole(&node, SCOPE, &wl.reference, end);
        let stats = node.pages.stats(SCOPE).unwrap();
        assert_eq!(stats.ingested, end, "seed {seed}");
        assert!(stats.materialized <= end, "seed {seed}");
    }
    assert!(crashed >= 20, "the run crashed often enough to mean something: {crashed}");
    assert!(verified_pages > 2000, "pages verified after crashes: {verified_pages}");
}

/// The log source the page store reads through, recording every retention
/// value the store reports — as it reports it.
struct RecordingLog {
    inner: Arc<dyn LogSource>,
    reported: Mutex<Vec<Lsn>>,
}

impl LogSource for RecordingLog {
    fn read_log<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        from: Lsn,
        max_bytes: usize,
    ) -> LocalBoxFuture<'a, Result<LogChunk, StoreError>> {
        self.inner.read_log(ctx, scope, from, max_bytes)
    }

    fn subscribe(
        &self,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
        from: Lsn,
    ) -> Box<dyn LogSubscription> {
        self.inner.subscribe(ctx, scope, from)
    }

    fn set_retained_from<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if matches!(holder, RetentionHolder::PageStore { .. }) {
                self.reported.lock().unwrap().push(lsn);
            }
            self.inner.set_retained_from(ctx, scope, holder, lsn).await
        })
    }

    fn release_holder<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        self.inner.release_holder(ctx, scope, holder)
    }

    fn retained_floor(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        self.inner.retained_floor(scope)
    }

    fn retention_holders(&self, scope: ScopeId) -> Result<Vec<HolderReport>, StoreError> {
        self.inner.retention_holders(scope)
    }

    fn append_before_time<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        ts: Hlc,
    ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>> {
        self.inner.append_before_time(ctx, scope, ts)
    }
}

/// §4.4 promise 5, at every cut: a holder must make its own restart point
/// durable before it moves its retention value past it. If the page store
/// reported a value and then lost it — the manifest not yet durable, the
/// crash arriving — the log below it could be discarded, and the store
/// would restart onto pages it can no longer rebuild.
#[test]
fn retention_is_reported_only_after_the_manifest_names_it() {
    let ops = flush_ops(true);
    let config = Config {
        report_retention: true,
        ..plain()
    };
    let mut runs_that_reported_twice = 0u32;

    for after in [false, true] {
        for cut in 1..=ops + 1 {
            let dev = Arc::new(CutDevice::new(cut.wrapping_mul(104_729) ^ u64::from(after), after));
            let log = open_log(dev.clone());
            let recorder = Arc::new(RecordingLog {
                inner: log.clone(),
                reported: Mutex::new(Vec::new()),
            });
            let source: Arc<dyn LogSource> = recorder.clone();
            let node = node_of(dev.clone(), log, open_pages(dev.clone(), source, config.clone()));
            create_scope(&node, SCOPE);

            let mut wl = Workload::new(0xC0FFEE);
            for _ in 0..GROUPS {
                wl.push_random(&node, SCOPE).expect("appends");
            }
            catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
            flush(&node, SCOPE).expect("the first flush lands");
            let first = node.pages.materialized_lsn(SCOPE).unwrap();
            for _ in 0..MORE {
                wl.push_random(&node, SCOPE).expect("appends");
            }
            catch_up(&node, SCOPE, wl.reference.end).expect("ingests");

            dev.arm(cut);
            let last = flush(&node, SCOPE);
            let live_floor = node.log.retained_floor(SCOPE);
            drop(node);
            let reported = recorder.reported.lock().unwrap().clone();

            dev.revive();
            let reopened = boot(dev.clone(), config.clone());
            let materialized = reopened.pages.materialized_lsn(SCOPE).expect("the scope survived");
            let at = format!("cut {cut} of {ops}, after={after}");

            assert_eq!(
                reported.first(),
                Some(&first),
                "{at}: the first flush reported what it materialized"
            );
            // The law: nothing was reported that the crash could take back.
            for lsn in &reported {
                assert!(
                    materialized >= *lsn,
                    "{at}: reported {lsn:?} as materialized, but after the crash the manifest names {materialized:?}"
                );
            }
            if reported.len() == 2 {
                // The second report came after its manifest write, so
                // that write survived whatever the crash did afterwards.
                assert_eq!(materialized, wl.reference.end, "{at}");
                runs_that_reported_twice += 1;
            }
            if cut > ops {
                // No cut at all: the flush landed, both reports were made,
                // and the live log store holds the value.
                assert!(
                    matches!(last, Ok(FlushOutcome::Flushed { .. })),
                    "{at}: got {last:?}"
                );
                assert_eq!(reported.len(), 2, "{at}");
                assert!(
                    matches!(live_floor, Ok(floor) if floor == wl.reference.end),
                    "{at}: the live log store's floor is {live_floor:?}"
                );
            }
            verify_whole(&reopened, SCOPE, &wl.reference, wl.reference.end);

            // A restarted store reports its holder again. It cannot assume
            // anyone remembered it on its behalf: the log store keeps
            // holders in memory and writes them only with its next manifest.
            flush(&reopened, SCOPE).expect("{at}: the restarted store flushes");
            let now = reopened.pages.materialized_lsn(SCOPE).expect("the scope");
            assert!(
                matches!(reopened.log.retained_floor(SCOPE), Ok(floor) if floor == now),
                "{at}: after the restart the log store's floor is the store's materialized LSN"
            );
        }
    }
    assert!(
        runs_that_reported_twice > 0,
        "some cut landed after the second report: {runs_that_reported_twice}"
    );
}

/// S19 at the store's scale, end to end: a failed sync fails the scope's
/// flushes for the life of the instance, and `open` will not build on the
/// file whose contents are unknown.
#[test]
fn a_failed_sync_is_permanent_for_the_instance() {
    let dev = Arc::new(MemBlockDevice::new());
    let config = no_backpressure(8);
    let node = boot(dev.clone(), config.clone());
    create_scope(&node, SCOPE);
    let mut wl = Workload::new(3);
    for _ in 0..10 {
        wl.push_random(&node, SCOPE).expect("appends");
    }
    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");

    dev.inject_sync_failure();
    assert!(matches!(flush(&node, SCOPE), Err(StoreError::Io(_))));
    assert!(
        matches!(flush(&node, SCOPE), Err(StoreError::Io(_))),
        "retrying proves nothing: a failed sync is permanent"
    );
    drop(node);

    let log = open_log(dev.clone());
    let source: Arc<dyn LogSource> = log.clone();
    let reopened = block_on(PageStore::open(
        dev.clone(),
        source,
        HeapInsertRedo::new(),
        config,
    ));
    assert!(
        matches!(reopened, Err(StoreError::Io(_))),
        "open refuses to build on the failed file"
    );
}
