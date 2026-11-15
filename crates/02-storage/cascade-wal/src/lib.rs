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
//! On-disk format (hand-rolled v2, invariant 3+4; WP1 F3): every record is
//! `[u32 payload_len][u32 format_version = 2][u8 type][3 reserved]
//! [u64 xid][u64 lsn][u64 epoch][payload][u32 CRC32 of all preceding
//! bytes]` — a 36-byte header. The LSN of a record is its byte offset in
//! the DB's log file *and* the explicit header field, so records are
//! self-describing (§5): the reader cross-checks the two and refuses a
//! transplant. The epoch field reserves §4 scope fencing in the format (0
//! until multi-owner). `LegacyInsert` (type 4) carries the v1 debug row
//! image (I8); the heap redo record (HEAP_INSERT, type 5) lands with its
//! producer (T1.3). The file is page-cache-backed (`BufferedFile` +
//! `fdatasync`) — the spec's `O_DIRECT` mandate is for *data pages*; the
//! WAL is fsync'd sequential I/O, exactly PostgreSQL's model.

pub mod format;
mod io;
mod recycle;
mod writer;

pub use io::{
    log_len, read_clean_marker, truncate_log, write_clean_marker, DebugWalReader, WalHandle,
};
pub use recycle::{recycle_decision, RecycleDecision, RECYCLE_SEGMENT_BYTES};
pub use writer::{Reservation, WalWriter};

use cascade_types::{DatabaseId, Hlc, Lsn};

/// A decoded WAL record — WP1 F3's type table. `BeginTxn`/`CommitTxn`/
/// `AbortTxn`/`LegacyInsert` are the debug slice's records; the reserved
/// families (heartbeat, high-water, bulk load, unlogged) decode today and
/// gain producers when their features land. Type bytes are stable forever:
/// new records extend the `type` byte, never renumber (see [`format`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    BeginTxn {
        xid: u64,
    },
    /// §6: the commit record carries txn_id (header `xid`) + commit_ts
    /// (payload, the packed [`Hlc`]). Commit ordering comes from
    /// commit_ts, never from LSN adjacency.
    CommitTxn {
        xid: u64,
        commit_ts: Hlc,
    },
    AbortTxn {
        xid: u64,
    },
    /// Type 4 (LEGACY_INSERT): the v1 debug row image — retained so
    /// existing logs replay verbatim. New code moves to HEAP_INSERT
    /// (type 5) when its producer lands (T1.3); this variant is on its
    /// way out.
    LegacyInsert {
        xid: u64,
        /// The table's storage segment — where the row image lives.
        table: cascade_types::SegmentId,
        id: i32,
        val: String,
    },
    /// §7 idle-scope advance (reserved): the scope's closed_ts moved to
    /// `closed_ts` with no txn activity. Nothing emits it yet.
    ClosedTsHeartbeat {
        closed_ts: u64,
    },
    /// §6 primary restart (reserved): the scope's HLC had issued up to
    /// `max_issued`. Nothing emits it yet.
    HlcHighWater {
        max_issued: u64,
    },
    /// §17 bulk load (reserved): payload shape lands with the bulk era.
    BulkLoad,
    /// §17 non-logged path (reserved).
    UnloggedMarker,
}

#[derive(Debug)]
pub enum WalError {
    Io(std::io::Error),
    /// A complete-length record failed its CRC — bit rot or a mid-log
    /// stomp. Recovery must refuse to serve over this.
    Corruption(Lsn),
    /// The record's declared length runs past end-of-file (or the header
    /// itself is truncated): a write that never finished — the normal
    /// crash artifact, never fsynced (FR-25's filled-watermark twin on
    /// the read side). Recovery treats this as end-of-log.
    TornTail(Lsn),
    /// A record whose header-declared LSN contradicts the position it was
    /// read from: transplanted or hand-stomped bytes. The record may be
    /// internally consistent (its CRC can still hold) — it is simply not
    /// the record this log position promised. This check exists because
    /// v2 records are self-describing (F3).
    LsnMismatch {
        at: Lsn,
        declared: Lsn,
    },
    /// A complete-length record whose type byte this decoder does not
    /// know — written by a newer producer. "Extend, never renumber" (F3)
    /// means old decoders refuse loudly, named, instead of misreading.
    UnknownRecordType {
        at: Lsn,
        type_byte: u8,
    },
    UnknownRecordVersion(u32),
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Io(e) => write!(f, "wal io error: {e}"),
            WalError::Corruption(lsn) => {
                write!(f, "wal record at {lsn:?} failed its CRC")
            }
            WalError::TornTail(lsn) => {
                write!(f, "wal record at {lsn:?} is torn past end-of-file (incomplete write)")
            }
            WalError::LsnMismatch { at, declared } => {
                write!(
                    f,
                    "wal record at {at:?} declares LSN {declared:?} — transplanted bytes"
                )
            }
            WalError::UnknownRecordType { at, type_byte } => {
                write!(f, "wal record at {at:?} has unknown record type {type_byte}")
            }
            WalError::UnknownRecordVersion(v) => write!(f, "wal record version {v} is unknown"),
        }
    }
}

/// One decoded record and where to continue — the unit recovery walks by.
#[derive(Debug)]
pub struct WalEntry {
    /// The record's LSN (its first byte) — cross-checked against the
    /// record's own declared LSN by the reader.
    pub lsn: Lsn,
    /// The owner epoch the writing scope stamped into the header (§4
    /// fencing; 0 until multi-owner). Recovery refuses records from
    /// superseded epochs.
    pub epoch: u64,
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
