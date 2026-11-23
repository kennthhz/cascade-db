//! Per-reactor WAL I/O: the append handle (write + `fdatasync` on the
//! calling reactor's own ring) and the recovery-side reader.
//!
//! Handles never leave this crate (I2) — callers get the writer's shared
//! atomics for free but every byte that reaches disk passes through a
//! [`WalHandle`] owned by exactly one reactor.

use std::{
    path::{Path, PathBuf},
};

use cascade_types::{DatabaseId, Lsn};
use glommio::io::BufferedFile;

use crate::{format, WalError, WalReader};

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
            Ok(decoded) => {
                debug_assert_eq!(decoded.total, total);
                // v2 records are self-describing (F3): the declared LSN
                // must agree with where this record was found. A mismatch
                // is a transplant — internally consistent bytes at the
                // wrong address — and the reader refuses it rather than
                // serve a record the position never promised.
                if decoded.lsn != start.0 {
                    return Err(WalError::LsnMismatch {
                        at: start,
                        declared: Lsn(decoded.lsn),
                    });
                }
                Ok(Some(crate::WalEntry {
                    lsn: start,
                    epoch: decoded.epoch,
                    record: decoded.record,
                    next: Lsn(start.0 + decoded.total as u64),
                }))
            }
            Err(format::DecodeError::Corruption) => Err(WalError::Corruption(start)),
            Err(format::DecodeError::UnknownRecordVersion(v)) => {
                Err(WalError::UnknownRecordVersion(v))
            }
            Err(format::DecodeError::UnknownRecordType(t)) => {
                Err(WalError::UnknownRecordType {
                    at: start,
                    type_byte: t,
                })
            }
        }
    }
}

// File-level helpers for boot recovery (T3.3): the clean marker and the
// torn-tail truncation. All synchronous `std::fs` — they run on the boot
// thread *before* any worker opens a handle on the same file, so no
// reactor ever sees a moved file length underneath it.
