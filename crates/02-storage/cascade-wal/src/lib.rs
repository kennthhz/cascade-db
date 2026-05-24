//! Per-database Write-Ahead Log.
//!
//! See `README.md` and `AI/CONTEXT.md`. The WAL record format is the single
//! hardest reservation to retrofit — see §6.3.

use cascade_types::{DatabaseId, Lsn};

/// Sequential WAL I/O — the analog of `PageStore` for the log.
///
/// Kept as a separate trait from `PageStore` so the WAL may live on a
/// different storage tier in the future (§6.4).
pub trait WalStore {
    /// Appends a binary WAL record to the end of the log.
    /// Returns the exact byte offset (LSN) where this record was written.
    async fn append_wal(
        &self,
        db: DatabaseId,
        payload: &[u8],
    ) -> Result<Lsn, WalError>;

    /// Flushes the WAL to durable storage up to its current tail.
    /// Called on `COMMIT`.
    async fn flush_wal(&self, db: DatabaseId) -> Result<(), WalError>;

    /// Recycles WAL segments whose contents are no longer needed for crash
    /// recovery (i.e., the corresponding dirty pages are durably flushed).
    async fn truncate_wal(&self, db: DatabaseId, up_to_lsn: Lsn) -> Result<(), WalError>;
}

/// Read-side trait for WAL consumers (recovery, logical CDC).
///
/// Reservation for §6.3: physical recovery is one consumer of this trait;
/// the logical decoder will be another. No code outside this crate may
/// read raw WAL bytes.
pub trait WalReader {
    /// Read the next record at or after `start`, returning the decoded
    /// record and its LSN. Returns `None` at end-of-log.
    async fn read_record(&self, db: DatabaseId, start: Lsn) -> Result<Option<(Lsn, WalRecord)>, WalError>;
}

/// A decoded WAL record. Concrete variants land in Phase 2; the shape here
/// is just enough to anchor the API.
#[derive(Debug)]
pub enum WalRecord {
    BeginTxn { xid: u64 },
    CommitTxn { xid: u64 },
    AbortTxn { xid: u64 },
    // Row-level events carry enough to reconstruct logical CDC (§6.3).
    Insert { xid: u64, /* table, after_image */ },
    Update { xid: u64, /* table, before_image, after_image */ },
    Delete { xid: u64, /* table, before_image */ },
    // DDL is logged so decoders can interpret historical row formats (§6.3).
    Ddl { xid: u64, /* statement */ },
    Checkpoint { /* metadata */ },
}

#[derive(Debug)]
pub enum WalError {
    Io(std::io::Error),
    Corruption(Lsn),
    UnknownRecordVersion(u32),
}
