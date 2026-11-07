//! Graceful shutdown (T4.4, FR-8, §10.2 — verbatim, in order).
//!
//! The serving housekeeper runs the whole sequence — §9 puts background
//! orchestration there, and shutdown is background work's last shift:
//!
//! ```text
//! 1. stop accepting       — the accept loop breaks on the shutdown flag
//!                           (woken promptly by shutdown(2) on a dup of
//!                           the listening fd; a connection that raced the
//!                           flag is refused with PG 57P01, never routed);
//! 2. drain                — sessions are marked draining: each finishes
//!                           its current statement, then the NEXT one is
//!                           refused with 57P01 and the connection closes
//!                           (FR-15 cleanup retested through this path).
//!                           The wait counts *in-flight statements*, not
//!                           connections — an idle client blocks nothing,
//!                           exactly like an idle txn-free backend in PG.
//!                           (The refusal reaches a session only at its
//!                           next statement: a connection parked entirely
//!                           idle — in a transaction or not — never sees
//!                           the flag and is closed by process exit, a
//!                           bare EOF rather than 57P01. Data-safe: a
//!                           transaction commits only through its own
//!                           COMMIT being processed, so an idle
//!                           transaction has nothing unflushed — post-M5
//!                           review low #13.)
//!                           The wait is COOPERATIVE (the H2 review fix):
//!                           it sleeps on glommio's timer, yielding the
//!                           ring, so an in-flight DDL's writer keeps
//!                           running and the bounce completes mid-drain —
//!                           the first draft blocked the ring, and every
//!                           shutdown with a DDL in flight burned the
//!                           deadline, ran the acks at a zero budget, and
//!                           refused its own clean marker.
//!                           Past `drain_deadline` the rest force-abort:
//!                           the waiter is bounded (unit-tested), and the
//!                           reactor drop in step 5 closes whatever fds
//!                           survive;
//! 3+4. final flushes      — one `DrainMsg::FinalFlush` down a *dedicated
//!                           drain channel* to each designated reactor
//!                           (the third signal family, after the control
//!                           and recycle channels — SPSC forbids sharing
//!                           a sender, T4.3's lesson): the WAL-home
//!                           reactor flushes the per-DB WAL tail first
//!                           (step 3), then every designated reactor runs
//!                           one last flush pass (step 4 — advancing the
//!                           per-DB checkpoint LSN is `flush_pass`'s own
//!                           job). Each worker acks through the channel
//!                           carried in the message — the staged form of
//!                           T5.1's `submit_to` return path. The flushes
//!                           run unconditionally: a frontier pinned at 0
//!                           still passes and still acks, which is how
//!                           the trace proves the flushes RAN rather than
//!                           were skipped as "nothing dirty";
//! 5. drop reactors        — `Runtime::shutdown` joins every
//!                           housekeeper and worker thread; joining a
//!                           worker drops its ring, and a dropped ring
//!                           kills the detached session tasks and closes
//!                           their fds and the per-reactor WAL handles.
//! ```
//!
//! Every phase boundary appends to a [`ShutdownTrace`] under one mutex —
//! the integration suite asserts the whole sequence, not just the end
//! state. On a boot with no listener nothing orchestrates (D9), so
//! shutdown degenerates to the joins; the trace records exactly that.

use std::{
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, TryRecvError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use cascade_telemetry::{self, Level};
use glommio::channels::shared_channel::{ConnectedReceiver, ConnectedSender};

/// PG admin_shutdown — the §10.2 refusal code: new connections during the
/// accept-loop race window and in-flight sessions' next statements both
/// get it before their connection closes.
pub const ADMIN_SHUTDOWN: &str = "57P01";

/// The drain-plane signal from the serving housekeeper to one designated
/// reactor (T4.4). Its own channel family — the control channel stays
/// pure flush-family (T4.2) and the recycle channel stays pure recycle
/// (T4.3); SPSC means no sender is ever shared, and isolation means one
/// family's backlog can never delay another's signal.
#[derive(Debug)]
pub enum DrainMsg {
    /// §10.2 steps 3+4 on this reactor: flush the per-DB WAL tail (home
    /// only), run one final flush pass, then ack through the carried
    /// channel — the staged form of T5.1's `submit_to` return path.
    FinalFlush {
        ack: std::sync::mpsc::Sender<Result<(), String>>,
    },
}

/// What one designated reactor needs to answer [`DrainMsg::FinalFlush`].
/// Built *in-task* on the worker (the WAL handle is `Rc` — reactor-local
/// by construction), passed to [`drain_loop`].
pub struct FinalFlushCtx {
    /// This reactor's per-DB WAL writer and handle. `Some` only when the
    /// boot has a WAL at all; the handle is the worker's own.
    pub wal: Option<(Arc<cascade_wal::WalWriter>, Rc<cascade_wal::WalHandle>)>,
    /// Whether this reactor is the dev DB's WAL-home reactor — the one
    /// whose flush is §10.2 step 3 (and whose ack stamps the marker's
    /// durable LSN on the serving side).
    pub is_home: bool,
}

/// The worker-side drain loop: a sibling task next to `control_loop` and
/// `recycle_loop`, on the designated reactor's own ring. Dies when the
/// serving side drops its sender (serve returns → drain senders drop →
/// `recv` yields `None`) or with the ring itself.
pub async fn drain_loop(
    recv: ConnectedReceiver<DrainMsg>,
    site: crate::checkpointer::CheckpointSite,
    final_flush: Option<FinalFlushCtx>,
) {
    while let Some(msg) = recv.recv().await {
        match msg {
            DrainMsg::FinalFlush { ack } => {
                let outcome = async {
                    match &final_flush {
                        None => Err("no WAL on this boot".to_string()),
                        Some(ctx) if ctx.is_home => {
                            let (writer, handle) = ctx.wal.as_ref().expect("is_home implies wal");
                            // §10.2 step 3: the per-DB WAL tail, first.
                            writer
                                .flush_wal(handle, writer.filled())
                                .await
                                .map_err(|e| e.to_string())?;
                            // Then step 4: the last flush pass.
                            site.run_pass().await;
                            Ok(())
                        }
                        Some(_) => {
                            // Step 4 only: the final pass. The frontier
                            // may not move (debug engine pins it at 0) —
                            // the pass RAN, and the ack says so.
                            site.run_pass().await;
                            Ok(())
                        }
                    }
                }
                .await;
                let _ = ack.send(outcome);
            }
        }
    }
}

/// The serving side of steps 3+4: one `FinalFlush` per designated
/// reactor, home site ordered **first**. Returns `(home_present, acks)` —
/// `home_present` is the load-bearing half: the home site is identified
/// by its **flag in the tuple, not by ack slot position**, so a boot
/// whose resolved WAL home has no designated reactor (a persisted home
/// on a pooled NUMA with no workers) can never have a non-home site's
/// Ok ack mistaken for the tail flush. A refused send (`try_send` on a
/// full or closed drain channel) simply drops the ack sender — the
/// matching `recv_timeout` reports `Disconnected`, which the drain
/// records honestly rather than retrying (shutdown is one-shot).
pub fn send_final_flush(
    senders: &[(u32, bool, ConnectedSender<DrainMsg>)],
) -> (bool, Vec<Receiver<Result<(), String>>>) {
    let home_pos = senders.iter().position(|(_, is_home, _)| *is_home);
    let mut order: Vec<usize> = (0..senders.len()).collect();
    if let Some(pos) = home_pos {
        order.swap(0, pos);
    }
    let acks: Vec<Receiver<Result<(), String>>> = order
        .into_iter()
        .map(|i| {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = senders[i].2.try_send(DrainMsg::FinalFlush { ack: tx });
            rx
        })
        .collect();
    // Slot 0 is the home ack *iff* `home_pos` existed — the caller gates
    // on the returned flag, which is identity-derived, not positional.
    (home_pos.is_some(), acks)
}

/// One append per §10.2 phase boundary, ordered by real completion time —
/// the sequence assertion the plan demands. `record` also emits one
/// telemetry line per phase: operators read the same order the tests do.
#[derive(Debug, Clone, Default)]
pub struct ShutdownTrace {
    inner: Arc<Mutex<Vec<&'static str>>>,
}

impl ShutdownTrace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, phase: &'static str) {
        cascade_telemetry::log(Level::Info, "shutdown_phase", &[("phase", phase)]);
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(phase);
    }

    /// The phases in completion order.
    pub fn phases(&self) -> Vec<&'static str> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// §10.2 step 2c's waiter: resolves once every gauge is at zero
/// (`false`), or at exactly the deadline (`true` — the rest force-abort)
/// — never later, and never on a *hang* path that silently turns every
/// shutdown into the deadline path. The wait is COOPERATIVE (the H2
/// review fix): each poll sleeps on glommio's timer, which yields the
/// ring, so the sibling tasks whose completions this wait exists for —
/// above all the Tier C writer carrying an in-flight DDL — keep running
/// while it polls. The first draft blocked in `std::thread::sleep`: a
/// DDL in flight at SIGTERM could never finish (its writer sat frozen
/// mid-ring), so the gauge never dropped, the deadline always burned,
/// and the ack waits below inherited a zero budget. One millisecond of
/// granularity is far below any real drain budget.
pub async fn wait_inflight(gauges: &[Arc<AtomicU64>], deadline: Duration) -> bool {
    let end = Instant::now() + deadline;
    loop {
        if gauges.iter().all(|g| g.load(Ordering::Relaxed) == 0) {
            return false;
        }
        if Instant::now() >= end {
            return true;
        }
        glommio::timer::sleep(Duration::from_millis(1)).await;
    }
}

/// The serving thread's piece of the shutdown state — everything §10.2
/// steps 1–4 need, built at boot (all `Send`; the flag and trace are
/// shared with [`Runtime`](crate::Runtime), which owns the caller side).
pub struct ServingShutdown {
    /// The flag the accept loop polls every pass; `Runtime::shutdown`
    /// sets it and wakes a blocked `accept(2)` via a dup of the fd.
    pub flag: Arc<AtomicBool>,
    /// Set before the drain wait; sessions refuse their next statement
    /// when they see it.
    pub draining: Arc<AtomicBool>,
    /// Per-worker in-flight-statement gauges — what step 2c waits on.
    pub inflight: Vec<Arc<AtomicU64>>,
    /// §10.2's `drain_deadline` (config `drain_deadline_s`).
    pub deadline: Duration,
    pub trace: ShutdownTrace,
    /// The WAL writer and its directory, for the clean marker's durable
    /// LSN. `None` on a boot with no `wal_dir` (nothing to flush or
    /// stamp).
    pub wal: Option<(Arc<cascade_wal::WalWriter>, std::path::PathBuf)>,
}

/// The ack wait, cooperative for the same reason as [`wait_inflight`]:
/// a blocking `recv_timeout` would park this ring for the whole budget
/// (or — at the zero budget the deadline path leaves — report Timeout
/// for acks that were microseconds away), and a parked ring is exactly
/// what the flushes' sibling tasks don't need at shutdown. Mirrors
/// `recv_timeout`'s three outcomes.
async fn recv_acked(
    rx: &Receiver<Result<(), String>>,
    budget: Duration,
) -> Result<Result<(), String>, RecvTimeoutError> {
    let end = Instant::now() + budget;
    loop {
        match rx.try_recv() {
            Ok(v) => return Ok(v),
            Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
            Err(TryRecvError::Empty) => {}
        }
        if Instant::now() >= end {
            return Err(RecvTimeoutError::Timeout);
        }
        glommio::timer::sleep(Duration::from_millis(1)).await;
    }
}

/// §10.2 steps 2–4 on the serving thread, after its accept loop broke on
/// the flag ("accept-stopped" is recorded by the caller, which knows the
/// loop broke). The waits are COOPERATIVE — this is `async` on the
/// serving ring, the H2 review fix: a ring blocked in a sleep loop could
/// never complete the very work the drain waits on, so any shutdown
/// with a DDL in flight burned the deadline and refused its marker.
/// `senders` are the *connected* drain-channel senders with each site's
/// home flag — the serving side keeps its own family, so the drain
/// never contends with the orchestrator's tick jobs for a sender slot.
pub async fn run_drain(ctx: &ServingShutdown, senders: &[(u32, bool, ConnectedSender<DrainMsg>)]) {
    // Step 2: refuse new statements, wait the in-flight ones out.
    ctx.draining.store(true, Ordering::Relaxed);
    ctx.trace.record("refuse-new");
    let aborted = wait_inflight(&ctx.inflight, ctx.deadline).await;
    ctx.trace.record(if aborted {
        "force-abort"
    } else {
        "sessions-drained"
    });

    // Steps 3+4: final per-DB WAL flush (home reactor, first) and the
    // final BPM flush (every designated reactor), each acked through the
    // message-carried channel. The ack waits share ONE deadline budget —
    // a wedged set of workers delays shutdown by the deadline *once* in
    // total, then the reactor drop in step 5 finishes the job; telemetry
    // notes it rather than hanging forever.
    let (home_present, acks) = send_final_flush(senders);
    // Nothing to flush without a WAL; and without a home reactor there is
    // nothing that CAN flush one — refuse the marker on both, loudly.
    let mut wal_flushed = ctx.wal.is_none();
    if ctx.wal.is_some() && !home_present {
        cascade_telemetry::log(
            Level::Warn,
            "shutdown_no_wal_home_reactor",
            &[("detail", "resolved WAL home has no designated reactor; \
                          the tail cannot be flushed, the marker is refused")],
        );
    }
    let mut all_ok = true;
    let budget_start = Instant::now();
    for (i, rx) in acks.iter().enumerate() {
        let left = ctx.deadline.saturating_sub(budget_start.elapsed());
        match recv_acked(rx, left).await {
            Ok(Ok(())) => {
                // The home ack (slot 0 after the ordering, and
                // `home_present` says a home site actually exists) means
                // the tail flushed — step 3 done.
                if i == 0 && home_present && ctx.wal.is_some() {
                    ctx.trace.record("final-wal-flush");
                    wal_flushed = true;
                }
            }
            Ok(Err(e)) => {
                cascade_telemetry::log(
                    Level::Warn,
                    "shutdown_final_flush_failed",
                    &[("site", i.to_string().as_str()), ("error", e.as_str())],
                );
                all_ok = false;
            }
            Err(RecvTimeoutError::Timeout) => {
                cascade_telemetry::log(
                    Level::Warn,
                    "shutdown_final_flush_ack_timeout",
                    &[("site", i.to_string().as_str())],
                );
                all_ok = false;
            }
            Err(RecvTimeoutError::Disconnected) => {
                all_ok = false;
            }
        }
    }
    if all_ok {
        ctx.trace.record("final-bpm-flush");
    } else {
        // Recorded honestly: some sites never confirmed. Step 5's
        // reactor drop closes whatever they held.
        ctx.trace.record("final-flush-incomplete");
    }

    // T3.3's clean marker, stamped only after the WAL tail is known
    // durable: an unacknowledged (or failed) flush — or a boot whose
    // home site never existed — must leave the marker unwritten, so the
    // next boot treats the log as unclean and replays honestly. A wedged
    // *other* site does not block the marker — the marker vouches for
    // the WAL, which the home site alone flushed.
    if let (Some((writer, dir)), true) = (&ctx.wal, wal_flushed) {
        let db = crate::tenant::dev_context().db;
        match cascade_wal::write_clean_marker(dir, db, writer.durable()) {
            Ok(()) => ctx.trace.record("clean-marker"),
            Err(e) => cascade_telemetry::log(
                Level::Warn,
                "shutdown_clean_marker_failed",
                &[("reason", e.to_string().as_str())],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drain's waits are cooperative — run them on an executor.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("shutdown-unit-ring")
            .make()
            .expect("executor builds")
            .run(fut)
    }

    /// The trace preserves real completion order — the whole point of the
    /// sequence assertion.
    #[test]
    fn trace_records_phases_in_order() {
        let trace = ShutdownTrace::new();
        trace.record("accept-stopped");
        trace.record("refuse-new");
        trace.record("sessions-drained");
        assert_eq!(trace.phases(), ["accept-stopped", "refuse-new", "sessions-drained"]);
    }

    #[test]
    fn wait_inflight_returns_immediately_when_idle() {
        let gauges = vec![Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        let started = Instant::now();
        assert!(!block_on(wait_inflight(&gauges, Duration::from_secs(30))));
        // The bound is generous on purpose (post-M5 review flake fix): the
        // claim is against the 30 s deadline, and block_on spins up a whole
        // executor whose first poll under parallel gate load has been
        // observed at ~80 ms — far under a bound that still pins "the
        // idle fleet returned at once, not at the deadline".
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an idle fleet must not approach the deadline: {:?}",
            started.elapsed()
        );
    }

    /// The hang-path guard: a gauge stuck at one makes the waiter return
    /// `true` at (not long past) the deadline — never forever, and never
    /// silently.
    #[test]
    fn wait_inflight_bounds_the_deadline_path() {
        let gauges = vec![Arc::new(AtomicU64::new(1)), Arc::new(AtomicU64::new(0))];
        let deadline = Duration::from_millis(120);
        let started = Instant::now();
        assert!(
            block_on(wait_inflight(&gauges, deadline)),
            "stuck gauge = force-abort"
        );
        let elapsed = started.elapsed();
        // Lower bound exact (the waiter may not return early on a stuck
        // gauge), upper slack generous — a loaded executor's timer can
        // overshoot the 1 ms poll far past the old 300 ms; the claim is
        // "at the deadline, not a hang", and 2 s keeps it honest.
        assert!(
            elapsed >= deadline && elapsed < deadline + Duration::from_secs(2),
            "bounded at the deadline, not a hang: {elapsed:?}"
        );
    }

    #[test]
    fn wait_inflight_returns_early_when_the_fleet_drains() {
        let gauges = vec![Arc::new(AtomicU64::new(1)), Arc::new(AtomicU64::new(1))];
        // BOTH gauges must drop — the waiter returns only when the whole
        // fleet is idle (the first draft cleared one and rode the full
        // 30 s deadline, which is exactly the shape this test exists to
        // disprove).
        let clear0 = Arc::clone(&gauges[0]);
        let clear1 = Arc::clone(&gauges[1]);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            clear0.store(0, Ordering::Relaxed);
            clear1.store(0, Ordering::Relaxed);
        });
        let started = Instant::now();
        assert!(!block_on(wait_inflight(&gauges, Duration::from_secs(30))));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "drained mid-wait returns early: {:?}",
            started.elapsed()
        );
    }

    /// The two receiver endpoints on their own executor thread (shared
    /// channel endpoints must connect on different executors — see
    /// handoff.rs's construction rule; same-order sequential connects on
    /// one *foreign* executor are fine, the deadlock needs both endpoints
    /// on one executor). Each site answers its FinalFlush by naming
    /// itself in the ack's Err — the only way a test can see WHICH site
    /// owns which ack slot.
    fn spawn_tagging_rxs(
        rx0: glommio::channels::shared_channel::SharedReceiver<DrainMsg>,
        rx1: glommio::channels::shared_channel::SharedReceiver<DrainMsg>,
    ) -> std::thread::JoinHandle<(String, String)> {
        std::thread::Builder::new()
            .name("shutdown-drain-rx".into())
            .spawn(move || {
                let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("shutdown-drain-rx-ring")
                    .make()
                    .expect("executor builds");
                ex.run(async move {
                    let c0 = rx0.connect().await;
                    let c1 = rx1.connect().await;
                    let mut answers = Vec::new();
                    for (rx, tag) in [(c0, "site-zero"), (c1, "site-one")] {
                        match rx.recv().await {
                            Some(DrainMsg::FinalFlush { ack }) => {
                                let _ = ack.send(Err(tag.to_string()));
                                answers.push(tag.to_string());
                            }
                            None => answers.push(String::from("closed")),
                        }
                    }
                    let mut it = answers.into_iter();
                    (it.next().unwrap(), it.next().unwrap())
                })
            })
            .expect("rx thread spawns")
    }

    /// The home site is identified by its FLAG, not by its position: the
    /// home (tagged "site-one") is second in the tuple vec, yet its ack
    /// must land in slot 0, and a set with no home flag must report
    /// `home_present == false` — the guard that keeps a non-home site's
    /// Ok from ever stamping the WAL flush or the clean marker.
    #[test]
    fn send_final_flush_keys_the_home_on_identity_not_position() {
        let (tx0, rx0) = glommio::channels::shared_channel::new_bounded::<DrainMsg>(8);
        let (tx1, rx1) = glommio::channels::shared_channel::new_bounded::<DrainMsg>(8);
        let rx_thread = spawn_tagging_rxs(rx0, rx1);
        let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("shutdown-drain-test")
            .make()
            .expect("executor builds");
        let (home_present, first_is_home) = ex.run(async move {
            let c0 = tx0.connect().await;
            let c1 = tx1.connect().await;
            // Home flagged on the SECOND site.
            let (present, acks) = send_final_flush(&[(0, false, c0), (0, true, c1)]);
            let ack0 = acks[0].recv().expect("home ack");
            let _ = acks[1].recv().expect("other ack");
            (present, ack0 == Err("site-one".to_string()))
        });
        assert!(home_present, "a flagged site exists");
        assert!(
            first_is_home,
            "the flagged site's ack owns slot 0 regardless of tuple order"
        );
        let (answered0, _answered1) = rx_thread.join().expect("rx thread joins");
        assert_eq!(answered0, "site-zero", "site zero got its own signal");

        // No flag anywhere: no home — the drain must refuse the marker's
        // premise, not mistake slot 0 for the tail flush. (The receivers
        // get their FinalFlushes and answer; nobody reads the acks —
        // the senders drop when this executor does.)
        let (tx0, rx0) = glommio::channels::shared_channel::new_bounded::<DrainMsg>(8);
        let (tx1, rx1) = glommio::channels::shared_channel::new_bounded::<DrainMsg>(8);
        let rx_thread2 = spawn_tagging_rxs(rx0, rx1);
        let ex2 = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("shutdown-drain-test2")
            .make()
            .expect("executor builds");
        let home_present = ex2.run(async move {
            let c0 = tx0.connect().await;
            let c1 = tx1.connect().await;
            let (present, _acks) = send_final_flush(&[(0, false, c0), (1, false, c1)]);
            present
        });
        assert!(
            !home_present,
            "no flagged site — the drain must know the tail cannot flush"
        );
        rx_thread2.join().expect("second rx thread joins");
    }
}
