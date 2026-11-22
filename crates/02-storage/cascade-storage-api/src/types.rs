//! Shared contract types (storage-layer-design.md §4.2): what crosses each
//! boundary. Identifiers live in `cascade-types`; everything a caller
//! hands to or receives from a storage contract lives here.

use cascade_types::{Hlc, Lsn, PageNo, SegmentId};
use cascade_wal_format::Frame;

/// A page within a scope: a scope owns its pages, so the database ID in
/// today's `PageId` is replaced by the scope. `SegmentId` is never reused
/// (S18), so a truncated table's old page can never come back where the
/// new table expects zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub segment: SegmentId,
    pub page_no: PageNo,
}

/// Whole log records, in LSN order, with no gaps (§4.4 promises 1–2).
#[derive(Debug)]
pub struct LogChunk {
    /// LSN of the first record.
    pub start: Lsn,
    /// One or more whole records in the `cascade-wal-format` frame.
    pub bytes: Vec<u8>,
    /// `start + bytes.len()`; the next chunk starts here.
    pub end: Lsn,
    /// The scope's durable LSN when the chunk was read.
    pub durable: Lsn,
}

impl LogChunk {
    /// The chunk's records, walked without decoding any payload (S5).
    /// Every record of a served chunk passes frame validation already —
    /// this is the caller's handle onto them, not a second validation.
    pub fn frames(&self) -> Frames<'_> {
        Frames { chunk: self, at: 0 }
    }
}

/// Stepping iterator over a chunk's record frames.
#[derive(Debug)]
pub struct Frames<'a> {
    chunk: &'a LogChunk,
    at: usize,
}

impl<'a> Iterator for Frames<'a> {
    type Item = Frame<'a>;
    fn next(&mut self) -> Option<Frame<'a>> {
        if self.at >= self.chunk.bytes.len() {
            return None;
        }
        match cascade_wal_format::walk(&self.chunk.bytes[self.at..]) {
            Ok(f) => {
                self.at += f.total;
                Some(f)
            }
            // A served chunk is whole validated records by contract
            // (§4.4 promise 2); a failure here is a bug, and the iterator
            // ends rather than lying.
            Err(_) => None,
        }
    }
}

/// A page image as of an LSN (§4.5): decrypted, CRC already checked — the
/// caller never sees storage's integrity plumbing.
#[derive(Debug)]
pub struct PageImage {
    /// `PAGE_SIZE` bytes, zero-filled for a page no record has touched.
    pub buf: cascade_types::AlignedBuf,
    /// END LSN of the last record applied (S17); 0 if never written.
    pub page_lsn: Lsn,
}

/// Per-append metadata the log store needs but payloads never reveal
/// (§4.3.1, §4.4).
#[derive(Debug, Clone, Copy)]
pub struct AppendMeta {
    /// Highest commit timestamp in these bytes, if any — the log store
    /// indexes it per append so point-in-time restore can find the
    /// boundary without parsing payloads (§4.4).
    pub max_commit_ts: Option<Hlc>,
    /// The writer's current durable LSN (S20). Replicas serve reads only
    /// below it; on a single server it is the local durable LSN.
    pub writer_durable: Lsn,
}

/// Lineage for a branched scope (§4.6): a page the branch has not written
/// is read from the parent as of the fork LSN. `attach` with a lineage
/// returns `Unsupported` in v1 — the hook exists so branch creation later
/// needs no contract change.
#[derive(Debug, Clone, Copy)]
pub struct Lineage {
    pub parent: ScopeId,
    pub fork_lsn: Lsn,
}

use cascade_types::ScopeId;

/// A registered holder of log retention (§4.4): the log store discards log
/// only below the minimum over all registered holders, and with no holder
/// registered it discards nothing (a misconfigured server keeps too much
/// log rather than losing log a page store still needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetentionHolder {
    /// The page store instance of this node — its materialized LSN is its
    /// retention value (§6.3).
    PageStore { instance: u64 },
    /// A follower kept warm for promotion (S20).
    Follower { node: cascade_types::NodeId },
    /// A change-data-capture tap (§4.2).
    Cdc { slot: u64 },
    /// The archive (§1.2).
    Archive,
}

/// A reader of page versions (§4.5): the page store may discard page
/// versions below the minimum over all readers' floors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReaderId(pub u64);

/// A flat name inside a `BlockDevice`'s namespace (§4.8): no caller
/// computes a path — file paths never leave the storage layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileName(pub String);

impl std::fmt::Display for FileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An open file on one core's `BlockHandle`. Handles never cross cores
/// (§4.8); the type carries no identity a caller can act on beyond
/// passing it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub u64);
