//! Storage-contract errors (§4.9). Every variant is named for the *rule*
//! it broke, so callers branch on meaning, not on strings.

use cascade_types::{Epoch, Lsn, ScopeId};

/// An I/O error from a `BlockDevice` (§4.8). Newtype over `std::io::Error`
/// so contract signatures stay nameable; `StoreError::Io` wraps it.
#[derive(Debug)]
pub struct IoError(pub std::io::Error);

impl From<std::io::Error> for IoError {
    fn from(e: std::io::Error) -> Self {
        IoError(e)
    }
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for IoError {}

/// Why an append's framing refused (`StoreError::BadFrame`). The log store
/// checks only the frame and never interprets a payload (§4.3 promise 4)
/// — these name the frame rule that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// A record's CRC did not cover its bytes.
    BadCrc,
    /// A record's header LSN did not equal its position.
    BadLsn,
    /// A record's header epoch did not equal the append's epoch.
    BadEpoch,
    /// The last record of `bytes` did not carry the group-end flag (S16).
    NotGroupEnd,
    /// A record declared a length running past `bytes`.
    BadLength,
    /// A format version or flag combination this store does not read.
    UnknownFrame,
}

/// Where durable data failed an integrity check (`StoreError::Corruption`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptionSite {
    /// A log frame failed its CRC or a record refused its header checks.
    LogFrame {
        scope: ScopeId,
        at: Lsn,
    },
    /// A page on disk failed its integrity check.
    Page {
        scope: ScopeId,
        page: crate::types::PageKey,
    },
    /// A manifest or metadata record failed its checksum.
    Manifest,
}

/// Errors of the encryption-at-rest plumbing (`StoreError::Key`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    /// The provider does not know this key reference.
    UnknownKey,
    /// The provider does not hold this key version (too old, retired).
    UnknownVersion,
    /// The key material is unavailable (operator action needed).
    Unavailable,
}

/// The storage contracts' error type (§4.9). `#[non_exhaustive]`: the
/// remote clients add their own later without breaking matches that cover
/// the documented rules.
#[non_exhaustive]
#[derive(Debug)]
pub enum StoreError {
    /// `append` or `open_writer` with an epoch below the current one
    /// (§4.3 promise 1).
    Fenced { current: Epoch },
    /// An append starting above the scope's appended end — the log never
    /// has holes (§4.3 promise 2).
    Gap { expected: Lsn },
    /// An append overlapping accepted bytes with different content
    /// (§4.3 promise 3).
    Conflict { at: Lsn },
    /// Framing refused (§4.3 promise 4) — a buggy writer caught at the
    /// boundary instead of on recovery.
    BadFrame { at: Lsn, reason: FrameError },
    /// A page request above the durable LSN (§4.5 promise 2) — no
    /// non-durable record is ever applied.
    BeyondDurable { durable: Lsn },
    /// A read below the retained floor (§4.4 promise 4), or a restore
    /// point the log store can no longer answer (§4.4).
    Truncated { floor: Lsn },
    /// A read starting inside a record (§4.4 promise 3).
    NotRecordBoundary { at: Lsn },
    /// The page store holds no version old enough (§4.5 promise 5); it
    /// never returns a version newer than requested.
    TooOld { oldest: Lsn },
    /// The scope was never created, or is dropped/unknown here.
    UnknownScope(ScopeId),
    /// The scope already exists.
    ScopeExists(ScopeId),
    /// The scope's log failed in this epoch; reopen to recover
    /// (§4.3 promise 7, §8.1).
    Failed { scope: ScopeId },
    /// A checksum or decryption failure in durable data.
    Corruption { what: CorruptionSite },
    Io(std::io::Error),
    Key(KeyError),
    /// Remote only: not enough replicas reachable (§4.3.1).
    Unavailable,
    /// The caller's `OpCtx` deadline passed (Rule 8).
    DeadlineExceeded,
    /// A contract hook that exists but has no v1 implementation (§4.6
    /// lineage attach).
    Unsupported(&'static str),
}

impl StoreError {
    /// `Fenced`, `Failed` and `Io` on the commit path are "outcome
    /// unknown" failures: the commit record may have become durable
    /// before the failure was noticed (WP5 D9 poisons the txn).
    pub fn is_outcome_unknown(&self) -> bool {
        matches!(
            self,
            StoreError::Fenced { .. } | StoreError::Failed { .. } | StoreError::Io(_)
        )
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Fenced { current } => write!(f, "fenced by epoch {current:?}"),
            StoreError::Gap { expected } => write!(f, "log gap: expected start {expected:?}"),
            StoreError::Conflict { at } => write!(f, "conflicting bytes at {at:?}"),
            StoreError::BadFrame { at, reason } => write!(f, "bad frame at {at:?}: {reason:?}"),
            StoreError::BeyondDurable { durable } => {
                write!(f, "beyond durable LSN {durable:?}")
            }
            StoreError::Truncated { floor } => write!(f, "truncated at floor {floor:?}"),
            StoreError::NotRecordBoundary { at } => write!(f, "{at:?} is inside a record"),
            StoreError::TooOld { oldest } => write!(f, "request below oldest readable {oldest:?}"),
            StoreError::UnknownScope(s) => write!(f, "unknown scope {s:?}"),
            StoreError::ScopeExists(s) => write!(f, "scope {s:?} already exists"),
            StoreError::Failed { scope } => write!(f, "scope {scope:?} log failed; reopen"),
            StoreError::Corruption { what } => write!(f, "corruption: {what:?}"),
            StoreError::Io(e) => write!(f, "storage io error: {e}"),
            StoreError::Key(e) => write!(f, "key error: {e:?}"),
            StoreError::Unavailable => write!(f, "not enough replicas reachable"),
            StoreError::DeadlineExceeded => write!(f, "deadline exceeded"),
            StoreError::Unsupported(what) => write!(f, "unsupported: {what}"),
        }
    }
}

impl std::error::Error for StoreError {}
