//! The checkpointer (T4.2, FR-31): the housekeeper *decides*, a worker
//! *flushes in-pool*, the WAL-truncatable frontier advances — and the
//! housekeeper thread never issues storage I/O.
//!
//! Division of labor (§9): the housekeeper-side [`CheckpointFanout`] is a
//! tick job that signals each NUMA pool's *designated reactor* — the first
//! worker on that pool's NUMA — over its dedicated control channel
//! ([`crate::handoff::ControlMsg`]; the formal `submit_to` primitive is
//! T5.1, and this channel is the staged stand-in with the same topology).
//! The worker side runs [`flush_pass`] over its own reactor-local BPM:
//! iterate dirty pages, write each back through [`cascade_bpm`]'s
//! WAL-before-data gate (T3.2 — the exact gate a single-page flush uses),
//! and advance the per-DB [`TruncatableLsn`]. The housekeeper only ever
//! `try_send`s a signal — it never blocks on, reads, or touches another
//! NUMA's Tier B state, and the `/proc`-based counters below exist so a
//! test can prove the no-storage-I/O half at the *syscall* layer (kernel
//! per-task accounting — an abstraction change cannot hide behind it).
//!
//! ## The truncatable frontier's conservative rule
//!
//! `TruncatableLsn` is the promise "WAL strictly below this LSN is not
//! needed to reconstruct durable state" — what the T4.3 recycler may
//! reclaim. Advancing it follows PostgreSQL's checkpoint redo rule in its
//! conservative form: the frontier moves to the **minimum stamp over every
//! page dirty at pass start** (refused/pinned pages carry their stamp into
//! the minimum — their accumulated state still lives only in the log),
//! *only* when at least one stamped page was seen; unlogged pages force no
//! advance (their coverage relation to the WAL is undefined here), and a
//! pass with nothing dirty advances nothing.
//!
//! Two deliberate under-advances, both load-bearing until later
//! milestones replace them:
//!
//! 1. No FPW (full-page-image) exists yet (§8.1's concurrent BPM owns
//!    it), so the redo rule is the *conservative* form.
//! 2. The debug engine's only durable state is the WAL itself (rows
//!    replay from offset 0, T3.3), and live workers host no BPM yet — so
//!    a live pass scans nothing and the frontier *stays at 0*: nothing is
//!    recyclable, and the recycler must respect that. The unit tests
//!    exercise the real math against a real store; the live test asserts
//!    the frontier does not move.

use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use cascade_bpm::{Bpm, BpmError};
use cascade_storage::PageStore;
use cascade_types::{DatabaseId, Lsn};
use glommio::channels::shared_channel::ConnectedReceiver;

use crate::handoff::ControlMsg;

/// The per-DB WAL-truncatable frontier (FR-31): monotonic, `Relaxed` —
/// readers observe progress late, never backwards. v1 holds one cell for
/// the dev database; the per-DB registry arrives with multi-db.
pub struct TruncatableLsn(AtomicU64);

impl TruncatableLsn {
    pub fn new() -> Self {
        TruncatableLsn(AtomicU64::new(0))
    }

    /// The current frontier. WAL strictly below this LSN is reclaimable
    /// (T4.3); `0` means nothing is.
    pub fn get(&self) -> Lsn {
        Lsn(self.0.load(Ordering::Relaxed))
    }

    /// Moves the frontier forward, never back (a stale pass must not
    /// un-advance a fresher one's progress).
    pub fn advance_to(&self, lsn: Lsn) {
        self.0.fetch_max(lsn.0, Ordering::Relaxed);
    }
}

impl Default for TruncatableLsn {
    fn default() -> Self {
        Self::new()
    }
}

/// What one in-pool pass did. `floor` is the minimum stamp over the
/// dirty-at-start pages (the value the frontier moved to), `None` when
/// nothing stamped was dirty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushReport {
    pub scanned: usize,
    pub flushed: usize,
    pub refused: usize,
    pub floor: Option<Lsn>,
}

/// One in-pool flush pass (FR-31's worker half): iterate this pool's dirty
/// pages for `db`, write each back through the T3.2 gate, advance the
/// frontier per the conservative redo rule. Runs on the pool's designated
/// reactor with THAT reactor's own store passed per call — the BPM is
/// `Send + Sync` (shared across the NUMA's cores, T6.2) but owns no store;
/// the per-call `&impl PageStore` is what keeps "in-pool" structural
/// rather than aspirational.
///
/// A page stamped beyond the writer's durable watermark parks the pass on
/// the gate until durability catches up — the same wait a committing
/// session takes, and the same back-pressure T3.2's single-page flush has.
pub async fn flush_pass<S: PageStore>(
    bpm: &Bpm,
    store: &S,
    db: DatabaseId,
    trunc: &TruncatableLsn,
) -> Result<FlushReport, BpmError> {
    let dirty: Vec<_> = bpm
        .dirty_pages()
        .into_iter()
        .filter(|p| p.db == db)
        .collect();
    let scanned = dirty.len();
    // Conservative redo floor: minimum stamp over ALL dirty-at-start
    // pages — a not-yet-flushed page's stamp still constrains, because
    // its accumulated state lives only in the log until it flushes.
    // (T6.5 replaces this rule with the drained-pass rule + in-flight
    // registry — FR-48/49.)
    let mut floor: Option<Lsn> = None;
    let mut flushed = 0;
    let mut refused = 0;
    for page in dirty {
        let stamp = bpm.page_lsn(&page).flatten();
        // Unlogged pages neither set nor block the floor; they still get
        // written back (fresh-load semantics need no WAL gate).
        if let Some(l) = stamp {
            floor = Some(match floor {
                Some(m) => m.min(l),
                None => l,
            });
        }
        if bpm.flush_page(store, page).await? {
            flushed += 1;
        } else {
            refused += 1;
        }
    }
    if let Some(f) = floor {
        trunc.advance_to(f);
    }
    Ok(FlushReport {
        scanned,
        flushed,
        refused,
        floor,
    })
}

/// The worker-side checkpoint site: what a control-channel receiver acts
/// on. The pass counter is the live observable ("passes ran on the
/// worker"); the BPM arrives with §8.1's concurrent BPM — until then a
/// pass counts and scans nothing, which is exactly why the live frontier
/// must stay at 0 (the module doc's safety argument).
pub struct CheckpointSite {
    passes: Arc<AtomicU64>,
}

impl CheckpointSite {
    pub fn new(passes: Arc<AtomicU64>) -> Self {
        CheckpointSite { passes }
    }

    /// Run one pass. The BPM hook lands with the §8.1 concurrent BPM:
    /// `flush_pass(&bpm, db, trunc).await` over this reactor's own pool.
    pub async fn run_pass(&self) {
        self.passes.fetch_add(1, Ordering::Relaxed);
    }
}

/// The worker's control-plane loop: a dedicated channel so checkpoint
/// signals never queue behind connection handoffs (and vice versa). Runs
/// as a sibling task on the worker's own ring; the loop dies with the
/// ring.
pub async fn control_loop(recv: ConnectedReceiver<ControlMsg>, site: CheckpointSite) {
    while let Some(msg) = recv.recv().await {
        match msg {
            ControlMsg::FlushPass => site.run_pass().await,
        }
    }
}

/// Aggregates for `Runtime` — the same counters the fanout job and the
/// workers increment. `None` on a listenerless boot (nothing orchestrates
/// — D9).
#[derive(Debug, Clone)]
pub struct CheckpointStats {
    requested: Arc<AtomicU64>,
    refused: Arc<AtomicU64>,
    passes: Arc<AtomicU64>,
}

impl CheckpointStats {
    /// The boot path's constructor: fresh request/refusal counters, over
    /// the one pass counter every worker's [`CheckpointSite`] increments
    /// (boot hands the same `Arc` to both sides).
    pub fn new(passes: Arc<AtomicU64>) -> Self {
        CheckpointStats {
            requested: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
            passes,
        }
    }

    /// Flush-pass signals sent to designated reactors since boot.
    pub fn requested(&self) -> u64 {
        self.requested.load(Ordering::Relaxed)
    }

    /// Signals dropped because a worker's control channel was full — the
    /// housekeeper never blocks on them (FR-31); the next tick re-signals.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Passes workers actually ran since boot.
    pub fn passes(&self) -> u64 {
        self.passes.load(Ordering::Relaxed)
    }
}

/// The housekeeper half (FR-31's "orchestrates only"): a tick job that, on
/// each tick, signals every NUMA pool's designated reactor. `try_send`
/// only — a full control channel drops the signal (counted) rather than
/// block the housekeeper; the next tick re-signals, and a pass is
/// idempotent, so a dropped signal costs one tick of freshness, nothing
/// more. Built on the serving thread (the senders are connected against
/// its executor, right before the orchestrator's first tick can fire).
pub struct CheckpointFanout {
    /// One designated worker per NUMA pool: (numa, its connected
    /// control-channel sender), in config order.
    designated: Vec<(u32, glommio::channels::shared_channel::ConnectedSender<ControlMsg>)>,
    stats: CheckpointStats,
}

impl CheckpointFanout {
    /// `designated` holds the *first* worker on each NUMA pool, in config
    /// order (boot's construction), already connected on the serving
    /// executor; `stats` is the boot-built counter set — the same `Arc`s
    /// `Runtime` exposes and the workers' sites increment into.
    pub fn new(
        designated: Vec<(u32, glommio::channels::shared_channel::ConnectedSender<ControlMsg>)>,
        stats: CheckpointStats,
    ) -> Self {
        CheckpointFanout { designated, stats }
    }
}

impl crate::orchestrator::TickJob for CheckpointFanout {
    fn name(&self) -> &str {
        "checkpoint-fanout"
    }

    fn tick(&mut self, _info: &crate::orchestrator::Tick) {
        for (_, sender) in &self.designated {
            self.stats.requested.fetch_add(1, Ordering::Relaxed);
            // Full control channel: drop the signal, count the drop, move
            // on — never block the housekeeper on a worker (FR-31).
            if sender.try_send(ControlMsg::FlushPass).is_err() {
                self.stats.refused.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The housekeeper-no-I/O instrumentation (T4.2 test review: instrument
/// the *syscall layer*). The kernel's per-task I/O accounting
/// (`/proc/<pid>/task/<tid>/io`) counts storage bytes at the syscall —
/// network traffic never appears in `read_bytes`/`write_bytes`, and no
/// abstraction change can route around the kernel's bookkeeping.
pub struct TaskIo {
    pub read_bytes: u64,
    pub write_bytes: u64,
}

/// Kernel-accounted storage I/O for one task of this process.
pub fn task_io_bytes(tid: u32) -> io::Result<TaskIo> {
    let mut read_bytes = None;
    let mut write_bytes = None;
    let text = std::fs::read_to_string(format!("/proc/self/task/{tid}/io"))?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("read_bytes:") {
            read_bytes = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("write_bytes:") {
            write_bytes = rest.trim().parse().ok();
        }
    }
    match (read_bytes, write_bytes) {
        (Some(r), Some(w)) => Ok(TaskIo {
            read_bytes: r,
            write_bytes: w,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task io file lacked read_bytes/write_bytes",
        )),
    }
}

/// Finds a task of this process by its kernel comm name (the pinned
/// threads are named `cascade-hk-<core>`, `cascade-w-<core>` at spawn).
/// `None` when no task matches — the caller decides what that means.
pub fn tid_named(comm: &str) -> Option<u32> {
    let tasks = std::fs::read_dir("/proc/self/task").ok()?;
    for entry in tasks.filter_map(|e| e.ok()) {
        let tid = match entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        {
            Some(t) => t,
            None => continue,
        };
        let Ok(name) = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")) else {
            continue;
        };
        // comm is truncated to 15 chars — match on what fits.
        let name = name.trim_end();
        if name == comm || (comm.len() > 15 && name.starts_with(&comm[..15])) {
            return Some(tid);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncatable_frontier_is_monotonic() {
        let t = TruncatableLsn::new();
        assert_eq!(t.get(), Lsn(0));
        t.advance_to(Lsn(100));
        assert_eq!(t.get(), Lsn(100));
        // A stale pass must not drag the frontier backwards.
        t.advance_to(Lsn(50));
        assert_eq!(t.get(), Lsn(100));
        t.advance_to(Lsn(200));
        assert_eq!(t.get(), Lsn(200));
    }

    /// The receiver endpoint on its own executor thread. Shared_channel
    /// endpoints must connect on *different* executors: `connect()` awaits
    /// the peer's connect, and a pending `Connector` is re-polled on every
    /// reactor loop pass — a sequential same-executor pair deadlocks
    /// *spinning* (100% CPU, forever). The serving/worker split in
    /// production has the same shape: router-side senders, worker-side
    /// receivers.
    fn spawn_rx(
        rx: glommio::channels::shared_channel::SharedReceiver<ControlMsg>,
        count: usize,
    ) -> std::thread::JoinHandle<usize> {
        std::thread::Builder::new()
            .name("fanout-rx-test".into())
            .spawn(move || {
                let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("fanout-rx-ring")
                    .make()
                    .expect("executor builds");
                ex.run(async move {
                    let rx = rx.connect().await;
                    let mut got = 0;
                    for _ in 0..count {
                        if rx.recv().await.is_some() {
                            got += 1;
                        }
                    }
                    got
                })
            })
            .expect("rx thread spawns")
    }

    #[test]
    fn fanout_ticks_signal_each_designated_reactor() {
        let (tx, rx) = glommio::channels::shared_channel::new_bounded::<ControlMsg>(8);
        let passes = Arc::new(AtomicU64::new(0));
        let stats = CheckpointStats {
            requested: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
            passes: Arc::clone(&passes),
        };
        let rx_thread = spawn_rx(rx, 2);
        let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("fanout-test")
            .make()
            .expect("executor builds");
        let requested = ex.run(async move {
            let mut job = CheckpointFanout::new(vec![(0, tx.connect().await)], stats);
            for ordinal in 1..=2u64 {
                crate::orchestrator::TickJob::tick(
                    &mut job,
                    &crate::orchestrator::Tick {
                        ordinal,
                        late: std::time::Duration::ZERO,
                    },
                );
            }
            assert_eq!(job.stats.requested(), 2);
            assert_eq!(job.stats.refused(), 0);
            assert_eq!(job.stats.passes(), 0, "signals sent, no pass ran yet");
            2
        });
        assert_eq!(requested, 2);
        assert_eq!(
            rx_thread.join().expect("rx thread joins"),
            2,
            "every ticked signal crossed the channel"
        );
    }

    /// The syscall-layer no-I/O property in miniature: ticking the fanout
    /// job leaves the *calling thread's* kernel-accounted storage I/O
    /// exactly where it was. The live integration test repeats this on the
    /// real serving housekeeper thread.
    #[test]
    fn fanout_ticks_issue_no_storage_io_on_the_caller() {
        // Capacity ≥ tick count (a warm-up run plus the measured run): this
        // test pins the *no-I/O* property, not the back-pressure refusal
        // path — sends into a full channel with a not-yet-draining receiver
        // would drop by design (the housekeeper never blocks on a worker),
        // nondeterministically.
        let (tx, rx) = glommio::channels::shared_channel::new_bounded::<ControlMsg>(64);
        let stats = CheckpointStats {
            requested: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
            passes: Arc::new(AtomicU64::new(0)),
        };
        let rx_thread = spawn_rx(rx, 32);
        // A dedicated runner thread, not the test runner's main thread:
        // the harness prints results from the main thread, and its
        // block-buffered stdout flushes (4 KiB pages) land in the
        // kernel's write_bytes accounting — one such flush mid-window
        // flaked this test once the lib suite's output volume crossed a
        // page boundary. The runner's only job is the ticks, so its
        // counters move for exactly one reason: a storage syscall the
        // fanout must not make. The ticks run on the executor the
        // runner drives (ex.run executes its future on the calling
        // thread), so the measured thread is the real tick caller.
        let runner = std::thread::Builder::new()
            .name("fanout-io-probe".into())
            .spawn(move || {
                let tid = tid_named("fanout-io-probe").expect("runner finds its own tid");
                let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("fanout-io-test")
                    .make()
                    .expect("executor builds");
                let measured = ex.run(async move {
                    let mut job = CheckpointFanout::new(vec![(0, tx.connect().await)], stats);
                    // Warm-up: the full 16-tick sequence runs once
                    // unsampled, so every code page the measured window
                    // will touch is resident before the `before` sample.
                    // A page evicted from the cache under full-suite
                    // memory pressure faults in as a block-layer read on
                    // THIS thread — observed once as 188_416 phantom
                    // "storage reads" of warm-up code, nothing to do with
                    // ticks. Sampling after the warm-up keeps the counter
                    // delta meaning "the fanout did I/O", not "the kernel
                    // refilled the cache"; the measured window shrinks to
                    // the tick loop itself.
                    for _ in 0..16 {
                        crate::orchestrator::TickJob::tick(
                            &mut job,
                            &crate::orchestrator::Tick {
                                ordinal: 1,
                                late: std::time::Duration::ZERO,
                            },
                        );
                    }
                    let warm_requested = job.stats.requested();
                    let before = task_io_bytes(tid).expect("task io readable");
                    for _ in 0..16 {
                        crate::orchestrator::TickJob::tick(
                            &mut job,
                            &crate::orchestrator::Tick {
                                ordinal: 1,
                                late: std::time::Duration::ZERO,
                            },
                        );
                    }
                    let after = task_io_bytes(tid).expect("task io readable");
                    assert_eq!(after.read_bytes, before.read_bytes, "no storage reads");
                    assert_eq!(after.write_bytes, before.write_bytes, "no storage writes");
                    let requested = job.stats.requested() - warm_requested;
                    assert_eq!(requested, 16, "the measured run issued every signal");
                    assert_eq!(job.stats.refused(), 0, "the buffer held every signal");
                    requested
                });
                measured
            })
            .expect("runner thread spawns");
        let requested = runner.join().expect("runner joins");
        assert_eq!(requested, 16);
        assert_eq!(rx_thread.join().expect("rx thread joins"), 32);
    }

    #[test]
    fn tid_named_finds_named_threads_or_none() {
        // The test runner's main thread carries the binary's name (15-char
        // comm) — lookup by a name that cannot exist returns None without
        // falling over; that is the contract the integration tests rely on
        // (a missing thread is `None`, not a panic).
        assert_eq!(tid_named("cascade-no-such-thread"), None);
    }
}
