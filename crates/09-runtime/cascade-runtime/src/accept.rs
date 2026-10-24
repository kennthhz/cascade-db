//! Housekeeper accept loop (spec §5.1; FR-5).
//!
//! One listening socket, owned by one housekeeper thread. The thread sleeps
//! inside `accept(2)` between events — there is no polling and no busy-wait.
//! `SO_REUSEPORT` is deliberately never set: v1 has exactly one accept point,
//! so kernel-side connection distribution would silently defeat routing
//! (§5.1). `SO_REUSEADDR` is set — it only permits rebinding after TIME_WAIT
//! and has no distribution effect.
//!
//! What crosses out of the loop is a bare `RawFd` (Copy) — the same value
//! FR-13 sends on to a worker later; no handle or ring is shared.

use std::{
    io,
    net::{SocketAddr, TcpListener},
    os::unix::io::AsRawFd,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::Sender,
        Arc,
    },
};

/// A connection accepted on the housekeeper, ready for the pre-routing
/// handshake (auth, FR-9) and then routing (T1.5). It stays a `std::net`
/// stream here because the housekeeper is a plain thread (R5); the *executor*
/// handoff later converts it to a bare `RawFd` (FR-13).
pub struct AcceptedConn {
    pub stream: std::net::TcpStream,
    pub peer: SocketAddr,
}

/// Wakeup statistics: one increment per `accept(2)` return. With no clients
/// this stays flat — the observable form of "sleeps between events" (FR-5).
#[derive(Debug, Default)]
pub struct AcceptStats {
    pub accepted: AtomicU64,
}

/// The single listening socket (spec §5.1).
pub struct AcceptLoop {
    listener: TcpListener,
    stats: Arc<AcceptStats>,
}

impl AcceptLoop {
    /// Bind the one listening socket.
    pub fn bind(host: &str, port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind((host, port))?;
        // SO_REUSEADDR only — never SO_REUSEPORT (FR-5).
        unsafe {
            let one: libc::c_int = 1;
            let got = libc::setsockopt(
                listener.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &one as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            if got != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { listener, stats: Arc::new(AcceptStats::default()) })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn stats(&self) -> &Arc<AcceptStats> {
        &self.stats
    }

    /// Take the stats and the underlying std listener out — the router
    /// (T1.5) drives the accept loop itself.
    pub fn into_parts(self) -> (Arc<AcceptStats>, TcpListener) {
        (self.stats, self.listener)
    }

    /// True iff the socket has `SO_REUSEPORT` — exists so the test can prove
    /// the negative (FR-5).
    pub fn reuses_port(&self) -> io::Result<bool> {
        let mut val: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let got = unsafe {
            libc::getsockopt(
                self.listener.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &mut val as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        if got != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(val != 0)
    }

    /// Serve forever on the calling thread: blocking accept, one
    /// [`AcceptedConn`] per client sent to `conns`. Kernel backlog (128)
    /// absorbs bursts; a full backlog stalls handshakes rather than failing
    /// the loop. If the receiver is gone, the connection is closed and the
    /// loop stops — no fd leaks.
    pub fn serve(self, conns: Sender<AcceptedConn>) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    self.stats.accepted.fetch_add(1, Ordering::Relaxed);
                    let peer = stream
                        .peer_addr()
                        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
                    // Router gone: dropping the conn closes the socket — no
                    // descriptor leak.
                    if conns.send(AcceptedConn { stream, peer }).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return, // listener dead; nothing to serve
            }
        }
    }
}
