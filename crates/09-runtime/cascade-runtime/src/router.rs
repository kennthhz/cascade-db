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
        atomic::{AtomicU64, Ordering},
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
    tierc::{Placement, TierCSnapshot},
};

/// PG too_many_connections — the FR-12/R10 refusal code.
const TOO_MANY_CONNECTIONS: &str = "53300";

/// One worker's route: a connected sender into that worker's channel, its
/// NUMA node, and its live connection counter (shared with the worker).
struct Route {
    sender: ConnectedSender<Handoff>,
    numa: u32,
    counter: Arc<AtomicU64>,
}

pub struct Router {
    policy: AuthPolicy,
    routes: Vec<Route>,
    max_conns_per_worker: u32,
    tierc: Arc<TierCSnapshot>,
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
            let load = route.counter.load(Ordering::Relaxed);
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
        let placement = self.tierc.resolve(session.startup.database());
        let Some(idx) = self.choose(&placement) else {
            // FR-12 (R10): a single-affinity DB with no available worker on
            // its bound NUMA is refused, naming the NUMA — never silently
            // crossed. A cross DB with no capacity anywhere gets the same
            // code without a NUMA (§5.2 rules only the single case).
            let message = match placement.bound_numa {
                Some(numa) => format!(
                    "single-affinity database {:?} is bound to NUMA {numa}, \
                     which has no available workers",
                    session.startup.database().unwrap_or("dev")
                ),
                None => "all workers are at their connection capacity".to_owned(),
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

    /// The housekeeper serving loop: accept (blocking — sleeps between
    /// events, FR-5), authenticate (FR-9: before routing), hand off, and
    /// never touch the fd again (FR-13).
    ///
    /// Runs an unpinned glommio executor on the calling thread purely for the
    /// sender-side channel connect; after that connect, the loop is plain
    /// blocking I/O and the ring idles.
    pub fn serve(
        listener: AcceptLoop,
        auth: AuthConfig,
        senders: Vec<SharedSender<Handoff>>,
        numas: Vec<u32>,
        counters: Vec<Arc<AtomicU64>>,
        tierc: Arc<TierCSnapshot>,
        max_conns_per_worker: u32,
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
                tierc,
            };

            let (stats, listener) = listener.into_parts();
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return, // listener dead
                };
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
                // dropping the stream closes it.
            }
        });
    }
}
