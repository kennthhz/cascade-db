//! The manifest (§5.2): replaced by atomic rename, it carries — per
//! scope — the current epoch, the key reference, the retention holders,
//! the durable LSN as of the manifest, and the compact extent index
//! (runs: consecutive extents live in consecutive frames, so a scope
//! that has written gigabytes still needs only a few hundred entries,
//! §5.3). Per stream it carries the S18 counters and the recovery scan
//! position (§5.7). On clean shutdown it records the exact end of the
//! log — after which any bad frame before that end is corruption, not a
//! torn flush. On every open the store reads the manifest, increments
//! the incarnation, and writes it back before any frame is written.

use cascade_storage_api::RetentionHolder;
use cascade_types::{Epoch, Lsn, ScopeId};

/// What stopped a manifest decode — metadata corruption is never
/// guessed, so every refusal is this one named value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestError;

pub const MANIFEST_MAGIC: u32 = 0x4341_4D4C; // "CAML" — Cascade manifest
pub const MANIFEST_VERSION: u16 = 1;

/// One contiguous located region of a scope's log (§5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentRun {
    pub lsn_start: Lsn,
    pub lsn_end: Lsn,
    pub stream: u32,
    pub segment_seq: u64,
    /// Byte offset of the region's first frame in the segment file.
    pub offset: u64,
}

/// Per-stream recovery position and S18 counters (§5.2, §5.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamPosition {
    pub stream: u32,
    /// The next segment_seq a reuse or extension may take (S18).
    pub next_segment_seq: u64,
    /// Where recovery starts scanning: the first frame it may trust.
    pub scan_segment_seq: u64,
    pub scan_frame_seq: u64,
    pub scan_offset: u64,
}

/// Per-scope manifest record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRecord {
    pub scope: ScopeId,
    pub epoch: Epoch,
    pub key_id: u64,
    /// The durable LSN as of this manifest.
    pub durable: Lsn,
    pub holders: Vec<(RetentionHolder, Lsn)>,
    pub runs: Vec<ExtentRun>,
}

/// The whole manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub incarnation: u64,
    /// The next ScopeId issuance (S18: counters survive restarts).
    pub next_scope_id: u64,
    /// Set by a clean shutdown: after it, any bad frame before the
    /// recorded end is corruption, not a torn flush (§5.7).
    pub clean_shutdown: bool,
    pub streams: Vec<StreamPosition>,
    pub scopes: Vec<ScopeRecord>,
}

/// Every open: read the manifest, increment the incarnation, write it
/// back before any frame is written (§5.7 — this is what keeps nonces
/// unique across a torn frame's rewritten position). The clean flag
/// clears: the store is unclean until proven otherwise.
pub fn bump_incarnation(mut m: Manifest) -> Manifest {
    m.incarnation += 1;
    m.clean_shutdown = false;
    m
}

fn holder_kind(h: &RetentionHolder) -> (u8, u64, u64) {
    match h {
        RetentionHolder::PageStore { instance } => (0, *instance, 0),
        RetentionHolder::Follower { node } => (1, u64::from(node.0), 0),
        RetentionHolder::Cdc { slot } => (2, *slot, 0),
        RetentionHolder::Archive => (3, 0, 0),
    }
}

fn holder_of(kind: u8, a: u64) -> Result<RetentionHolder, ManifestError> {
    match kind {
        0 => Ok(RetentionHolder::PageStore { instance: a }),
        1 => Ok(RetentionHolder::Follower {
            node: cascade_types::NodeId(u32::try_from(a).map_err(|_| ManifestError)?),
        }),
        2 => Ok(RetentionHolder::Cdc { slot: a }),
        3 => Ok(RetentionHolder::Archive),
        _ => Err(ManifestError),
    }
}

fn put_u8(b: &mut Vec<u8>, v: u8) {
    b.push(v);
}
fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, ManifestError> {
        let v = *self.b.get(self.at).ok_or(ManifestError)?;
        self.at += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, ManifestError> {
        if self.b.len() < self.at + 2 {
            return Err(ManifestError);
        }
        let v = u16::from_le_bytes(self.b[self.at..self.at + 2].try_into().expect("2"));
        self.at += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, ManifestError> {
        if self.b.len() < self.at + 4 {
            return Err(ManifestError);
        }
        let v = u32::from_le_bytes(self.b[self.at..self.at + 4].try_into().expect("4"));
        self.at += 4;
        Ok(v)
    }
    fn u64(&mut self) -> Result<u64, ManifestError> {
        if self.b.len() < self.at + 8 {
            return Err(ManifestError);
        }
        let v = u64::from_le_bytes(self.b[self.at..self.at + 8].try_into().expect("8"));
        self.at += 8;
        Ok(v)
    }
}

/// Encodes the manifest with a CRC32 trailer over everything above it.
pub fn encode(m: &Manifest) -> Vec<u8> {
    let mut b = Vec::new();
    put_u32(&mut b, MANIFEST_MAGIC);
    put_u16(&mut b, MANIFEST_VERSION);
    put_u16(&mut b, 0); // flags
    put_u64(&mut b, m.incarnation);
    put_u64(&mut b, m.next_scope_id);
    put_u8(&mut b, if m.clean_shutdown { 1 } else { 0 });
    put_u32(&mut b, m.streams.len() as u32);
    for s in &m.streams {
        put_u32(&mut b, s.stream);
        put_u64(&mut b, s.next_segment_seq);
        put_u64(&mut b, s.scan_segment_seq);
        put_u64(&mut b, s.scan_frame_seq);
        put_u64(&mut b, s.scan_offset);
    }
    put_u32(&mut b, m.scopes.len() as u32);
    for s in &m.scopes {
        put_u64(&mut b, s.scope.0);
        put_u64(&mut b, s.epoch.0);
        put_u64(&mut b, s.key_id);
        put_u64(&mut b, s.durable.0);
        put_u32(&mut b, s.holders.len() as u32);
        for (h, lsn) in &s.holders {
            let (kind, a, c) = holder_kind(h);
            put_u8(&mut b, kind);
            put_u64(&mut b, a);
            put_u64(&mut b, c);
            put_u64(&mut b, lsn.0);
        }
        put_u32(&mut b, s.runs.len() as u32);
        for r in &s.runs {
            put_u64(&mut b, r.lsn_start.0);
            put_u64(&mut b, r.lsn_end.0);
            put_u32(&mut b, r.stream);
            put_u64(&mut b, r.segment_seq);
            put_u64(&mut b, r.offset);
        }
    }
    let crc = crc32(&b);
    put_u32(&mut b, crc);
    b
}

/// Decodes a manifest; any CRC mismatch or short read is a refusal —
/// metadata corruption is never guessed.
pub fn decode(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    if bytes.len() < 4 {
        return Err(ManifestError);
    }
    let body = &bytes[..bytes.len() - 4];
    let stored_crc = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().expect("4"));
    if crc32(body) != stored_crc {
        return Err(ManifestError);
    }
    let mut r = Reader { b: bytes, at: 0 };
    if r.u32()? != MANIFEST_MAGIC {
        return Err(ManifestError);
    }
    if r.u16()? != MANIFEST_VERSION {
        return Err(ManifestError);
    }
    let _flags = r.u16()?;
    let incarnation = r.u64()?;
    let next_scope_id = r.u64()?;
    let clean_shutdown = r.u8()? == 1;
    let streams_n = r.u32()? as usize;
    let mut streams = Vec::with_capacity(streams_n.min(1024));
    for _ in 0..streams_n {
        streams.push(StreamPosition {
            stream: r.u32()?,
            next_segment_seq: r.u64()?,
            scan_segment_seq: r.u64()?,
            scan_frame_seq: r.u64()?,
            scan_offset: r.u64()?,
        });
    }
    let scopes_n = r.u32()? as usize;
    let mut scopes = Vec::with_capacity(scopes_n.min(4096));
    for _ in 0..scopes_n {
        let scope = ScopeId(r.u64()?);
        let epoch = Epoch(r.u64()?);
        let key_id = r.u64()?;
        let durable = Lsn(r.u64()?);
        let holders_n = r.u32()? as usize;
        let mut holders = Vec::with_capacity(holders_n.min(64));
        for _ in 0..holders_n {
            let kind = r.u8()?;
            let a = r.u64()?;
            let _c = r.u64()?;
            let lsn = Lsn(r.u64()?);
            holders.push((holder_of(kind, a)?, lsn));
        }
        let runs_n = r.u32()? as usize;
        let mut runs = Vec::with_capacity(runs_n.min(4096));
        for _ in 0..runs_n {
            runs.push(ExtentRun {
                lsn_start: Lsn(r.u64()?),
                lsn_end: Lsn(r.u64()?),
                stream: r.u32()?,
                segment_seq: r.u64()?,
                offset: r.u64()?,
            });
        }
        scopes.push(ScopeRecord {
            scope,
            epoch,
            key_id,
            durable,
            holders,
            runs,
        });
    }
    Ok(Manifest {
        incarnation,
        next_scope_id,
        clean_shutdown,
        streams,
        scopes,
    })
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
