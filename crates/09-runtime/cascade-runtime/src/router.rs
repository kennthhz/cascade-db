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
//! whole machine, I13), and picks by the §5.2 load composite (FR-11, T5.6
//! — R9's phase two complete): connection count, then the reactor's
//! published CPU EWMA ([`crate::cpuload`]), then NIC proximity as a
//! cross-only tie-breaker — see [`composite_key`]. A `single` DB whose
//! bound NUMA has no available worker is refused with PG `53300` naming
//! the NUMA — never silently crossed (FR-12, R10).

use std::{
    net::TcpStream,
    os::unix::io::{FromRawFd, IntoRawFd},
    sync::{
        atomic::Ordering,
        Arc,
    },
    time::Duration,
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
    /// The NIC's NUMA node (§5.2's proximity tie-breaker, T5.6). Boot
    /// resolves it once — from config or sysfs discovery — and the NIC
    /// does not move; `None` leaves the tie-breaker inert.
    nic_numa: Option<u32>,
}

/// One candidate for the composite pick — the pure decision's input view,
/// extracted from [`Route`] (which holds executor-bound senders) so the
/// full ordering matrix is unit-testable without an executor.
struct Candidate {
    numa: u32,
    load: u64,
    cpu_milli: u64,
}

/// The §5.2 load composite (FR-11, R9 phase two) as ONE ordering key,
/// lexicographic in the spec's listed order: active connections, then
/// recent CPU utilization (the reactor's published EWMA), then NIC
/// proximity — which is a TIE-BREAKER ONLY, and meaningful only for
/// `cross` DBs: a `single` DB's candidate set is already one NUMA, so
/// proximity cannot discriminate within it, and the key holds its
/// component at 0 across that set. `None` NIC (undiscovered) is inert
/// the same way. Each component is consulted only when every earlier
/// one ties; strict `<` keeps first-route-wins on full ties —
/// deterministic, as the count-only router was.
fn composite_key(
    c: &Candidate,
    placement: &Placement,
    nic_numa: Option<u32>,
) -> (u64, u64, u64) {
    let nic = match (placement.affinity, nic_numa) {
        (NumaAffinity::Cross, Some(nic)) if c.numa != nic => 1,
        _ => 0,
    };
    (c.load, c.cpu_milli, nic)
}

/// The pure pick: filter to the placement's candidate set (I13 affinity
/// restriction, A7 availability cap), order by the composite key, return
/// the winner's index. Extracted for the exact-matrix unit tests.
fn pick(
    candidates: &[Candidate],
    placement: &Placement,
    nic_numa: Option<u32>,
    cap: u32,
) -> Option<usize> {
    let mut best: Option<(usize, (u64, u64, u64))> = None;
    for (i, c) in candidates.iter().enumerate() {
        // I13: single-affinity DBs consider only their bound NUMA's
        // workers; cross DBs the whole machine.
        if placement.affinity == NumaAffinity::Single && Some(c.numa) != placement.bound_numa {
            continue;
        }
        if c.load >= cap as u64 {
            continue; // A7: per-worker availability cap
        }
        let key = composite_key(c, placement, nic_numa);
        if best.map_or(true, |(_, b)| key < b) {
            best = Some((i, key));
        }
    }
    best.map(|(i, _)| i)
}

impl Router {
    /// Pick the least-loaded candidate by the §5.2 composite (FR-11):
    /// see [`composite_key`] for the ordering. Returns `None` when the
    /// placement's candidate set has no available worker.
    fn choose(&self, placement: &Placement) -> Option<usize> {
        let candidates: Vec<Candidate> = self
            .routes
            .iter()
            .map(|r| Candidate {
                numa: r.numa,
                load: r.counter.connections.load(Ordering::Relaxed),
                cpu_milli: r.counter.cpu_ewma_milli.load(Ordering::Relaxed),
            })
            .collect();
        pick(
            &candidates,
            placement,
            self.nic_numa,
            self.max_conns_per_worker,
        )
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
            Ok(()) => {
                // The gauge counts ADMITTED connections from the instant
                // of admission, not from when the worker gets around to
                // `session::run` — otherwise a worker descheduled between
                // handoff and count reads as idle and every accept in the
                // window piles onto it (observed: two sequential test
                // clients both routed to one worker under full-suite
                // load). The session end is the only decreter; the
                // boot-window gate refusal (boot.rs `gate_handoff`)
                // decrements its own admission.
                self.routes[idx]
                    .counter
                    .connections
                    .fetch_add(1, Ordering::Relaxed);
                Some(peer)
            }
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
    /// the accept loop — FR-6); after that the loop is plain blocking
    /// I/O that yields once per pass so the ring's OTHER tasks — the
    /// writer and its forwarders, which serve the DDL bounce (T5.4,
    /// FR-29) — keep running between accepts.
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
        nic_numa: Option<u32>,
        handshake_timeout_ms: u64,
        tick_init: crate::orchestrator::OrchestratorInit,
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
                nic_numa,
            };
            // The handshake's time budget (H1, `handshake_timeout_ms`): a
            // silent or trickling client must not park THIS loop — with
            // it gone, every tick, the Tier C writer, and shutdown all
            // went with it. `None` = the config disabled the bound.
            let handshake_budget = if handshake_timeout_ms > 0 {
                Some(Duration::from_millis(handshake_timeout_ms))
            } else {
                None
            };

            // The orchestrator is serving-thread-local (its jobs may hold
            // executor-bound state). Assemble it here, before the accept
            // loop, so the first tick can never fire into an unregistered
            // job: the stall watch (T4.1). (The checkpointer's fanout and
            // the WAL recycler that registered after it retired with
            // storage migration step M8: the buffer pool no longer writes
            // pages back, so there is no flush pass to fan out and no
            // truncatable frontier for a recycler to consume — the log
            // store owns retention, storage design §5.6.)
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
            // The drain channels (T4.4) — connected here on the serving
            // executor, held as plain locals
            // (they are NOT tick jobs; only the drain uses them, and the
            // drain is one-shot). The middle flag marks the WAL-home
            // reactor — the drain keys step 3's ack on it, never on slot
            // position.
            //
            // Family order here is load-bearing (T4.5): a `connect` blocks
            // until the peer endpoint connects on its own executor, so the
            // serving side must connect the families in the SAME order the
            // workers do (handoff → drain → reap; the control and recycle
            // families that used to sit between handoff and drain retired
            // with storage M8).
            // Connecting the reaper BEFORE the drain deadlocked the boot:
            // the serving thread waited inside reap-sender #0 for a worker
            // that was itself still waiting inside its drain connect — for
            // this thread's drain senders, which came after the reapers.
            let mut drain_senders = Vec::with_capacity(drain.len());
            for (numa, is_home, tx) in drain {
                drain_senders.push((numa, is_home, tx.connect().await));
            }
            // The reaper (T4.5, FR-34) runs after the drain: unlike the
            // retired recycler it has no producer/consumer dependency on
            // the others (it reads no frontier, writes no LSN), so the
            // tick order carries no correctness here — the CONNECT order
            // above is what must mirror the workers. Its fanout targets
            // EVERY worker: idle connections live on all rings, not just
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
                // The ring is not ours alone: the Tier C writer (FR-29) and
                // its write-channel forwarders live HERE, and the accept
                // pass itself is blocking I/O that never awaits. One
                // delegation per pass is what lets a DDL change cross the
                // funnel and its ack come back; without it the writer sat
                // unpolled forever behind the accept loop (the T5.4 bug:
                // a session awaiting the ack wedged until the stall rule
                // killed it). When no sibling has work, a no-op.
                glommio::yield_if_needed().await;
                // One wakeup = one accept(2) return, timed out or not. The
                // idle-rate observable for FR-5/T4.1: an idle server wakes
                // ~once per tick — a `loop {}` would show millions.
                wakeups.fetch_add(1, Ordering::Relaxed);
                // Ticks poll at the TOP of every pass, not inside the
                // accept-timeout arm below (the M1 review find): with a
                // backlog that never drains — sustained connection churn —
                // no pass ever times out, and a poll parked in that arm
                // suspends every tick job precisely while the server is
                // busiest. Polling first also keeps `until_next` honest
                // for the pass's park: due jobs fire before their deadline
                // is read, so the park never waits on a passed deadline.
                orchestrator.poll_due();
                // The serving quantum: every cross-ring stage of the DDL
                // bounce (the write forwarder, the writer's budgeted
                // fan-out wait, the ack) resumes only when an accept
                // return hands the ring back to its other tasks, so a
                // tick-paced park would pace the whole bounce at the tick
                // interval — measured at ~6 s per CREATE ROLE under the
                // 1 s default tick, which quietly broke R11's bounded
                // ack. The quantum bounds it at a few tens of ms instead;
                // ticks still fire exactly, via the poll at the top of
                // every pass — only the idle WAKEUP RATE rises to the
                // quantum
                // when `tick_ms` exceeds it (fixtures ticking at or below
                // 50 ms — where the FR-5 wakeups ≈ ticks assertions live —
                // are unaffected: their `until_next` is already the
                // quantum). The endgame is an async accept whose awaits
                // let task wakers through directly; until then, this.
                const SERVING_QUANTUM: Duration = Duration::from_millis(50);
                let park = orchestrator.until_next().min(SERVING_QUANTUM);
                crate::accept::set_recv_timeout(&listener, park)
                    .expect("SO_RCVTIMEO set");
                let mut stream = match listener.accept() {
                    Ok((s, _)) => s,
                    // Timed out or raced the timeout: the pass's tick poll
                    // already ran at the top.
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
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
                if let Ok(session) = handshake(&mut stream, &router.policy, handshake_budget) {
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
                crate::shutdown::run_drain(ctx, &drain_senders).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full §5.2 ordering matrix, EXACT — synthetic candidates, no
    /// executor. The tie-review rule (exactly-tied, asserted, not
    /// approximately) lives here: the NIC-decisive cases construct
    /// literal equality in the leading components.
    fn cand(numa: u32, load: u64, cpu_milli: u64) -> Candidate {
        Candidate { numa, load, cpu_milli }
    }

    fn place(affinity: NumaAffinity, bound: Option<u32>) -> Placement {
        Placement { affinity, bound_numa: bound }
    }

    #[test]
    fn count_is_primary_cpu_secondary() {
        let cs = [cand(0, 0, 900), cand(1, 1, 0)];
        // Count 0 wins over count 1 even with nine times the CPU —
        // utilization never overturns the primary key.
        assert_eq!(pick(&cs, &place(NumaAffinity::Cross, None), None, 200), Some(0));
        // At an exact count tie the EWMA decides.
        let tied = [cand(0, 2, 500), cand(1, 2, 100)];
        assert_eq!(pick(&tied, &place(NumaAffinity::Cross, None), None, 200), Some(1));
    }

    #[test]
    fn nic_decides_only_on_exact_ties_and_only_cross() {
        let cs = [cand(0, 1, 0), cand(1, 1, 0)]; // exact (count, cpu) tie
        let cross = place(NumaAffinity::Cross, None);
        // NIC on numa 1: the nic-local candidate wins the exact tie...
        assert_eq!(pick(&cs, &cross, Some(1), 200), Some(1));
        // ...the same tie with NO nic discovery stays first-wins.
        assert_eq!(pick(&cs, &cross, None, 200), Some(0));
        // The same exact tie under `single` binding to numa 0: proximity
        // is NOT consulted — the candidate set is already one NUMA, so
        // the tie is full and first-wins keeps the bound-NUMA candidate.
        assert_eq!(
            pick(&cs, &place(NumaAffinity::Single, Some(0)), Some(1), 200),
            Some(0)
        );
        // NIC never overturns a count: the distant candidate's count is
        // LOWER, so it wins despite the nic-local one being occupied.
        let counts = [cand(0, 0, 0), cand(1, 1, 0)];
        assert_eq!(pick(&counts, &cross, Some(1), 200), Some(0));
    }

    #[test]
    fn cpu_and_nic_only_matter_in_listed_order() {
        // cpu can overturn NIC-influence within a count tie: the distant
        // but LESS-USED-cpu candidate beats the nic-local hot one.
        let cs = [cand(0, 1, 10), cand(1, 1, 900)];
        assert_eq!(pick(&cs, &place(NumaAffinity::Cross, None), Some(1), 200), Some(0));
        // ...and NIC resolves only after cpu ties: equal counts, unequal
        // cpu already decided above; here cpu ties and NIC flips it.
        let tied = [cand(0, 1, 10), cand(1, 1, 10)];
        assert_eq!(pick(&tied, &place(NumaAffinity::Cross, None), Some(1), 200), Some(1));
    }

    #[test]
    fn full_ties_stay_first_wins_and_filters_hold() {
        // Full tie: first route (deterministic, as the count-only router).
        let cs = [cand(0, 1, 5), cand(1, 1, 5)];
        assert_eq!(pick(&cs, &place(NumaAffinity::Cross, None), Some(0), 200), Some(0));
        // A7 cap: at-cap candidates are unavailable even if perfect.
        assert_eq!(pick(&cs, &place(NumaAffinity::Cross, None), None, 1), None);
        let capped = [cand(0, 1, 0), cand(1, 0, 0)];
        assert_eq!(pick(&capped, &place(NumaAffinity::Cross, None), None, 1), Some(1));
        // I13: single binding filters the candidate set, whatever the
        // other candidates' keys look like.
        let scattered = [cand(0, 0, 0), cand(1, 1, 0), cand(2, 2, 0)];
        assert_eq!(
            pick(&scattered, &place(NumaAffinity::Single, Some(2)), None, 200),
            Some(2),
            "only the bound NUMA is a candidate, even at higher load"
        );
        // Single bound to a NUMA whose candidates are all at cap: refused.
        let drained = [cand(7, 1, 0), cand(3, 3, 0)];
        assert_eq!(
            pick(&drained, &place(NumaAffinity::Single, Some(7)), None, 1),
            None,
            "FR-12's precondition: no available worker on the bound NUMA"
        );
    }
}
