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
//! On-disk format (WP1 F3, format v3 since M1 — owned by
//! `cascade-wal-format`, re-exported here as [`format`]): every record is
//! `[u32 payload_len][u32 format_version][u8 type][u8 flags][2 reserved]
//! [u64 xid][u64 lsn][u64 epoch][payload][u32 CRC32 of all preceding
//! bytes]` — a 36-byte header; v3's flags byte carries the group-end bit
//! (S16, the mini-transaction is the unit of durability) and v2 bytes
//! still decode. The LSN of a record is its byte offset in
//! the DB's log file *and* the explicit header field, so records are
//! self-describing (§5): the reader cross-checks the two and refuses a
//! transplant. The epoch field reserves §4 scope fencing in the format (0
//! until multi-owner). HEAP_INSERT (type 5) is the heap redo record —
//! `[segment u32][cell bytes verbatim as cascade-page lays them down]` —
//! the runtime's row image since T1.3. LEGACY_INSERT (type 4), the v1 debug
//! row image, is decode-retained: logs written before the switch still
//! replay, but nothing emits it anymore. The file is page-cache-backed (`BufferedFile` +
//! `fdatasync`) — the spec's `O_DIRECT` mandate is for *data pages*; the
//! WAL is fsync'd sequential I/O, exactly PostgreSQL's model.

pub use cascade_wal_format as format;
pub use cascade_wal_format::WalRecord;

mod io;
mod recycle;
mod writer;

pub use io::{
    log_len, read_clean_marker, truncate_log, write_clean_marker, DebugWalReader, WalHandle,
};
pub use recycle::{recycle_decision, RecycleDecision, RECYCLE_SEGMENT_BYTES};
pub use writer::{Reservation, WalWriter};

use cascade_types::{DatabaseId, Lsn};

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
    /// M5: the durability sink refused — fenced, failed, gap, framing:
    /// whatever the `LogSink` contract names (storage §4.9). The commit
    /// path treats this as outcome-unknown (WP5 D9).
    Sink(cascade_storage_api::StoreError),
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
            WalError::Sink(e) => write!(f, "wal sink refused: {e}"),
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
