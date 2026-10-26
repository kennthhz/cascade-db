//! What crosses from the housekeeper to a worker (FR-13).
//!
//! A bare descriptor plus routing facts — all `Send`, and `RawFd` is `Copy`,
//! so the value fits glommio's cross-executor channel (spec §5.1). No file
//! handle, ring, or connection object is ever shared.

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
