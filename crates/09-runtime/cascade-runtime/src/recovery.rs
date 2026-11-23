//! Boot recovery (T3.3, FR-7): walk the WAL and decide what a boot resumes
//! from, *before* the socket binds.
//!
//! The debug slice's only durable state home is the WAL — data pages are
//! not checkpointed yet (that arrival is M4) — so replay is not optional
//! even after a *clean* shutdown: committed rows live only in log records.
//! Every boot therefore walks the log and replays committed transactions
//! into the seed rows each worker's table starts with. What the clean
//! marker (`db_<db>.clean`, written on graceful shutdown) decides is the
//! boot's *character*: a marker with the log ending exactly at it is a
//! clean boot; anything else (no marker, or bytes past it) is a crash
//! recovery, reported as such.
//!
//! Error philosophy (wal invariant 4): a torn tail — a record whose bytes
//! run past EOF, never fsynced — is the normal crash artifact. It is
//! truncated away and boot proceeds; no durable byte is lost because no
//! flush ever covered it. A complete-length record that fails its CRC is
//! bit rot or a mid-log stomp *underneath* durable history: [`prepare`]
//! refuses, and the caller must not open a socket. Recovering *into* a
//! lying log would let a later boot serve rows whose history nobody can
//! vouch for.

use std::{collections::HashMap, path::PathBuf, sync::OnceLock, time::Duration};

use cascade_storage_api::log_source::LogSource;
use cascade_wal_format;
use cascade_types::{DatabaseId, Lsn, ScopeId};


/// What boot resumes from. Produced by [`prepare`] before any worker
/// exists; consumed by boot to seed the writer's watermarks and every
/// reactor's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// Byte offset the new writer starts at: the replayed log end (a
    /// record boundary). Boot passes it to `WalWriter::new_at` — a writer
    /// seeded at 0 here would overwrite replayed history with its first
    /// flush.
    pub resume_offset: u64,
    /// Rows from committed transactions, in commit order — each worker's
    /// table is seeded with these before its first session.
    pub seed_rows: Vec<(i32, String)>,
    /// The clean marker's LSN when one exists (a graceful shutdown
    /// preceded this boot).
    pub clean_lsn: Option<Lsn>,
    /// True when the last shutdown was not clean: no marker, or log bytes
    /// past the marker's LSN. Informational in this slice (replay runs on
    /// every boot either way — the WAL is the only durable state home);
    /// telemetry and tests read it.
    pub unclean: bool,
    /// The torn tail found at recovery, if any: the LSN the incomplete
    /// record started at. It was truncated away before this plan was
    /// returned.
    pub torn_tail: Option<Lsn>,
}

impl RecoveryPlan {
    /// The plan for a log that does not exist yet: first boot, nothing to
    /// replay, writer starts at 0.
    pub fn fresh() -> Self {
        RecoveryPlan {
            resume_offset: 0,
            seed_rows: Vec::new(),
            clean_lsn: None,
            unclean: false,
            torn_tail: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// A complete-length record failed its CRC mid-stream. Boot must
    /// refuse: replaying over corrupt history would serve rows whose
    /// durability nobody can vouch for.
    #[error("wal record at {lsn:?} failed its CRC mid-stream — refusing boot over corrupt history")]
    Corrupt { lsn: Lsn },
    /// A record whose bytes are internally consistent but whose placement
    /// contradicts the log: the header declares an LSN other than the
    /// position it was read from — a transplant. The CRC may still hold,
    /// so this is named on its own: "CRC failed" would send the operator
    /// chasing bit rot instead of a hand-edit or copy-paste bug (the
    /// loud-refusal family, WP1 F3 / wal invariant 7).
    #[error("wal record at {at:?} declares LSN {declared:?} — transplanted bytes, refusing boot")]
    LsnMismatch { at: Lsn, declared: Lsn },
    /// The log carries a record type byte this build cannot decode —
    /// written by a newer producer ("extend, never renumber" means old
    /// builds refuse loudly). Same refuse posture as a CRC failure,
    /// named so the operator sees producer/decoder version skew, not
    /// hardware failure.
    #[error(
        "wal record at {lsn:?} has unknown record type {type_byte} — refusing boot (newer producer?)"
    )]
    UnknownRecordType { lsn: Lsn, type_byte: u8 },
    /// The log carries a record version this build cannot decode — newer
    /// or bit-flipped format. Same refuse posture as a CRC failure.
    #[error("wal record version {version} is unknown to this build — refusing boot")]
    UnknownVersion { version: u32 },
    #[error("recovery io error: {0}")]
    Io(#[from] std::io::Error),
}

/// M6 (storage §9): the replay walk over a [`LogSource`] — the log store
/// serves only durable, gap-free records and has already dropped its torn
/// tail at open, so this walk has no truncation, no clean marker and no
/// torn-tail arm: it replays exactly what durability acked and stops at
/// the durable LSN. The replay law is byte-for-byte the legacy walk's
/// (committed transactions in commit order, uncommitted buffers drop, an
/// unparseable cell from a CRC-valid record refuses the boot).
pub async fn prepare_from_source<S: LogSource + ?Sized>(
    source: &S,
    scope: ScopeId,
    durable: Lsn,
) -> Result<RecoveryPlan, RecoveryError> {
    let span = cascade_telemetry::Span::noop();
    let ctx = cascade_storage_api::OpCtx::new(
        cascade_storage_api::TenantContext::for_database(DatabaseId(0)),
        &span,
    );
    let mut open: HashMap<u64, Vec<(i32, String)>> = HashMap::new();
    let mut seed_rows: Vec<(i32, String)> = Vec::new();
    let mut cursor = Lsn(0);
    while cursor.0 < durable.0 {
        // Whole records up to about 1 MiB; the store serves at least one
        // whole record per call and stops at durability (§4.4).
        let chunk = source
            .read_log(&ctx, scope, cursor, 1024 * 1024)
            .await
            .map_err(|e| RecoveryError::Io(std::io::Error::other(e.to_string())))?;
        if chunk.bytes.is_empty() {
            break; // durable reached (or a gap the store refused to paper over)
        }
        let mut at = 0usize;
        while at < chunk.bytes.len() {
            let decoded = cascade_wal_format::decode(&chunk.bytes[at..]).map_err(|e| match e {
                cascade_wal_format::DecodeError::Corruption => {
                    RecoveryError::Corrupt { lsn: Lsn(chunk.start.0 + at as u64) }
                }
                cascade_wal_format::DecodeError::UnknownRecordVersion(v) => {
                    RecoveryError::UnknownVersion { version: v }
                }
                cascade_wal_format::DecodeError::UnknownRecordType(t) => {
                    RecoveryError::UnknownRecordType {
                        lsn: Lsn(chunk.start.0 + at as u64),
                        type_byte: t,
                    }
                }
            })?;
            replay_record(&decoded.record, cursor, &mut open, &mut seed_rows)?;
            cursor = Lsn(cursor.0 + decoded.total as u64);
            at += decoded.total;
        }
    }
    Ok(RecoveryPlan {
        resume_offset: durable.0,
        seed_rows,
        clean_lsn: None, // the marker died with the file path (§9, M6)
        unclean: false,  // the store's own open decided the tail's fate
        torn_tail: None, // dropped by the log store before this walk
    })
}

/// One record's replay effect — the legacy walk's match, kept verbatim so
/// the LogSource walk's law stays the law the legacy tests pinned.
fn replay_record(
    record: &cascade_wal::WalRecord,
    cursor: Lsn,
    open: &mut HashMap<u64, Vec<(i32, String)>>,
    seed_rows: &mut Vec<(i32, String)>,
) -> Result<(), RecoveryError> {
    match record {
        cascade_wal::WalRecord::BeginTxn { xid } => {
            open.entry(*xid).or_default();
        }
        cascade_wal::WalRecord::LegacyInsert { xid, id, val, .. } => {
            open.entry(*xid).or_default().push((*id, val.clone()));
        }
        cascade_wal::WalRecord::HeapInsert { xid, cell, .. } => {
            match crate::session::parse_heap_insert_row(cell) {
                Some(row) => open.entry(*xid).or_default().push(row),
                None => return Err(RecoveryError::Corrupt { lsn: cursor }),
            }
        }
        cascade_wal::WalRecord::CommitTxn { xid, .. } => {
            if let Some(rows) = open.remove(xid) {
                seed_rows.extend(rows);
            }
        }
        cascade_wal::WalRecord::AbortTxn { xid } => {
            open.remove(xid);
        }
        cascade_wal::WalRecord::ClosedTsHeartbeat { .. }
        | cascade_wal::WalRecord::HlcHighWater { .. }
        | cascade_wal::WalRecord::BulkLoad
        | cascade_wal::WalRecord::UnloggedMarker => {}
    }
    Ok(())
}


// ---- Crash barrier (the T3.3 harness's crash points) ----

static BARRIER_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

fn barrier_dir() -> Option<&'static PathBuf> {
    BARRIER_DIR
        .get_or_init(|| std::env::var_os("CASCADE_TEST_BARRIER_DIR").map(PathBuf::from))
        .as_ref()
}

/// Test-only crash point, armed by the crash harness via
/// `CASCADE_TEST_BARRIER_DIR` (unset ⇒ no-op, production cost = one env
/// read per process). A session reaching `phase` writes
/// `<dir>/<phase>` and parks until the harness answers `<dir>/<phase>.go`
/// — or, in the intended use, never proceeds: the harness `kill -9`s the
/// process the moment the marker appears, making the kill land at a
/// deterministic protocol point (mid-reserve, mid-fill, post-sync)
/// instead of wherever the scheduler happened to be.
pub async fn crash_barrier(phase: &str) {
    let Some(dir) = barrier_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(phase), b"1");
    let go = dir.join(format!("{phase}.go"));
    loop {
        if go.exists() {
            return;
        }
        glommio::timer::sleep(Duration::from_millis(1)).await;
    }
}
