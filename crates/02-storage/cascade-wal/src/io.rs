//! Per-reactor WAL I/O: the append handle (write + `fdatasync` on the
//! calling reactor's own ring) and the recovery-side reader.
//!
//! Handles never leave this crate (I2) — callers get the writer's shared
//! atomics for free but every byte that reaches disk passes through a
//! [`WalHandle`] owned by exactly one reactor.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use cascade_types::{DatabaseId, Lsn};
use glommio::io::BufferedFile;

use crate::{format, WalError, WalReader};

/// Watchdog bound for one ring op. WSL2 (5.15) + glommio 0.9 has been
/// observed to never deliver the CQE for a buffered write/fdatasync roughly
/// once per few thousand multi-session ops; glommio has no timeout of its
/// own, so the awaiting session — and with it, at low concurrency, the
/// whole database — hangs forever. Both ops are idempotent (a pwrite at an
/// explicit offset with the same bytes; a repeated `fdatasync`), so a
/// bounded wait plus re-issue turns the hang into a retry; a superseded op
/// that completes late is harmless by the same argument. ~50× the p99 sync
/// on the M1 bench disk; revisit on bare metal.
const OP_WATCHDOG: Duration = Duration::from_millis(250);
/// Give up after this many watchdog firings on one op and surface an error:
/// the session gets an honest 58000 instead of an unbounded stall.
const OP_ATTEMPTS: usize = 3;

/// TEMPORARY M2 diagnostic: one line per watchdog firing. Removed with the
/// M2 bench gate.
fn waldbg(op: &str, off: u64, attempt: usize) {
    eprintln!(
        "WALDBG tid={:?} op={} ev=watchdog off={} attempt={}",
        std::thread::current().id(),
        op,
        off,
        attempt
    );
}

/// Per-reactor append handle for one DB's log file. Not `Send` across
/// reactors by discipline (the file is bound to the opening ring);
/// ownership is one reactor for its lifetime, like `CoreStorage`.
pub struct WalHandle {
    file: BufferedFile,
    /// Byte length the writer believes the file has — diagnostics.
    written_ops: AtomicU64,
    written_bytes: AtomicU64,
}

impl WalHandle {
    /// Opens (creating if needed) `<wal_dir>/db_<db>.log`.
    ///
    /// The handle is length-agnostic — it writes wherever the writer's
    /// watermarks say — but a `WalWriter::new()` starts its LSNs at 0, so
    /// pairing one with a log that already holds records would overwrite
    /// them. Boot recovery resumes with [`crate::WalWriter::new_at`]
    /// seeded at the replayed log end (T3.3); only that constructor may
    /// pair with a non-empty log.
    pub async fn open(wal_dir: &Path, db: DatabaseId) -> Result<Self, WalError> {
        std::fs::create_dir_all(wal_dir).map_err(WalError::Io)?;
        let path: PathBuf = wal_dir.join(format!("db_{}.log", db.0));
        let file = glommio::io::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .buffered_open(path)
            .await
            .map_err(|e| WalError::Io(e.into()))?;
        Ok(WalHandle {
            file,
            written_ops: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
        })
    }

    /// Appends `bytes` at byte offset `offset` (the LSN). Batched: the
    /// flush leader submits one write for the whole group. Watchdog-guarded
    /// (see [`OP_WATCHDOG`]).
    pub(crate) async fn write_batch(&self, offset: u64, bytes: Vec<u8>) -> Result<(), WalError> {
        let len = bytes.len() as u64;
        for attempt in 0..OP_ATTEMPTS {
            if attempt > 0 {
                waldbg("write", offset, attempt);
            }
            match glommio::timer::timeout(OP_WATCHDOG, self.file.write_at(bytes.clone(), offset))
                .await
            {
                Ok(written) => {
                    if written as u64 != len {
                        return Err(WalError::Io(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!("short wal write: {written} of {len} bytes"),
                        )));
                    }
                    self.written_ops.fetch_add(1, Ordering::Relaxed);
                    self.written_bytes.fetch_add(len, Ordering::Relaxed);
                    return Ok(());
                }
                Err(glommio::GlommioError::TimedOut(_)) => continue, // watchdog: re-issue
                Err(e) => return Err(WalError::Io(e.into())),
            }
        }
        Err(WalError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("wal write at {offset} missed its completion {OP_ATTEMPTS} times"),
        )))
    }

    /// Durability barrier (FR-26): `fdatasync` on this reactor's own ring,
    /// through this reactor's own handle. `fdatasync` is per-inode, so it
    /// durably covers records appended by every reactor. Watchdog-guarded
    /// (see [`OP_WATCHDOG`]).
    pub(crate) async fn sync_data(&self) -> Result<(), WalError> {
        for attempt in 0..OP_ATTEMPTS {
            if attempt > 0 {
                waldbg("sync", 0, attempt);
            }
            match glommio::timer::timeout(OP_WATCHDOG, self.file.fdatasync()).await {
                Ok(()) => return Ok(()),
                Err(glommio::GlommioError::TimedOut(_)) => continue, // watchdog: re-issue
                Err(e) => return Err(WalError::Io(e.into())),
            }
        }
        Err(WalError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("wal fdatasync missed its completion {OP_ATTEMPTS} times"),
        )))
    }

    /// `(write operations, bytes written)` — exactly-once assertions in
    /// tests, group-commit efficiency diagnostics later.
    pub fn write_stats(&self) -> (u64, u64) {
        (
            self.written_ops.load(Ordering::Relaxed),
            self.written_bytes.load(Ordering::Relaxed),
        )
    }
}

/// Debug recovery reader over one DB's log file.
pub struct DebugWalReader {
    file: BufferedFile,
    len: u64,
}

impl DebugWalReader {
    /// Opens `<wal_dir>/db_<db>.log` read-only. The file length is
    /// captured at open: records appended afterwards are invisible to this
    /// reader (it reports end-of-log there). M1 recovery opens the reader
    /// only after writers are done.
    pub async fn open(wal_dir: &Path, db: DatabaseId) -> Result<Self, WalError> {
        let path: PathBuf = wal_dir.join(format!("db_{}.log", db.0));
        let file = glommio::io::OpenOptions::new()
            .read(true)
            .buffered_open(path)
            .await
            .map_err(|e| WalError::Io(e.into()))?;
        let len = file.file_size().await.map_err(|e| WalError::Io(e.into()))?;
        Ok(DebugWalReader { file, len })
    }

    /// Byte length of the log.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True when the log holds no records.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl DebugWalReader {
    async fn read(file: &BufferedFile, pos: u64, len: usize) -> Result<Vec<u8>, WalError> {
        let rr = file
            .read_at(pos, len)
            .await
            .map_err(|e| WalError::Io(e.into()))?;
        Ok(rr.to_vec())
    }
}

impl WalReader for DebugWalReader {
    /// Decodes the single record starting at `start` (its LSN). `None` at
    /// end-of-log; [`WalError::TornTail`] when the record's declared length
    /// runs past EOF (a write that never finished — the normal crash
    /// artifact, never fsynced); [`WalError::Corruption`] on a
    /// complete-length record that fails its CRC (bit rot — recovery must
    /// refuse over this, wal invariant 4).
    async fn read_record(&self, start: Lsn) -> Result<Option<crate::WalEntry>, WalError> {
        if start.0 >= self.len {
            return Ok(None);
        }
        // Header first to learn the length, then the whole record.
        let hdr = Self::read(&self.file, start.0, format::HEADER_LEN + format::CRC_LEN).await?;
        if hdr.len() < format::HEADER_LEN + format::CRC_LEN {
            // EOF mid-header: the write never finished.
            return Err(WalError::TornTail(start));
        }
        let payload_len = u32::from_le_bytes(hdr[..4].try_into().expect("4 bytes")) as usize;
        let total = format::HEADER_LEN + payload_len + format::CRC_LEN;
        let bytes = if total == hdr.len() {
            hdr
        } else {
            Self::read(&self.file, start.0, total).await?
        };
        if bytes.len() < total {
            // Declared length runs past EOF: torn by definition.
            return Err(WalError::TornTail(start));
        }
        match format::decode(&bytes) {
            Ok((rec, consumed)) => {
                debug_assert_eq!(consumed, total);
                Ok(Some(crate::WalEntry {
                    lsn: start,
                    record: rec,
                    next: Lsn(start.0 + consumed as u64),
                }))
            }
            Err(format::DecodeError::Corruption) => Err(WalError::Corruption(start)),
            Err(format::DecodeError::UnknownRecordVersion(v)) => {
                Err(WalError::UnknownRecordVersion(v))
            }
        }
    }
}

/// File-level helpers for boot recovery (T3.3): the clean marker and the
/// torn-tail truncation. All synchronous `std::fs` — they run on the boot
/// thread *before* any worker opens a handle on the same file, so no
/// reactor ever sees a moved file length underneath it.

/// Byte length of the DB's log file (`0` when there is none yet).
pub fn log_len(wal_dir: &Path, db: DatabaseId) -> std::io::Result<u64> {
    let path: PathBuf = wal_dir.join(format!("db_{}.log", db.0));
    match std::fs::metadata(&path) {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// Truncates the log to `len` and fsyncs it — the torn-tail recovery
/// action: everything at or past `len` was an unfinished write that no
/// flush ever covered, so dropping it cannot lose a durable byte. Must
/// run before any handle opens the file (see above).
pub fn truncate_log(wal_dir: &Path, db: DatabaseId, len: u64) -> std::io::Result<()> {
    let path: PathBuf = wal_dir.join(format!("db_{}.log", db.0));
    let f = std::fs::OpenOptions::new().write(true).open(&path)?;
    f.set_len(len)?;
    f.sync_all()
}

/// Writes the clean marker: `db_<db>.clean` holding the durable LSN at
/// graceful shutdown. File fsync, then a best-effort directory fsync so
/// the marker's *existence* is durable too. Only a graceful shutdown
/// writes it (M4 owns the full drain; the server's SIGTERM path is the
/// caller today).
pub fn write_clean_marker(wal_dir: &Path, db: DatabaseId, lsn: Lsn) -> std::io::Result<()> {
    std::fs::create_dir_all(wal_dir)?;
    let path: PathBuf = wal_dir.join(format!("db_{}.clean", db.0));
    {
        let mut f = std::fs::File::create(&path)?;
        f.write_all(&lsn.0.to_le_bytes())?;
        f.sync_all()?;
    }
    // The marker must exist after a crash, not just its content: fsync the
    // directory. Missing support degrades to content-only durability.
    if let Ok(dir) = std::fs::File::open(wal_dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// The clean marker's LSN (`None` = no marker: the last shutdown was not
/// graceful, or this log never had one).
pub fn read_clean_marker(wal_dir: &Path, db: DatabaseId) -> std::io::Result<Option<Lsn>> {
    let path: PathBuf = wal_dir.join(format!("db_{}.clean", db.0));
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if bytes.len() != 8 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("clean marker is {} bytes, expected 8", bytes.len()),
        ));
    }
    Ok(Some(Lsn(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))))
}
