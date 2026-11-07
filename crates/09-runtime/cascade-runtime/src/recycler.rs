//! The WAL segment recycler (T4.3, FR-32): the housekeeper *decides* from
//! the per-DB truncatable frontier, and — like the checkpointer —
//! orchestrates only. Each tick, [`WalRecycler`] `try_send`s a
//! [`RecycleSignal`] carrying the frontier snapshot down the DB's
//! *dedicated recycle channel*, whose receiving end lives on the
//! designated reactor of the DB's WAL-home NUMA (boot resolves the home
//! *before* the housekeepers spawn — never at tick time, which would put
//! a file read on the housekeeper and break the no-I/O property). The
//! channel is the isolation: SPSC means the recycler cannot share the
//! checkpoint fanout's senders, so every background signal family owns a
//! channel, and a recycle signal is structurally unable to reach any
//! reactor but its DB's home one (§6.2/§9).
//!
//! The worker-side [`RecycleSite`] computes the boundary —
//! [`cascade_wal::recycle_decision`], strictly-below the frontier,
//! quantized to whole windows — and counts. The physical act
//! (rotate-and-retire) lands with the segmented WAL (§8.2); until then,
//! and while the live frontier is pinned at 0 (no worker BPM: rows live
//! only in the log, so *nothing* is reclaimable — the checkpointer's
//! safety argument continues here), a live recycle reclaims zero bytes
//! and the log file is untouched, which the integration test pins
//! byte-for-byte.
//!
//! The recycler NEVER advances the frontier — only the checkpointer
//! writes it. The recycler is a pure consumer of the snapshot.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use cascade_types::{DatabaseId, Lsn};
use glommio::channels::shared_channel::{ConnectedReceiver, ConnectedSender, SharedSender};

use crate::{
    checkpointer::TruncatableLsn,
    orchestrator::{Tick, TickJob},
};

/// One recycle order (FR-32): reclaim `db`'s WAL strictly below `below`.
/// Rides the dedicated recycle channel — no other message type ever does,
/// so the worker-side loop needs no dispatch and no other loop can be
/// handed one.
#[derive(Debug)]
pub struct RecycleSignal {
    pub db: DatabaseId,
    pub below: Lsn,
}

/// The housekeeper-side recycler (FR-32's orchestrating half): a tick job
/// signaling the DB's WAL-home reactor with the frontier snapshot. One DB
/// per instance (v1: the dev database; the per-DB registry arrives with
/// multi-db). `try_send` only — a full control channel drops the signal
/// (counted); recycle is idempotent per frontier snapshot, so a dropped
/// signal costs one tick of staleness, nothing more.
pub struct WalRecycler {
    db: DatabaseId,
    sender: ConnectedSender<RecycleSignal>,
    truncatable: Arc<TruncatableLsn>,
    stats: RecycleStats,
}

impl WalRecycler {
    /// `sender` is the DB's recycle-channel endpoint, connected on the
    /// serving executor (router assembly connects boot's `SharedSender`);
    /// `stats` is the boot-built counter set `Runtime` exposes.
    pub fn new(
        db: DatabaseId,
        sender: ConnectedSender<RecycleSignal>,
        truncatable: Arc<TruncatableLsn>,
        stats: RecycleStats,
    ) -> Self {
        WalRecycler {
            db,
            sender,
            truncatable,
            stats,
        }
    }
}

impl TickJob for WalRecycler {
    fn name(&self) -> &str {
        "wal-recycler"
    }

    fn tick(&mut self, _info: &Tick) {
        // The snapshot: everything downstream decides from THIS value, so
        // a signal is reproducible and a concurrent advance cannot expand
        // an in-flight recycle (strictly-below is per-snapshot).
        let below = self.truncatable.get();
        self.stats.signals.fetch_add(1, Ordering::Relaxed);
        if self
            .sender
            .try_send(RecycleSignal {
                db: self.db,
                below,
            })
            .is_err()
        {
            self.stats.refused.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The worker-side recycle site: what a [`RecycleSignal`] acts on. The
/// boundary math is the crate's ([`cascade_wal::recycle_decision`]); the
/// physical act is documented there — until the segmented WAL, a recycle
/// counts and reclaims nothing (frontier 0 live, so the answer is always
/// zero in production).
pub struct RecycleSite {
    passes: Arc<AtomicU64>,
    /// Bytes physically reclaimed, ever. Zero until the segmented WAL
    /// exists — the decision's reclaimable prefix is deliberately not
    /// counted (post-M5 review low #9: it was re-added every pass).
    reclaimed_bytes: Arc<AtomicU64>,
}

impl RecycleSite {
    pub fn new(passes: Arc<AtomicU64>, reclaimed_bytes: Arc<AtomicU64>) -> Self {
        RecycleSite {
            passes,
            reclaimed_bytes,
        }
    }

    /// One recycle for `db` below `below`. `_db` keys the (future)
    /// physical act per database — v1's decision math is per-call, so the
    /// count is the observable; per-DB isolation is enforced upstream by
    /// the targeting (only the home reactor's channel ever receives
    /// signals for its own DB).
    ///
    /// `reclaimed_bytes` counts bytes ACTUALLY reclaimed — and until the
    /// segmented WAL the physical act reclaims nothing (the debug engine's
    /// log is live from byte 0), so it never moves however large the
    /// decision's reclaimable prefix is. The old line added that prefix on
    /// every pass: once any caller's frontier left 0, the same unreclaimed
    /// bytes would re-add per tick, forever — a counter lying upward
    /// (post-M5 review low #9). The segmented WAL adds the real increments
    /// where the truncation actually runs.
    pub fn run_recycle(&self, _db: DatabaseId, below: Lsn) -> cascade_wal::RecycleDecision {
        let decision = cascade_wal::recycle_decision(below, cascade_wal::RECYCLE_SEGMENT_BYTES);
        self.passes.fetch_add(1, Ordering::Relaxed);
        decision
    }
}

/// The worker-side loop draining the dedicated recycle channel — the
/// only traffic it ever carries, so there is nothing to dispatch. Runs
/// as a sibling task on the worker's own ring (like the checkpoint
/// control loop); the loop dies with the ring, or sooner when the
/// sender side drops (a boot with no recycler closes it at startup).
pub async fn recycle_loop(recv: ConnectedReceiver<RecycleSignal>, site: RecycleSite) {
    while let Some(sig) = recv.recv().await {
        site.run_recycle(sig.db, sig.below);
    }
}

/// Aggregates for `Runtime` — the same counters the tick job and the
/// workers' sites increment. `None` on boots with no listener or no WAL:
/// nothing orchestrates (D9) or nothing exists to recycle.
#[derive(Debug, Clone)]
pub struct RecycleStats {
    signals: Arc<AtomicU64>,
    refused: Arc<AtomicU64>,
    passes: Arc<AtomicU64>,
    reclaimed_bytes: Arc<AtomicU64>,
}

impl RecycleStats {
    /// The boot path's constructor: fresh signal/refusal counters over the
    /// one pass/reclaimed pair every worker's [`RecycleSite`] increments
    /// (boot hands the same `Arc`s to both sides).
    pub fn new(passes: Arc<AtomicU64>, reclaimed_bytes: Arc<AtomicU64>) -> Self {
        RecycleStats {
            signals: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
            passes,
            reclaimed_bytes,
        }
    }

    /// Recycle signals sent to the WAL-home reactor since boot.
    pub fn signals(&self) -> u64 {
        self.signals.load(Ordering::Relaxed)
    }

    /// Signals dropped because the recycle channel was full — the
    /// housekeeper never blocks (FR-32's orchestrates-only half).
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Recycles the home reactor ran since boot.
    pub fn passes(&self) -> u64 {
        self.passes.load(Ordering::Relaxed)
    }

    /// Bytes the recycler computed reclaimable, summed since boot. Zero
    /// is the live engine's standing truth until the frontier leaves 0.
    pub fn reclaimed_bytes(&self) -> u64 {
        self.reclaimed_bytes.load(Ordering::Relaxed)
    }
}

/// The boot→serving-thread ingredient bundle (all `Send`, unlike the job
/// itself, which owns a connected sender and never crosses a thread). The
/// `sender` half of the recycle channel boot created; the matching
/// receiver goes to the home NUMA's designated worker.
pub struct RecycleInit {
    pub db: DatabaseId,
    pub sender: SharedSender<RecycleSignal>,
    pub truncatable: Arc<TruncatableLsn>,
    pub stats: RecycleStats,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The receiver endpoint on its own executor thread, draining until
    /// the sender side drops (which closes the channel and ends the
    /// loop). Shared_channel endpoints must connect on *different*
    /// executors — a sequential same-executor pair deadlocks spinning
    /// (see handoff.rs's module doc) — and the receiver must outlive the
    /// sender's connect: `Connector::poll` unwraps the peer's sleep
    /// notifier, so a peer executor that already exited panics the
    /// connect. Recv-until-closed keeps the peer alive for both.
    fn spawn_rx_until_closed<T: Send + Sized + 'static>(
        rx: glommio::channels::shared_channel::SharedReceiver<T>,
    ) -> std::thread::JoinHandle<Vec<T>> {
        std::thread::Builder::new()
            .name("recycler-rx-test".into())
            .spawn(move || {
                let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("recycler-rx-ring")
                    .make()
                    .expect("executor builds");
                ex.run(async move {
                    let rx = rx.connect().await;
                    let mut got = Vec::new();
                    while let Some(msg) = rx.recv().await {
                        got.push(msg);
                    }
                    got
                })
            })
            .expect("rx thread spawns")
    }

    /// Low #9 (post-M5 review): the counter says what was PHYSICALLY
    /// reclaimed — nothing, until the segmented WAL. At a frontier far
    /// above zero the decision's reclaimable prefix is large; the old code
    /// added it every pass, so the same unreclaimed bytes accumulated per
    /// tick. The decision itself still reports the prefix honestly — only
    /// the counter kept still.
    #[test]
    fn reclaimed_bytes_stays_zero_while_the_act_reclaims_nothing() {
        let passes = Arc::new(AtomicU64::new(0));
        let reclaimed = Arc::new(AtomicU64::new(0));
        let site = RecycleSite::new(Arc::clone(&passes), Arc::clone(&reclaimed));
        let decision = site.run_recycle(DatabaseId(0), Lsn(1 << 30));
        assert_eq!(
            decision.reclaimable_bytes,
            1 << 30,
            "the DECISION still reports the full reclaimable prefix"
        );
        assert_eq!(passes.load(Ordering::Relaxed), 1, "the pass ran");
        assert_eq!(
            reclaimed.load(Ordering::Relaxed),
            0,
            "nothing was physically reclaimed, so the counter never moved"
        );
    }

    /// FR-32's targeting, pinned: only the WAL-home reactor's channel
    /// ever sees recycle signals — the other NUMA's designated reactor
    /// receives nothing (cross-DB/cross-NUMA touch is structural, not a
    /// filter the job could forget).
    #[test]
    fn recycler_signals_only_the_home_reactor() {
        let (tx_home, rx_home) =
            glommio::channels::shared_channel::new_bounded::<RecycleSignal>(8);
        let (tx_other, rx_other) =
            glommio::channels::shared_channel::new_bounded::<RecycleSignal>(8);
        let truncatable = Arc::new(TruncatableLsn::new());
        truncatable.advance_to(Lsn(500));
        let stats = RecycleStats::new(Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));

        let rx_home_thread = spawn_rx_until_closed(rx_home);
        let rx_other_thread = spawn_rx_until_closed(rx_other);

        let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("recycler-test")
            .make()
            .expect("executor builds");
        let db = DatabaseId(7);
        let (signals, refused) = ex.run(async move {
            let mut job =
                WalRecycler::new(db, tx_home.connect().await, Arc::clone(&truncatable), stats);
            for ordinal in 1..=2u64 {
                TickJob::tick(
                    &mut job,
                    &Tick {
                        ordinal,
                        late: Duration::ZERO,
                    },
                );
            }
            // The other endpoint stays connected-but-silent: its receiver
            // parks in recv-until-closed until this block drops both
            // senders, then reports whatever crossed (nothing).
            drop(tx_other.connect().await);
            (job.stats.signals(), job.stats.refused())
        });
        assert_eq!(signals, 2);
        assert_eq!(refused, 0);

        let got = rx_home_thread.join().expect("home rx joins");
        assert_eq!(got.len(), 2);
        for sig in got {
            assert_eq!(sig.db, DatabaseId(7), "the signal names its db");
            assert_eq!(sig.below, Lsn(500), "the snapshot frontier rides the signal");
        }
        assert!(
            rx_other_thread.join().expect("other rx joins").is_empty(),
            "a non-home reactor must never receive recycle signals"
        );
    }

    /// The worker-side loop under real traffic: each signal runs its own
    /// pass, and the counter stays honest — db 1's `below` (2×SEG) yields
    /// a decision whose reclaimable prefix is 2×SEG, db 2's zero frontier
    /// yields zero, and NEITHER lands in `reclaimed_bytes` because the
    /// physical act reclaims nothing until the segmented WAL (low #9; the
    /// old assertion here pinned the re-add — "db 1 reclaimed its full
    /// windows" — which was the defect). The decision math itself is
    /// pinned in cascade-wal's recycle tests; the honesty unit test above
    /// pins the counter.
    #[test]
    fn recycle_loop_runs_each_signal_and_reclaimed_bytes_stay_honest() {
        let (tx, rx) = glommio::channels::shared_channel::new_bounded::<RecycleSignal>(8);
        let passes = Arc::new(AtomicU64::new(0));
        let reclaimed = Arc::new(AtomicU64::new(0));

        // The sender on its own executor thread (different-executor
        // connect rule); it fires the two signals and drops, closing the
        // channel — which is what ends the loop under test.
        let sender_thread = std::thread::Builder::new()
            .name("recycle-loop-tx-test".into())
            .spawn(move || {
                let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("recycle-loop-tx-ring")
                    .make()
                    .expect("executor builds");
                ex.run(async move {
                    let sender = tx.connect().await;
                    let _ = sender.try_send(RecycleSignal {
                        db: DatabaseId(1),
                        below: Lsn(2 * cascade_wal::RECYCLE_SEGMENT_BYTES),
                    });
                    let _ = sender.try_send(RecycleSignal {
                        db: DatabaseId(2),
                        below: Lsn(0),
                    });
                });
            })
            .expect("sender thread spawns");

        let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("recycle-loop-test")
            .make()
            .expect("executor builds");
        let (recycles, reclaimed_bytes) = ex.run(async move {
            let recv = rx.connect().await;
            let site = RecycleSite::new(Arc::clone(&passes), Arc::clone(&reclaimed));
            recycle_loop(recv, site).await;
            (
                passes.load(Ordering::Relaxed),
                reclaimed.load(Ordering::Relaxed),
            )
        });

        sender_thread.join().expect("sender thread joins");
        assert_eq!(recycles, 2);
        assert_eq!(
            reclaimed_bytes, 0,
            "nothing physically reclaimed: db 1's 2×SEG reclaimable prefix \
             must never enter the counter, whatever its frontier is"
        );
    }

    /// Same syscall-layer property as the fanout (T4.2): ticking the
    /// recycler leaves the calling thread's kernel-accounted storage I/O
    /// exactly where it was — the live integration test repeats it on the
    /// real serving housekeeper. Measured on a dedicated runner thread
    /// (see the fanout test's comment): the test harness's main-thread
    /// stdout flushes would pollute the main thread's write_bytes.
    #[test]
    fn recycler_tick_issues_no_storage_io_on_the_caller() {
        // Capacity ≥ tick count: this pins the no-I/O property, not the
        // back-pressure refusal path (try_send drops on a full buffer).
        let (tx, rx) = glommio::channels::shared_channel::new_bounded::<RecycleSignal>(32);
        let truncatable = Arc::new(TruncatableLsn::new());
        let stats = RecycleStats::new(Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let rx_thread = spawn_rx_until_closed(rx);
        let runner = std::thread::Builder::new()
            .name("recycler-io-probe".into())
            .spawn(move || {
                let tid =
                    crate::checkpointer::tid_named("recycler-io-probe").expect("runner finds its own tid");
                let before = crate::checkpointer::task_io_bytes(tid).expect("task io readable");
                let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("recycler-io-test")
                    .make()
                    .expect("executor builds");
                let signals = ex.run(async move {
                    let mut job = WalRecycler::new(
                        DatabaseId(1),
                        tx.connect().await,
                        Arc::clone(&truncatable),
                        stats,
                    );
                    for _ in 0..16 {
                        TickJob::tick(
                            &mut job,
                            &Tick {
                                ordinal: 1,
                                late: Duration::ZERO,
                            },
                        );
                    }
                    job.stats.signals()
                });
                let after = crate::checkpointer::task_io_bytes(tid).expect("task io readable");
                assert_eq!(after.read_bytes, before.read_bytes, "no storage reads");
                assert_eq!(after.write_bytes, before.write_bytes, "no storage writes");
                signals
            })
            .expect("runner thread spawns");
        let signals = runner.join().expect("runner joins");
        assert_eq!(signals, 16);
        assert_eq!(rx_thread.join().expect("rx thread joins").len(), 16);
    }
}
