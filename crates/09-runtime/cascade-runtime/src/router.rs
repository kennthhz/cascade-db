//! Router v1: accept → authenticate → route by DB NUMA affinity → hand off
//! (spec §5.1, §5.2; FR-9, FR-10, FR-11, FR-12, FR-13).
//!
//! The housekeeper thread runs its own *unpinned* executor
//! (`Placement::Unbound`): affinity is already set by `pin_current`, so the
//! thread stays CFS-scheduled (R5) while the executor gives us
//! `SharedSender::connect()` and the shared channel the fd crosses on.
//!
//! Routing resolves the connection's database placement against the Tier C
//! snapshot (FR-10 — the snapshot, not boot config), restricts the
//! candidate set (`single` → the bound NUMA's workers only, `cross` → the
//! whole machine, I13), and picks least-loaded by connection count — the
//! count component of FR-11's composite (CPU sampling + NIC tie-breaker
//! arrive in M5 per R9's two phases). A `single` DB whose bound NUMA has
//! no available worker is refused with PG `53300` naming the NUMA — never
//! silently crossed (FR-12, R10).

use std::{
    net::TcpStream,
    os::unix::io::{FromRawFd, IntoRawFd},
    sync::{
        atomic::Ordering,
        Arc,
    },
};

use glommio::{
    channels::shared_channel::{ConnectedSender, SharedSender},
    LocalExecutorBuilder, Placement as ExecutorPlacement,
};

use crate::{
    accept::AcceptLoop,
    config::{AuthConfig, NumaAffinity},
    handoff::Handoff,
    pg::{handshake, send_fatal, AuthPolicy, Session},
    tierc::{Catalog, ClusterSnapshot, Placement, TierCPlane, CANNOT_CONNECT_NOW},
};

/// PG too_many_connections — the FR-12/R10 refusal code.
const TOO_MANY_CONNECTIONS: &str = "53300";

/// One worker's route: a connected sender into that worker's channel, its
/// NUMA node, and its live connection counter (shared with the worker).
struct Route {
    sender: ConnectedSender<Handoff>,
    numa: u32,
    counter: Arc<crate::stats::WorkerCounters>,
}

pub struct Router {
    policy: AuthPolicy,
    routes: Vec<Route>,
    max_conns_per_worker: u32,
    tierc: Arc<Catalog>,
}

impl Router {
    /// Pick the least-loaded *candidate* with capacity (FR-11 count
    /// component, A7 cap, I13 candidate restriction). Ties go to the first
    /// route — deterministic. Returns `None` when the placement's candidate
    /// set has no available worker.
    fn choose(&self, placement: &Placement) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for (i, route) in self.routes.iter().enumerate() {
            // I13: single-affinity DBs consider only their bound NUMA's
            // workers; cross DBs the whole machine.
            if placement.affinity == NumaAffinity::Single
                && Some(route.numa) != placement.bound_numa
            {
                continue;
            }
            let load = route.counter.connections.load(Ordering::Relaxed);
            if load >= self.max_conns_per_worker as u64 {
                continue; // A7: per-worker availability cap
            }
            if best.map_or(true, |(_, b)| load < b) {
                best = Some((i, load));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Hand the connection to the chosen worker as a bare fd (FR-13). Takes
    /// ownership of the stream and always disposes of it exactly once: either
    /// the fd crosses, or it is closed here (with a `53300` sent first when
    /// the refusal is the affinity/cap rule, FR-12). Returns the peer address
    /// on success, for logging.
    fn route(&self, mut stream: TcpStream, session: &Session) -> Option<std::net::SocketAddr> {
        let peer = stream
            .peer_addr()
            .unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
        // FR-28's read on the routing path: one pointer copy, no lock.
        // The serving catalog is published before the accept loop opens
        // (FR-6, §10.1 step 7 → 9), so the `None` arm is defensive —
        // refuse cannot_connect_now rather than route from an absent
        // catalog.
        let Some(placement) = self
            .tierc
            .load()
            .as_ref()
            .map(|snapshot| snapshot.resolve(session.startup.database()))
        else {
            let _ = send_fatal(
                &mut stream,
                CANNOT_CONNECT_NOW,
                "the database system is starting up",
            );
            return None; // caller drops the stream → closed
        };
        let Some(idx) = self.choose(&placement) else {
            // FR-12 (R10): a single-affinity DB with no available worker on
            // its bound NUMA is refused, naming the NUMA — never silently
            // crossed. A cross DB with no capacity anywhere gets the same
            // code without a NUMA (§5.2 rules only the single case). The
            // match is on the whole placement so a `Single` row with no
            // bound NUMA cannot silently fall into the cross-flavored
            // message (unreachable for a validated config: A13 binds among
            // worker NUMAs).
            let message = match (placement.affinity, placement.bound_numa) {
                (NumaAffinity::Single, Some(numa)) => format!(
                    "single-affinity database {:?} is bound to NUMA {numa}, \
                     which has no available workers",
                    session.startup.database().unwrap_or("dev")
                ),
                (NumaAffinity::Single, None) => format!(
                    "single-affinity database {:?} has no bound NUMA with available workers",
                    session.startup.database().unwrap_or("dev")
                ),
                (NumaAffinity::Cross, _) => {
                    "all workers are at their connection capacity".to_owned()
                }
            };
            let _ = send_fatal(&mut stream, TOO_MANY_CONNECTIONS, &message);
            return None; // caller drops the stream → closed
        };
        // Admitted race (T2.4 code review): the count may move between the
        // check above and the handoff — the cap is best-effort at the
        // router. A handoff that lands on an over-cap worker is still
        // served; a full channel (below) closes the fd honestly.
        let fd = stream.into_raw_fd();
        let handoff = Handoff {
            fd,
            peer,
            user: session.startup.user().unwrap_or_default().to_owned(),
            database: session.startup.database().map(str::to_owned),
        };
        match self.routes[idx].sender.try_send(handoff) {
            Ok(()) => Some(peer),
            Err(dead) => {
                // Channel full or worker gone: the fd must not leak. Recover
                // it from the error and close.
                let fd = match dead {
                    glommio::GlommioError::Closed(glommio::ResourceType::Channel(h)) => h.fd,
                    glommio::GlommioError::WouldBlock(glommio::ResourceType::Channel(h)) => h.fd,
                    _ => unreachable!("try_send only fails with Closed/WouldBlock"),
                };
                drop(unsafe { TcpStream::from_raw_fd(fd) });
                None
            }
        }
    }

    /// The housekeeper serving loop: accept (blocking, but bounded by the
    /// orchestrator's next tick deadline — sleeps between events, FR-5),
    /// authenticate (FR-9: before routing), hand off, and never touch the fd
    /// again (FR-13). Between accept returns, due background ticks fire
    /// (T4.1, D9: the serving housekeeper *is* the background-work home).
    ///
    /// Shutdown (T4.4): every loop exit re-checks the shutdown flag; a set
    /// flag breaks the loop (a blocked `accept(2)` is woken by `shutdown(2)`
    /// on a dup of the listener fd — the EINVAL lands in the dead-listener
    /// arm), a connection that raced the flag is refused with PG `57P01`
    /// rather than routed, and after the loop the serving thread runs the
    /// whole §10.2 drain ([`crate::shutdown::run_drain`]).
    ///
    /// Runs an unpinned glommio executor on the calling thread for the
    /// sender-side channel connects and the Tier C writer (T5.3: the
    /// serialized mutation path runs here, and its boot publication gates
    /// the accept loop — FR-6); after that, the loop is plain
    /// blocking I/O and the ring idles between events.
    #[allow(clippy::too_many_arguments)]
    pub fn serve(
        listener: AcceptLoop,
        auth: AuthConfig,
        senders: Vec<SharedSender<Handoff>>,
        numas: Vec<u32>,
        counters: Vec<Arc<crate::stats::WorkerCounters>>,
        tierc_plane: TierCPlane,
        initial: ClusterSnapshot,
        max_conns_per_worker: u32,
        tick_init: crate::orchestrator::OrchestratorInit,
        designated: Vec<(u32, SharedSender<crate::handoff::ControlMsg>)>,
        checkpoint: crate::checkpointer::CheckpointStats,
        recycle: Option<crate::recycler::RecycleInit>,
        shutdown: Option<crate::shutdown::ServingShutdown>,
        drain: Vec<(u32, bool, SharedSender<crate::shutdown::DrainMsg>)>,
        reap: Option<crate::reaper::ReapInit>,
    ) {
        let ex = LocalExecutorBuilder::new(ExecutorPlacement::Unbound)
            .name("cascade-hk-ring")
            .make()
            .expect("housekeeper executor");
        ex.run(async move {
            let policy = AuthPolicy::from_config(&auth).expect("auth config validated at boot");
            let mut routes = Vec::with_capacity(senders.len());
            for ((sender, numa), counter) in senders.into_iter().zip(numas).zip(counters) {
                routes.push(Route {
                    sender: sender.connect().await,
                    numa,
                    counter,
                });
            }
            let router = Router {
                policy,
                routes,
                max_conns_per_worker,
                tierc: Arc::clone(&tierc_plane.catalog),
            };

            // The orchestrator is serving-thread-local (its jobs may hold
            // executor-bound state). Assemble it here, before the accept
            // loop, so the first tick can never fire into an unregistered
            // job: the stall watch first (T4.1), then the checkpointer's
            // fanout (T4.2, FR-31) over this executor-connected control
            // senders.
            let wakeups = tick_init.wakeups;
            let mut orchestrator = crate::orchestrator::Orchestrator::from_counters(
                tick_init.interval,
                tick_init.ticks,
                tick_init.max_late_us,
            );
            orchestrator.register(Box::new(crate::orchestrator::StallWatch::new(
                tick_init.stalls,
                tick_init.stall_soft,
                tick_init.stall_hard,
            )));
            let mut sites = Vec::with_capacity(designated.len());
            for (numa, tx) in designated {
                sites.push((numa, tx.connect().await));
            }
            // The recycler (T4.3, FR-32) registers AFTER the fanout (T4.2):
            // it consumes the frontier the checkpointer's passes advance,
            // so on one tick the recycler observes the fresh value. It
            // rides its OWN channel — SPSC means it cannot share the
            // fanout's senders — whose receiving end sits on the WAL-home
            // NUMA's designated reactor; the targeting is the isolation,
            // and the fanout's set stays exactly the T4.2 full one.
            orchestrator.register(Box::new(crate::checkpointer::CheckpointFanout::new(
                sites, checkpoint,
            )));
            if let Some(init) = recycle {
                let sender = init.sender.connect().await;
                orchestrator.register(Box::new(crate::recycler::WalRecycler::new(
                    init.db,
                    sender,
                    init.truncatable,
                    init.stats,
                )));
            }
            // The drain channels (T4.4) are the third signal family —
            // connected here on the serving executor, held as plain locals
            // (they are NOT tick jobs; only the drain uses them, and the
            // drain is one-shot). The middle flag marks the WAL-home
            // reactor — the drain keys step 3's ack on it, never on slot
            // position.
            //
            // Family order here is load-bearing (T4.5): a `connect` blocks
            // until the peer endpoint connects on its own executor, so the
            // serving side must connect the families in the SAME order the
            // workers do (handoff → control → recycle → drain → reap).
            // Connecting the reaper BEFORE the drain deadlocked the boot:
            // the serving thread waited inside reap-sender #0 for a worker
            // that was itself still waiting inside its drain connect — for
            // this thread's drain senders, which came after the reapers.
            let mut drain_senders = Vec::with_capacity(drain.len());
            for (numa, is_home, tx) in drain {
                drain_senders.push((numa, is_home, tx.connect().await));
            }
            // The reaper (T4.5, FR-34) registers last: unlike the
            // recycler it has no producer/consumer dependency on the
            // others (it reads no frontier, writes no LSN), so the tick
            // order carries no correctness here — the CONNECT order above
            // is what must mirror the workers. Its fanout targets EVERY
            // worker: idle connections live on all rings, not just
            // designated ones.
            if let Some(init) = reap {
                let mut senders = Vec::with_capacity(init.senders.len());
                for tx in init.senders {
                    senders.push(tx.connect().await);
                }
                orchestrator.register(Box::new(crate::reaper::IdleReaper::new(
                    senders, init.stats,
                )));
            }
            // Tier C's serialized writer and the boot publication (T5.3,
            // FR-6/FR-29, §10.1 step 7): the writer installs on THIS ring —
            // its write-channel forwarders and publication inits connect
            // here, the families the workers' loops connect LAST (after
            // the reap: this call site is the mirror of that order). The
            // accept loop below stays closed until the initial snapshot
            // has stored on every reactor's catalog — no route serves
            // from an absent catalog, and workers spawned early gate
            // their handoffs on the same publication.
            let published = crate::tierc::start_writer(tierc_plane, initial).await;
            published
                .await
                .expect("the writer publishes the boot snapshot before serving");
            // The shutdown flag read every loop pass. `None` on a
            // listenerless boot (nothing accepts, nothing drains — D9).
            let shutdown_flagged = |flag: Option<&Arc<std::sync::atomic::AtomicBool>>| {
                flag.map(|f| f.load(Ordering::Relaxed)).unwrap_or(false)
            };

            let (stats, listener) = listener.into_parts();
            loop {
                // One wakeup = one accept(2) return, timed out or not. The
                // idle-rate observable for FR-5/T4.1: an idle server wakes
                // ~once per tick — a `loop {}` would show millions.
                wakeups.fetch_add(1, Ordering::Relaxed);
                crate::accept::set_recv_timeout(&listener, orchestrator.until_next())
                    .expect("SO_RCVTIMEO set");
                let mut stream = match listener.accept() {
                    Ok((s, _)) => s,
                    // Timed out or raced the timeout: the tick window is up.
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        orchestrator.poll_due();
                        if shutdown_flagged(shutdown.as_ref().map(|s| &s.flag)) {
                            break;
                        }
                        continue;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                        if shutdown_flagged(shutdown.as_ref().map(|s| &s.flag)) {
                            break;
                        }
                        continue;
                    }
                    // Transient accept failure (the peer aborted its
                    // connect): real PostgreSQL continues, and so do we —
                    // treating it as listener death would skip the drain
                    // and the clean marker entirely (T4.4 review).
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionAborted => {
                        if shutdown_flagged(shutdown.as_ref().map(|s| &s.flag)) {
                            break;
                        }
                        continue;
                    }
                    // Listener dead — genuinely, or woken by the shutdown
                    // `shutdown(2)` (a dup'd fd closed on the accept side
                    // reads as EINVAL here). The flag tells the two apart:
                    // shutdown proceeds, a real listener death ends serve.
                    Err(_) => {
                        if shutdown_flagged(shutdown.as_ref().map(|s| &s.flag)) {
                            break;
                        }
                        return;
                    }
                };
                // §10.2 step 1: stop accepting. A connection that raced the
                // flag between its connect and this check never gets routed
                // into a draining fleet — refused with PG 57P01, then closed.
                if shutdown_flagged(shutdown.as_ref().map(|s| &s.flag)) {
                    let mut refused = stream;
                    let _ = crate::pg::send_fatal(
                        &mut refused,
                        crate::shutdown::ADMIN_SHUTDOWN,
                        "terminating connection: the server is shutting down",
                    );
                    break;
                }
                // Transactional request/response traffic is dominated by
                // small segments: with Nagle on, each response's later
                // segments wait out the peer's delayed ACK (tens of ms on
                // WSL2). Real PostgreSQL sets nodelay on accepted sockets
                // too.
                let _ = stream.set_nodelay(true);
                stats.accepted.fetch_add(1, Ordering::Relaxed);
                if let Ok(session) = handshake(&mut stream, &router.policy) {
                    router.route(stream, &session);
                }
                // else: a rejection was already sent (or the peer vanished);
                // dropping the stream closes it. Ticks wait for the next
                // loop pass — at most one handshake of lateness.
            }

            // The accept loop broke on the flag: §10.2 steps 2–4 run right
            // here, on the serving thread, before the executor drops (step
            // 5 is Runtime::shutdown's joins). The drain senders drop with
            // this async block — that is what ends each worker's
            // `drain_loop`.
            if let Some(ctx) = &shutdown {
                ctx.trace.record("accept-stopped");
                crate::shutdown::run_drain(ctx, &drain_senders);
            }
        });
    }
}
