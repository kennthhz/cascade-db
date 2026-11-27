//! Concurrency. Every other suite drives the store from one thread; this
//! one puts real threads on one `PageStore` — the only writer to the log,
//! readers that make it ingest, and a flusher — because the lock order,
//! the gates under contention, and the exclusion between a lazy page read
//! and a flush's home writes have to hold with all of them running at once.
//!
//! Nothing here is timed. The threads run until the readers and the
//! flusher have done a fixed amount of work against the writer; what is
//! checked is that no path fails, panics, hangs, or leaves the store
//! serving anything other than what the format builds.
//!
//! Two things this test must not be. It must not depend on relative thread
//! speed: the writer keeps its lead over the store's ingest bounded, so a
//! read's catch-up is finite on any machine (an unthrottled writer left
//! every reader inside one endless catch-up). And a hang must fail loudly
//! and say which thread stopped, and how — not stall the binary.

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use cascade_storage_api::fake::{test_ctx, MemBlockDevice};
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::page_source::PageSource;
use cascade_storage_api::{PageKey, StoreError};
use cascade_types::{Lsn, PageNo, ScopeId, SegmentId};
use futures_lite::future::block_on;

use common::*;

/// How much concurrent activity must happen before the threads are
/// stopped.
const NEED_READS: u64 = 400;
const NEED_FLUSHES: u64 = 20;
/// The most the log may run ahead of the store's ingest.
const LEAD: u64 = 128 * 1024;
/// Slots in the per-thread progress counters.
const WRITER: usize = 0;
const READER: std::ops::Range<usize> = 1..5;
const FLUSHER: usize = 5;

/// A reader's key: any segment, and a page below where the workload gets to.
fn some_page(seed: &mut u64) -> PageKey {
    *seed ^= *seed >> 12;
    *seed ^= *seed << 25;
    *seed ^= *seed >> 27;
    *seed = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
    let pick = *seed;
    PageKey {
        segment: SegmentId(1 + (pick % SEGMENTS as u64) as u32),
        page_no: ((pick >> 32) % 48) as PageNo,
    }
}

/// Runs a thread body, and turns a panic in it into a reported failure. A
/// thread that died silently would look exactly like a hang.
fn run_guarded(who: String, failures: &mpsc::Sender<String>, body: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(body)) {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a non-string panic payload".to_string());
        let _ = failures.send(format!("{who} panicked: {message}"));
    }
}

/// Arms the hang watchdog: after 60 s it prints, per thread, the loop
/// counters it was given, then twice-sampled `/proc` thread states (parked
/// vs spinning, and on which futex), and aborts. `name` labels the slots.
fn arm_watchdog(finished: Arc<AtomicBool>, progress: Vec<Arc<AtomicU64>>, label: &'static str) {
    std::thread::spawn(move || {
        for _ in 0..600 {
            if finished.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let counts = |progress: &[Arc<AtomicU64>]| -> Vec<u64> {
            progress.iter().map(|p| p.load(Ordering::Relaxed)).collect()
        };
        eprintln!(
            "WATCHDOG: {label} hung. Loop iterations per thread: {:?}",
            counts(&progress)
        );
        let first = thread_states();
        std::thread::sleep(Duration::from_millis(1000));
        let second = thread_states();
        eprintln!(
            "WATCHDOG: a second later: {:?}\nWATCHDOG: per thread (id, user ticks then now, system ticks then now, wchan, syscall):",
            counts(&progress)
        );
        for (a, b) in first.iter().zip(second.iter()) {
            eprintln!(
                "WATCHDOG:   tid {} user {}->{} sys {}->{} wchan {} syscall {}",
                a.0, a.1, b.1, a.2, b.2, b.3, b.4
            );
        }
        std::process::abort();
    });
}

/// What every thread of this process is doing, from `/proc`: its id, user
/// and system CPU ticks, the kernel function it is sleeping in, and the
/// syscall it is blocked in. That tells a parked thread from a spinning
/// one, and which lock it is parked on. Empty where `/proc` is not there.
fn thread_states() -> Vec<(u32, u64, u64, String, String)> {
    let mut states = Vec::new();
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return states;
    };
    for task in tasks.flatten() {
        let tid = task
            .file_name()
            .to_string_lossy()
            .parse::<u32>()
            .unwrap_or(0);
        let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else {
            continue;
        };
        let Some((_, after_comm)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // After the command name: state is field 0, utime 11, stime 12.
        let tick = |i: usize| {
            fields
                .get(i)
                .and_then(|f| f.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let read = |name: &str| {
            std::fs::read_to_string(task.path().join(name))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        states.push((tid, tick(11), tick(12), read("wchan"), read("syscall")));
    }
    states.sort_by_key(|s| s.0);
    states
}

#[test]
fn readers_a_flusher_and_ingest_share_one_store() {
    let node = boot(Arc::new(MemBlockDevice::new()), squeezed(8, 12));
    create_scope(&node, SCOPE);
    let store = node.pages.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let (failures, heard) = mpsc::channel::<String>();
    let reads = Arc::new(AtomicU64::new(0));
    let flushes = Arc::new(AtomicU64::new(0));
    let progress: Vec<Arc<AtomicU64>> = (0..6).map(|_| Arc::new(AtomicU64::new(0))).collect();

    // The watchdog. If the threads are still going long after their work
    // can take, a lock or a wakeup has been lost: report who stopped, and
    // how, then abort. It stays armed until every thread has been joined.
    let finished = Arc::new(AtomicBool::new(false));
    arm_watchdog(finished.clone(), progress.clone(), "single-scope");

    // The one writer to the log. It owns the reference replay, and keeps
    // appending until told to stop — but never runs more than `LEAD` bytes
    // ahead of the store's ingest, so that no reader's catch-up is bound to
    // grow.
    let writer = {
        let stop = stop.clone();
        let store = store.clone();
        let log = node.log.clone();
        let dev = node.dev.clone();
        let failures = failures.clone();
        let progress = progress.clone();
        std::thread::spawn(move || -> Option<Workload> {
            let node = node_of(dev, log, store);
            let mut out = None;
            run_guarded("the writer".to_string(), &failures, || {
                let mut wl = Workload::new(0xC0CC_0115_7E44);
                while !stop.load(Ordering::SeqCst) {
                    let durable = node.log.durable_lsn(SCOPE).map_or(0, |d| d.0);
                    let ingested = node.pages.stats(SCOPE).map_or(0, |s| s.ingested.0);
                    if durable.saturating_sub(ingested) > LEAD {
                        std::thread::sleep(Duration::from_micros(100));
                        continue;
                    }
                    wl.push_random(&node, SCOPE).expect("the writer appends");
                    progress[WRITER].fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                out = Some(wl);
            });
            out
        })
    };

    let mut threads = Vec::new();

    // Readers. Each asks for a page as of the durable LSN it saw on its
    // *previous* look — one step stale on purpose — so that the log is
    // usually already ahead of what it asks for, the chunk it ingests runs
    // past its request, and other readers' ingests have moved pages on.
    for i in 0..4usize {
        let stop = stop.clone();
        let store = store.clone();
        let log = node.log.clone();
        let failures = failures.clone();
        let progress = progress.clone();
        let reads = reads.clone();
        threads.push(std::thread::spawn(move || {
            run_guarded(format!("reader {i}"), &failures, || {
                let span = cascade_telemetry::Span::noop();
                let ctx = test_ctx(&span, 1);
                let mut seed = 0x5EED_0000 + i as u64;
                let mut stale = Lsn(0);
                while !stop.load(Ordering::SeqCst) {
                    let Ok(current) = log.durable_lsn(SCOPE) else {
                        break;
                    };
                    let at = std::mem::replace(&mut stale, current);
                    let key = some_page(&mut seed);
                    progress[READER.start + i].fetch_add(1, Ordering::Relaxed);
                    match block_on(store.get_page(&ctx, SCOPE, key, at)) {
                        Ok(_) => {
                            reads.fetch_add(1, Ordering::Relaxed);
                        }
                        // The page moved past what this reader asked for,
                        // because another reader ingested further: the
                        // documented answer without page versions.
                        Err(StoreError::TooOld { .. }) => {}
                        Err(e) => {
                            let _ = failures.send(format!(
                                "reader {i}: page {key:?} as of {at:?}: {e:?}"
                            ));
                            break;
                        }
                    }
                    std::thread::yield_now();
                }
            });
        }));
    }

    // The flusher, in as tight a loop as it can manage: a hot-looping
    // acquirer is what a fair gate has to withstand.
    {
        let stop = stop.clone();
        let store = store.clone();
        let failures = failures.clone();
        let progress = progress.clone();
        let flushes = flushes.clone();
        threads.push(std::thread::spawn(move || {
            let span = cascade_telemetry::Span::noop();
            let ctx = test_ctx(&span, 1);
            run_guarded("the flusher".to_string(), &failures, || {
                while !stop.load(Ordering::SeqCst) {
                    progress[FLUSHER].fetch_add(1, Ordering::Relaxed);
                    match block_on(store.flush(&ctx, SCOPE)) {
                        Ok(_) => {
                            flushes.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            let _ = failures.send(format!("flusher: {e:?}"));
                            break;
                        }
                    }
                    std::thread::yield_now();
                }
            });
        }));
    }

    // Run until the readers and the flusher have done their share against
    // the writer — and the writer has actually written: on an empty scope
    // a reader's miss is nearly free, so reads can hit their quota before
    // the writer thread has run its first iteration. Stopping at once if
    // any path reports a failure.
    let mut failure = None;
    while reads.load(Ordering::Relaxed) < NEED_READS
        || flushes.load(Ordering::Relaxed) < NEED_FLUSHES
        || progress[WRITER].load(Ordering::Relaxed) < 50
    {
        if let Ok(message) = heard.try_recv() {
            failure = Some(message);
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::SeqCst);

    // Every thread must come back. A thread that does not is the hang the
    // watchdog reports — it is still armed.
    let wl = writer.join().expect("the writer thread ends");
    for t in threads {
        t.join().expect("every thread ends: no path may hang");
    }
    finished.store(true, Ordering::SeqCst);
    while let Ok(message) = heard.try_recv() {
        if failure.is_none() {
            failure = Some(message);
        }
    }
    if let Some(message) = failure {
        panic!("a concurrent path failed: {message}");
    }
    let wl = wl.expect("the writer finished its workload");

    // The threads really ran against each other.
    assert!(
        progress[WRITER].load(Ordering::Relaxed) >= 100,
        "the writer appended {} groups",
        progress[WRITER].load(Ordering::Relaxed)
    );
    for i in READER {
        assert!(
            progress[i].load(Ordering::Relaxed) >= 10,
            "reader {} looped {} times",
            i - READER.start,
            progress[i].load(Ordering::Relaxed)
        );
    }

    // And the store is exactly the replay.
    catch_up(&node, SCOPE, wl.reference.end).expect("ingests");
    flush(&node, SCOPE).expect("flushes");
    let compared = verify_whole(&node, SCOPE, &wl.reference, wl.reference.end);
    assert!(compared > 30, "pages compared: {compared}");
}

/// Two scopes under threads at once: one writer each, one reader each, and
/// a flusher serving both — the manifest, the doublewrite area and the
/// gates are shared, and a mistake in any scope-keyed bookkeeping (a
/// snapshot taken across scopes, a batch entry routed to the wrong scope's
/// pages) would show up as one scope serving the other's rows. The first
/// test covers contention on one scope; this one covers contention between
/// scopes, threaded, which no other suite does.
#[test]
fn two_scopes_under_threads_stay_separate() {
    const OTHER: ScopeId = ScopeId(8);
    let node = boot(Arc::new(MemBlockDevice::new()), squeezed(8, 12));
    create_scope(&node, SCOPE);
    create_scope(&node, OTHER);
    let store = node.pages.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let (failures, heard) = mpsc::channel::<String>();
    let reads = Arc::new(AtomicU64::new(0));
    let progress: Vec<Arc<AtomicU64>> = (0..6).map(|_| Arc::new(AtomicU64::new(0))).collect();
    // writer 7, writer 8, reader 7, reader 8, flusher.
    const W_A: usize = 0;
    const W_B: usize = 1;
    const R_A: usize = 2;
    const R_B: usize = 3;
    const FLUSH: usize = 4;

    let finished = Arc::new(AtomicBool::new(false));
    arm_watchdog(finished.clone(), progress.clone(), "two-scope");

    // Two writers, one per scope, each with its own bounded lead. Each
    // returns its workload (and its reference replay) through the join
    // handle; a writer that panicked reports through the channel instead.
    let mut writers = Vec::new();
    let mut threads = Vec::new();
    let scopes_by_slot = [(SCOPE, W_A), (OTHER, W_B)];
    for ((scope, slot), seed) in scopes_by_slot.iter().zip([11u64, 22]) {
        let stop = stop.clone();
        let store = store.clone();
        let log = node.log.clone();
        let dev = node.dev.clone();
        let failures = failures.clone();
        let progress = progress.clone();
        let (scope, slot) = (*scope, *slot);
        writers.push(std::thread::spawn(move || -> Option<Workload> {
            let node = node_of(dev, log, store);
            let mut out = None;
            run_guarded(format!("writer {scope:?}"), &failures, || {
                let mut wl = Workload::new(0x5C0FE * seed);
                while !stop.load(Ordering::SeqCst) {
                    let durable = node.log.durable_lsn(scope).map_or(0, |d| d.0);
                    let ingested = node.pages.stats(scope).map_or(0, |s| s.ingested.0);
                    if durable.saturating_sub(ingested) > LEAD {
                        std::thread::sleep(Duration::from_micros(100));
                        continue;
                    }
                    wl.push_random(&node, scope).expect("the writer appends");
                    progress[slot].fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                out = Some(wl);
            });
            out
        }));
    }

    // One reader per scope, one step stale, as in the single-scope test.
    for (scope, slot, i) in [(SCOPE, R_A, 0usize), (OTHER, R_B, 1)] {
        let stop = stop.clone();
        let store = store.clone();
        let log = node.log.clone();
        let failures = failures.clone();
        let progress = progress.clone();
        let reads = reads.clone();
        threads.push(std::thread::spawn(move || {
            run_guarded(format!("reader {scope:?}"), &failures, || {
                let span = cascade_telemetry::Span::noop();
                let ctx = test_ctx(&span, 1);
                let mut seed = 0x7EA2 + i as u64;
                let mut stale = Lsn(0);
                while !stop.load(Ordering::SeqCst) {
                    let Ok(current) = log.durable_lsn(scope) else {
                        break;
                    };
                    let at = std::mem::replace(&mut stale, current);
                    let key = some_page(&mut seed);
                    progress[slot].fetch_add(1, Ordering::Relaxed);
                    match block_on(store.get_page(&ctx, scope, key, at)) {
                        Ok(_) => {
                            reads.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(StoreError::TooOld { .. }) => {}
                        Err(e) => {
                            let _ = failures
                                .send(format!("reader {scope:?}: page {key:?} at {at:?}: {e:?}"));
                            break;
                        }
                    }
                    std::thread::yield_now();
                }
            });
        }));
    }

    // One flusher for both scopes, in as tight a loop as it can manage.
    {
        let stop = stop.clone();
        let store = store.clone();
        let failures = failures.clone();
        let progress = progress.clone();
        threads.push(std::thread::spawn(move || {
            let span = cascade_telemetry::Span::noop();
            let ctx = test_ctx(&span, 1);
            run_guarded("the flusher".to_string(), &failures, || {
                let mut turn = false;
                while !stop.load(Ordering::SeqCst) {
                    progress[FLUSH].fetch_add(1, Ordering::Relaxed);
                    let scope = if turn { SCOPE } else { OTHER };
                    turn = !turn;
                    if let Err(e) = block_on(store.flush(&ctx, scope)) {
                        let _ = failures.send(format!("flusher: {e:?}"));
                        break;
                    }
                    std::thread::yield_now();
                }
            });
        }));
    }

    // Run until both scopes have seen real concurrent work: readers well
    // past a token effort and each writer tens of groups in.
    let mut failure = None;
    while reads.load(Ordering::Relaxed) < 200
        || progress[W_A].load(Ordering::Relaxed) < 50
        || progress[W_B].load(Ordering::Relaxed) < 50
    {
        if let Ok(message) = heard.try_recv() {
            failure = Some(message);
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::SeqCst);

    let mut joined = Vec::new();
    for handle in writers {
        joined.push(handle.join().expect("every writer thread ends: no path may hang"));
    }
    for handle in threads {
        handle.join().expect("every thread ends: no path may hang");
    }
    finished.store(true, Ordering::SeqCst);
    while let Ok(message) = heard.try_recv() {
        if failure.is_none() {
            failure = Some(message);
        }
    }
    if let Some(message) = failure {
        panic!("a concurrent path failed: {message}");
    }
    for (workload, (scope, slot)) in joined.into_iter().zip(scopes_by_slot) {
        let wl = workload.unwrap_or_else(|| panic!("writer {scope:?} finished no workload"));
        assert!(
            progress[slot].load(Ordering::Relaxed) >= 50,
            "writer {scope:?} appended {} groups",
            progress[slot].load(Ordering::Relaxed)
        );
        // Each scope is exactly its own replay — never the other's rows.
        catch_up(&node, scope, wl.reference.end).expect("ingests");
        flush(&node, scope).expect("flushes");
        let compared = verify_whole(&node, scope, &wl.reference, wl.reference.end);
        assert!(compared > 10, "scope {scope:?}: pages compared {compared}");
    }
}
