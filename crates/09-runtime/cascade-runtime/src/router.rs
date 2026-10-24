//! Router v0: accept → authenticate → hand off (spec §5.1; FR-9, FR-11, FR-13).
//!
//! The housekeeper thread runs its own *unpinned* executor
//! (`Placement::Unbound`): affinity is already set by `pin_current`, so the
//! thread stays CFS-scheduled (R5) while the executor gives us
//! `SharedSender::connect()` and the shared channel the fd crosses on.
//!
//! Routing v0 is least-loaded by connection count — the count component of
//! FR-11's composite (CPU sampling + NUMA proximity arrive in M2/M5, as does
//! per-DB affinity: FR-10/FR-12).

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
    LocalExecutorBuilder, Placement,
};

use crate::{
    accept::AcceptLoop,
    config::AuthConfig,
    handoff::Handoff,
    pg::{handshake, AuthPolicy, Session},
};

/// One worker's route: a connected sender into that worker's channel plus its
/// live connection counter (shared with the worker).
struct Route {
    sender: ConnectedSender<Handoff>,
    counter: Arc<AtomicU64>,
}

pub struct Router {
    policy: AuthPolicy,
    routes: Vec<Route>,
    max_conns_per_worker: u32,
}

impl Router {
    /// Pick the least-loaded worker with capacity (FR-11 count component,
    /// A7 cap). Ties go to the first route — deterministic.
    fn choose(&self) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for (i, route) in self.routes.iter().enumerate() {
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
    /// the fd crosses, or it is closed here. Returns the peer address on
    /// success, for logging.
    fn route(&self, stream: TcpStream, session: &Session) -> Option<std::net::SocketAddr> {
        let peer = stream
            .peer_addr()
            .unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
        let Some(idx) = self.choose() else {
            return None; // caller drops the stream → closed
        };
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
        counters: Vec<Arc<AtomicU64>>,
        max_conns_per_worker: u32,
    ) {
        let ex = LocalExecutorBuilder::new(Placement::Unbound)
            .name("cascade-hk-ring")
            .make()
            .expect("housekeeper executor");
        ex.run(async move {
            let policy = AuthPolicy::from_config(&auth).expect("auth config validated at boot");
            let mut routes = Vec::with_capacity(senders.len());
            for (sender, counter) in senders.into_iter().zip(counters) {
                routes.push(Route {
                    sender: sender.connect().await,
                    counter,
                });
            }
            let router = Router {
                policy,
                routes,
                max_conns_per_worker,
            };

            let (stats, listener) = listener.into_parts();
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return, // listener dead
                };
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
