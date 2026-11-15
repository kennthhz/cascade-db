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

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::OnceLock,
    time::Duration,
};

use cascade_types::{DatabaseId, Lsn};
use cascade_wal::{WalError, WalReader};

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

/// Walks the DB's log and produces the resume plan. Synchronous (it runs
/// on the boot thread through a throwaway executor) and single-shot: by
/// the time any worker opens a handle, the log is final — torn tail
/// already truncated, no concurrent appends.
///
/// The walk: `BeginTxn` opens a buffer, `Insert` appends its row image,
/// `CommitTxn` applies the buffer in commit order, `AbortTxn` discards it.
/// An uncommitted buffer at end-of-log (crash mid-transaction) is dropped
/// — exactly the FR-27 contract: nothing unacked survives. A torn tail
/// ends the walk and is truncated; corruption refuses the boot.
pub fn prepare(wal_dir: &std::path::Path, db: DatabaseId) -> Result<RecoveryPlan, RecoveryError> {
    // No log file: first boot (or a wiped data dir). Nothing to walk — and
    // no shutdown ever happened, so this is neither clean nor unclean.
    if cascade_wal::log_len(wal_dir, db)? == 0 {
        let clean_lsn = cascade_wal::read_clean_marker(wal_dir, db)?;
        return Ok(RecoveryPlan {
            clean_lsn,
            ..RecoveryPlan::fresh()
        });
    }

    let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("cascade-recovery")
        .make()
        .map_err(|e| RecoveryError::Io(std::io::Error::other(e.to_string())))?;
    let plan = ex.run(async {
        let reader = cascade_wal::DebugWalReader::open(wal_dir, db)
            .await
            .map_err(|e| wal_err(e, Lsn(0)))?;

        let mut open: HashMap<u64, Vec<(i32, String)>> = HashMap::new();
        let mut seed_rows: Vec<(i32, String)> = Vec::new();
        let mut cursor = Lsn(0);
        let mut torn_tail = None;
        loop {
            match reader.read_record(cursor).await {
                Ok(Some(entry)) => {
                    match entry.record {
                        cascade_wal::WalRecord::BeginTxn { xid } => {
                            open.entry(xid).or_default();
                        }
                        cascade_wal::WalRecord::LegacyInsert { xid, id, val, .. } => {
                            open.entry(xid).or_default().push((id, val));
                        }
                        // Commit order is replay order: the row vector is
                        // exactly what the acking boot served from its
                        // table.
                        cascade_wal::WalRecord::CommitTxn { xid, .. } => {
                            if let Some(rows) = open.remove(&xid) {
                                seed_rows.extend(rows);
                            }
                        }
                        cascade_wal::WalRecord::AbortTxn { xid } => {
                            open.remove(&xid);
                        }
                        // Reserved families (§6/§7/§17): no replay effect
                        // in the debug slice — they carry no row data.
                        cascade_wal::WalRecord::ClosedTsHeartbeat { .. }
                        | cascade_wal::WalRecord::HlcHighWater { .. }
                        | cascade_wal::WalRecord::BulkLoad
                        | cascade_wal::WalRecord::UnloggedMarker => {}
                    }
                    cursor = entry.next;
                }
                // End-of-log: the walk boundary is the resume point.
                Ok(None) => break,
                // The normal crash artifact: stop here; the caller
                // truncates the tail below.
                Err(WalError::TornTail(lsn)) => {
                    torn_tail = Some(lsn);
                    break;
                }
                Err(e) => return Err(wal_err(e, cursor)),
            }
        }

        if let Some(lsn) = torn_tail {
            cascade_wal::truncate_log(wal_dir, db, lsn.0)?;
        }

        let clean_lsn = cascade_wal::read_clean_marker(wal_dir, db)?;
        // Clean means: a marker exists AND the log ends exactly at it. Any
        // byte past the marker was written after the graceful shutdown's
        // last sync — a crash, not a clean stop.
        let unclean = !matches!(clean_lsn, Some(l) if l.0 == cursor.0);
        Ok(RecoveryPlan {
            resume_offset: cursor.0,
            seed_rows,
            clean_lsn,
            unclean,
            torn_tail,
        })
    })?;
    Ok(plan)
}

/// Maps a reader error to the recovery error it refuses boot with. The
/// `at` LSN only surfaces for errors that do not carry one of their own.
fn wal_err(e: WalError, at: Lsn) -> RecoveryError {
    match e {
        WalError::Corruption(lsn) => RecoveryError::Corrupt { lsn },
        // Named refusals stay named through the boot boundary — a
        // transplant and an unknown producer's record are integrity
        // failures recovery must refuse over, but their causes are not
        // CRC failures and must not read as one.
        WalError::LsnMismatch { at, declared } => RecoveryError::LsnMismatch { at, declared },
        WalError::UnknownRecordType { at, type_byte } => RecoveryError::UnknownRecordType {
            lsn: at,
            type_byte,
        },
        WalError::UnknownRecordVersion(v) => RecoveryError::UnknownVersion { version: v },
        WalError::Io(io) => RecoveryError::Io(io),
        // The walk handles torn tails itself; this arm is unreachable but
        // must exist — report it at the cursor, refusing beats guessing.
        WalError::TornTail(_) => RecoveryError::Corrupt { lsn: at },
    }
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
