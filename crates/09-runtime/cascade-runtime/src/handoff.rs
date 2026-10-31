//! What crosses from the housekeeper to a worker (FR-13).
//!
//! Two planes, two channels, never mixed: connection handoffs (`Handoff`)
//! ride the routing channel the least-loaded pick feeds; control signals
//! (`ControlMsg`) ride a dedicated per-worker channel the housekeeper's
//! background jobs feed (T4.2, FR-31 — the formal `submit_to` semantics
//! land with T5.1; this channel is the staged stand-in with the same
//! topology: housekeeper → designated reactor, never reactor-to-reactor).
//! A control signal is data, not a descriptor: nothing here shares a file
//! handle, ring, or connection object.
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

/// Control-plane signal from the housekeeper to one worker (T4.2).
#[derive(Debug)]
pub enum ControlMsg {
    /// Run one in-pool flush pass over this reactor's local dirty state
    /// (FR-31: the housekeeper decides, the reactor touches its own pool).
    FlushPass,
}
