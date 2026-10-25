//! Per-database Write-Ahead Log.
//!
//! See `README.md` and `AI/CONTEXT.md`. The WAL record format is the single
//! hardest reservation to retrofit — see §6.3.
//!
//! ## M1 debug slice (T1.10): the durability core, single-DB
//!
//! [`WalWriter`] is the `Arc`-shared, per-DB writer of §8.2 — shared state
//! only (atomics + a staging map), no file handle. The hot path is
//! [`append_wal`](WalWriter::append_wal) → reserve (an LSN is handed out
//! *before* the record exists) → [`Reservation::fill`] (bytes become
//! visible; the *filled* watermark advances over the contiguous prefix of
//! completely written records, never past an unfilled reservation — FR-25).
//! [`flush_wal`](WalWriter::flush_wal) elects one leader by CAS; the leader
//! writes `[durable..min(filled, up_to)]` through the *caller's*
//! [`WalHandle`] — a per-reactor handle on the calling reactor's own ring
//! (I2: handles never leave this crate) — `fdatasync`s, publishes the
//! durable LSN, and wakes waiters; losers wait on the durable LSN (FR-26).
//!
//! A dropped (never-filled) reservation is a *permanently torn* log
//! position: the filled watermark stops before it and nothing at or beyond
//! it can ever become durable. That is the honest crash semantic — a panic
//! between reserve and fill means the record never landed — and the tests
//! plant it deliberately. Recovery/abort handling arrives with T1.11.
//!
//! On-disk format (hand-rolled v1, invariant 3+4): every record is
//! `[u32 payload_len][u32 version = 1][u8 type][3 reserved][u64 xid]
//! [payload][u32 CRC32 of all preceding bytes]`; the LSN of a record is its
//! byte offset in the DB's log file. `Insert` carries the full row image
//! (I8). The file is page-cache-backed (`BufferedFile` + `fdatasync`) —
//! the spec's `O_DIRECT` mandate is for *data pages*; the WAL is fsync'd
//! sequential I/O, exactly PostgreSQL's model.

mod format;
mod io;
mod writer;

pub use io::{DebugWalReader, WalHandle};
pub use writer::{Reservation, WalWriter};

use cascade_types::{DatabaseId, Lsn};

/// A decoded WAL record. The debug slice uses `BeginTxn`/`CommitTxn`/
/// `AbortTxn`/`Insert` (full row image, I8); new types extend the `type`
/// byte without touching the layout (see [`format`]-reserved version field
/// for format changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    BeginTxn {
        xid: u64,
    },
    CommitTxn {
        xid: u64,
    },
    AbortTxn {
        xid: u64,
    },
    Insert {
        xid: u64,
        /// The table's storage segment — where the row image lives.
        table: cascade_types::SegmentId,
        id: i32,
        val: String,
    },
}

#[derive(Debug)]
pub enum WalError {
    Io(std::io::Error),
    Corruption(Lsn),
    UnknownRecordVersion(u32),
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Io(e) => write!(f, "wal io error: {e}"),
            WalError::Corruption(lsn) => write!(f, "wal record at {lsn:?} failed its CRC"),
            WalError::UnknownRecordVersion(v) => write!(f, "wal record version {v} is unknown"),
        }
    }
}

/// One decoded record and where to continue — the unit recovery walks by.
#[derive(Debug)]
pub struct WalEntry {
    /// The record's LSN (its first byte).
    pub lsn: Lsn,
    pub record: WalRecord,
    /// One past the record's last byte: pass this as the next
    /// [`WalReader::read_record`] `start` to step through the log.
    pub next: Lsn,
}

/// Read-side trait for WAL consumers (recovery, logical CDC).
///
/// Reservation for §6.3: physical recovery is one consumer of this trait;
/// the logical decoder will be another. No code outside this crate may read
/// raw WAL bytes. The reader is per-DB (invariant 2: one WAL per
/// `DatabaseId`), so the draft's `db` parameter lives on the constructor.
#[allow(async_fn_in_trait)] // readers are reactor-bound, like stores
pub trait WalReader {
    /// Read the record starting at exactly `start`. Returns `None` at
    /// end-of-log; [`WalError::Corruption`] on a torn or bit-rotted record.
    async fn read_record(&self, start: Lsn) -> Result<Option<WalEntry>, WalError>;
}

// `DatabaseId` names the log file; kept in scope for the public constructors.
#[allow(unused_imports)]
use DatabaseId as _DbIdAnchor;
