//! What crosses from the housekeeper to a worker (FR-13).
//!
//! Connection handoffs (`Handoff`) ride the routing channel the
//! least-loaded pick feeds. (The control-signal plane of T4.2/FR-31 — the
//! dedicated per-worker channel the checkpointer's fanout and the
//! recycler's signals rode — retired with storage migration step M8: the
//! buffer pool stopped writing pages back, so there is no flush pass to
//! fan out and no frontier for a recycler to consume; the log store owns
//! retention (§5.6). Background signal families that remain — the drain
//! (T4.4) and the reaper (T4.5) — keep the same topology rule:
//! housekeeper → designated reactor, never reactor-to-reactor, and a
//! signal is data, not a descriptor: nothing here shares a file handle,
//! ring, or connection object.)
//!
//! Channel construction rule (learned the hard way in the T4.2 tests):
//! the two endpoints of a `shared_channel` must `connect()` on
//! *different* executors — `connect` awaits the peer's connect, and a
//! pending connector is re-polled on every reactor loop pass, so a
//! sequential same-executor pair deadlocks *spinning* at 100% CPU
//! forever, never idling. Production satisfies this structurally
//! (router-side senders, worker-side receivers); any future producer of
//! these messages (T4.5's reaper) must keep the split.

use std::net::SocketAddr;

#[derive(Debug)]
pub struct Handoff {
    /// Owning descriptor. The receiving reactor adopts it onto its own ring;
    /// from `into_raw_fd` on the housekeeper to adoption on the worker,
    /// exactly one side owns it at any time.
    pub fd: std::os::unix::io::RawFd,
    pub peer: SocketAddr,
    pub user: String,
    pub database: Option<String>,
}
