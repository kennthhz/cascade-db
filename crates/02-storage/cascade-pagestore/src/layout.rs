//! On-disk byte layouts (§6.3). Every integer is little-endian, every
//! structure ends in a CRC32 over everything before it, and every decoder
//! refuses rather than guesses. Three structures, one namespace:
//!
//! | File | Holds | Layout |
//! | :--- | :--- | :--- |
//! | `pg-MANIFEST` | the restart point: per scope its key reference, its materialized LSN and the segments it has pages in | one 4 KiB block, replaced by atomic rename |
//! | `pg-dblwrite` | the doublewrite area: the one batch in flight | a header region of whole 4 KiB blocks, then the page images |
//! | `pg-<scope>-<segment>` | page images, verbatim | page `n` at `n × 8 KiB` |
//! | `pgm-<scope>-<segment>` | one 64-byte record per page: page LSN, image CRC, room for §7 | `n × 64` |
//!
//! **Why the image is stored verbatim and its checksum is in a sidecar.**
//! The old `CoreStorage` kept a CRC in the first four bytes of the page
//! slot — which, in `cascade-page`'s layout, are the format version and
//! page kind, so a page read back through it looked never-written. The
//! page store writes the image exactly as redo produced it: the §10
//! differential law (a served page equals the replayed page, byte for
//! byte) holds on the disk bytes themselves, and the record beside it
//! carries what the image cannot — the CRC over the *whole* image, the
//! page LSN in a form-independent place, and the key version, nonce and
//! authentication tag that encryption at rest will need (§11 item 3)
//! without `cascade-page` giving up a byte.
//!
//! These layouts are a one-way door: they are pinned byte for byte by the
//! golden tests in this module.

use cascade_storage_api::FileName;
use cascade_types::{Lsn, PageNo, ScopeId, SegmentId};

/// 4 KiB — the device's alignment unit, and the granularity at which a
/// crash keeps or drops unsynced writes (§4.8).
pub(crate) const BLOCK: usize = 4096;

// ---------------------------------------------------------------------------
// Names. A `BlockDevice` namespace is flat (a name joins straight onto the
// root), so the scope prefix is part of the name rather than a directory.
// ---------------------------------------------------------------------------

pub(crate) const MANIFEST_NAME: &str = "pg-MANIFEST";
pub(crate) const MANIFEST_TMP_NAME: &str = "pg-MANIFEST.tmp";
pub(crate) const DBLWRITE_NAME: &str = "pg-dblwrite";
const PAGE_FILE_PREFIX: &str = "pg-";
const META_FILE_PREFIX: &str = "pgm-";

pub(crate) fn page_file(scope: ScopeId, segment: SegmentId) -> FileName {
    FileName(format!("{PAGE_FILE_PREFIX}{}-{}", scope.0, segment.0))
}

pub(crate) fn meta_file(scope: ScopeId, segment: SegmentId) -> FileName {
    FileName(format!("{META_FILE_PREFIX}{}-{}", scope.0, segment.0))
}

/// `pg-<scope>-<segment>` — a page-image file. The three fixed names
/// (`pg-MANIFEST`, its `.tmp`, `pg-dblwrite`) parse to `None`, as does
/// anything that is not this store's.
pub(crate) fn parse_page_file(name: &str) -> Option<(ScopeId, SegmentId)> {
    parse_prefixed(name, PAGE_FILE_PREFIX)
}

/// `pgm-<scope>-<segment>` — the page records beside a page-image file.
pub(crate) fn parse_meta_file(name: &str) -> Option<(ScopeId, SegmentId)> {
    parse_prefixed(name, META_FILE_PREFIX)
}

fn parse_prefixed(name: &str, prefix: &str) -> Option<(ScopeId, SegmentId)> {
    let (scope, segment) = name.strip_prefix(prefix)?.split_once('-')?;
    Some((ScopeId(scope.parse().ok()?), SegmentId(segment.parse().ok()?)))
}

// ---------------------------------------------------------------------------
// Shared reading helpers
// ---------------------------------------------------------------------------

/// Why a layout refused. `Corrupt` means the bytes failed a check this
/// version understands; `UnknownVersion` means a newer build wrote them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutError {
    Corrupt,
    UnknownVersion(u16),
    /// The manifest does not fit its one block (too many scopes or
    /// segments) — a capacity limit, not damage.
    TooLarge,
}

fn bytes(buf: &[u8], at: usize, n: usize) -> Result<&[u8], LayoutError> {
    buf.get(at..at + n).ok_or(LayoutError::Corrupt)
}

fn u16_at(buf: &[u8], at: usize) -> Result<u16, LayoutError> {
    Ok(u16::from_le_bytes(
        bytes(buf, at, 2)?.try_into().expect("2 bytes"),
    ))
}

fn u32_at(buf: &[u8], at: usize) -> Result<u32, LayoutError> {
    Ok(u32::from_le_bytes(
        bytes(buf, at, 4)?.try_into().expect("4 bytes"),
    ))
}

fn u64_at(buf: &[u8], at: usize) -> Result<u64, LayoutError> {
    Ok(u64::from_le_bytes(
        bytes(buf, at, 8)?.try_into().expect("8 bytes"),
    ))
}

fn crc(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

// ---------------------------------------------------------------------------
// The manifest: one 4 KiB block, replaced by atomic rename
//
//   0  magic u32 | 4 version u16 | 6 reserved u16
//   8  instance u64            the holder id reported to the log store
//   16 scope_count u32 | 20 reserved u32
//   24 scopes, scope_count times:
//        scope u64 | key_id u64 | materialized u64 | segment_count u32 | reserved u32
//        segments, segment_count times: segment u32
//   crc32 u32 over everything above | zero padding to the block's end
// ---------------------------------------------------------------------------

pub(crate) const MANIFEST_MAGIC: u32 = 0x5053_4D46;
pub(crate) const MANIFEST_VERSION: u16 = 1;
const MANIFEST_HEADER: usize = 24;
const MANIFEST_SCOPE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestScope {
    pub scope: ScopeId,
    pub key_id: u64,
    /// On-disk pages are exactly the replay of the log below this LSN.
    pub materialized: Lsn,
    /// Every segment that has page files, in ascending order. The list is
    /// what lets `open` refuse when a file has gone missing: the log
    /// below `materialized` may be gone too, so such a page cannot be
    /// rebuilt, and serving zeros for it would be silent data loss.
    pub segments: Vec<SegmentId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub instance: u64,
    pub scopes: Vec<ManifestScope>,
}

pub(crate) fn encode_manifest(m: &Manifest) -> Result<[u8; BLOCK], LayoutError> {
    let mut b = [0u8; BLOCK];
    b[0..4].copy_from_slice(&MANIFEST_MAGIC.to_le_bytes());
    b[4..6].copy_from_slice(&MANIFEST_VERSION.to_le_bytes());
    b[8..16].copy_from_slice(&m.instance.to_le_bytes());
    b[16..20].copy_from_slice(&(m.scopes.len() as u32).to_le_bytes());
    let mut at = MANIFEST_HEADER;
    for s in &m.scopes {
        // The scope's fixed part, every segment, and the trailing CRC must
        // all still fit the block.
        if at + MANIFEST_SCOPE + 4 * s.segments.len() + 4 > BLOCK {
            return Err(LayoutError::TooLarge);
        }
        b[at..at + 8].copy_from_slice(&s.scope.0.to_le_bytes());
        b[at + 8..at + 16].copy_from_slice(&s.key_id.to_le_bytes());
        b[at + 16..at + 24].copy_from_slice(&s.materialized.0.to_le_bytes());
        b[at + 24..at + 28].copy_from_slice(&(s.segments.len() as u32).to_le_bytes());
        at += MANIFEST_SCOPE;
        for seg in &s.segments {
            b[at..at + 4].copy_from_slice(&seg.0.to_le_bytes());
            at += 4;
        }
    }
    let sum = crc(&b[..at]);
    b[at..at + 4].copy_from_slice(&sum.to_le_bytes());
    Ok(b)
}

pub(crate) fn decode_manifest(buf: &[u8]) -> Result<Manifest, LayoutError> {
    if buf.len() < BLOCK {
        return Err(LayoutError::Corrupt);
    }
    if bytes(buf, 0, 4)? != MANIFEST_MAGIC.to_le_bytes() {
        return Err(LayoutError::Corrupt);
    }
    let version = u16_at(buf, 4)?;
    if version != MANIFEST_VERSION {
        return Err(LayoutError::UnknownVersion(version));
    }
    let instance = u64_at(buf, 8)?;
    let count = u32_at(buf, 16)? as usize;
    let mut at = MANIFEST_HEADER;
    let mut scopes = Vec::new();
    // The encoder writes scopes, and each scope's segments, in ascending
    // order. A repeat is damage: seeding state from such a manifest would
    // silently replace the first entry with the second.
    let mut last_scope = None;
    for _ in 0..count {
        let scope = ScopeId(u64_at(buf, at)?);
        if last_scope.is_some_and(|seen| seen >= scope) {
            return Err(LayoutError::Corrupt);
        }
        last_scope = Some(scope);
        let key_id = u64_at(buf, at + 8)?;
        let materialized = Lsn(u64_at(buf, at + 16)?);
        let seg_count = u32_at(buf, at + 24)? as usize;
        at += MANIFEST_SCOPE;
        let mut segments = Vec::new();
        let mut last_segment = None;
        for _ in 0..seg_count {
            let segment = SegmentId(u32_at(buf, at)?);
            if last_segment.is_some_and(|seen| seen >= segment) {
                return Err(LayoutError::Corrupt);
            }
            last_segment = Some(segment);
            segments.push(segment);
            at += 4;
        }
        scopes.push(ManifestScope {
            scope,
            key_id,
            materialized,
            segments,
        });
    }
    let stored = u32_at(buf, at)?;
    at += 4;
    if at > BLOCK || crc(&buf[..at - 4]) != stored {
        return Err(LayoutError::Corrupt);
    }
    if !buf[at..BLOCK].iter().all(|&b| b == 0) {
        // Bytes after the checksum are not covered by it; a writer that
        // put something there is not a writer this decoder knows.
        return Err(LayoutError::Corrupt);
    }
    Ok(Manifest { instance, scopes })
}

// ---------------------------------------------------------------------------
// A page record: 64 bytes in the sidecar, one per page
//
//    0 magic u16 | 2 version u16 | 4 flags u16 | 6 reserved u16
//    8 page_lsn u64
//   16 image_crc u32          CRC32 of the whole 8 KiB image
//   20 key_version u32        §7: the key version the image was sealed with
//   24 nonce [u8; 12]         §7
//   36 tag    [u8; 16]        §7
//   52 reserved [u8; 8]
//   60 record_crc u32         over bytes 0..60 — the fields above are
//                             protected too, not just the image
//
// All zero means the page was never written. Two CRCs, because they answer
// two different questions: `record_crc` says "this record is what was
// written", `image_crc` says "the image beside it is the one it describes".
// ---------------------------------------------------------------------------

pub(crate) const PAGE_REC_LEN: usize = 64;
pub(crate) const PAGE_RECS_PER_BLOCK: usize = BLOCK / PAGE_REC_LEN;
const REC_MAGIC: u16 = 0x5047;
const REC_VERSION: u16 = 1;
const REC_CRC_AT: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageRecord {
    pub page_lsn: Lsn,
    pub image_crc: u32,
    pub key_version: u32,
    pub nonce: [u8; 12],
    pub tag: [u8; 16],
}

impl PageRecord {
    /// A plaintext record for an image (§7's plumbing lands later).
    pub(crate) fn seal(image: &[u8], page_lsn: Lsn) -> Self {
        PageRecord {
            page_lsn,
            image_crc: crc(image),
            key_version: 0,
            nonce: [0; 12],
            tag: [0; 16],
        }
    }
}

pub(crate) fn encode_page_record(r: &PageRecord) -> [u8; PAGE_REC_LEN] {
    let mut b = [0u8; PAGE_REC_LEN];
    b[0..2].copy_from_slice(&REC_MAGIC.to_le_bytes());
    b[2..4].copy_from_slice(&REC_VERSION.to_le_bytes());
    b[8..16].copy_from_slice(&r.page_lsn.0.to_le_bytes());
    b[16..20].copy_from_slice(&r.image_crc.to_le_bytes());
    b[20..24].copy_from_slice(&r.key_version.to_le_bytes());
    b[24..36].copy_from_slice(&r.nonce);
    b[36..52].copy_from_slice(&r.tag);
    let sum = crc(&b[..REC_CRC_AT]);
    b[REC_CRC_AT..].copy_from_slice(&sum.to_le_bytes());
    b
}

/// `Ok(None)` for an all-zero record: the page was never written, which is
/// a normal outcome (§4.5 promise 4), not damage.
pub(crate) fn decode_page_record(buf: &[u8]) -> Result<Option<PageRecord>, LayoutError> {
    let buf = match bytes(buf, 0, PAGE_REC_LEN) {
        Ok(b) => b,
        // A short buffer is a caller bug (the device only hands out whole
        // blocks); treat it as damage rather than panicking on a read path.
        Err(_) => return Err(LayoutError::Corrupt),
    };
    if buf.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    if buf[..2] != REC_MAGIC.to_le_bytes() {
        return Err(LayoutError::Corrupt);
    }
    let version = u16_at(buf, 2)?;
    if version != REC_VERSION {
        return Err(LayoutError::UnknownVersion(version));
    }
    if crc(&buf[..REC_CRC_AT]) != u32_at(buf, REC_CRC_AT)? {
        return Err(LayoutError::Corrupt);
    }
    Ok(Some(PageRecord {
        page_lsn: Lsn(u64_at(buf, 8)?),
        image_crc: u32_at(buf, 16)?,
        key_version: u32_at(buf, 20)?,
        nonce: buf[24..36].try_into().expect("12 bytes"),
        tag: buf[36..52].try_into().expect("16 bytes"),
    }))
}

// ---------------------------------------------------------------------------
// The doublewrite batch header: whole 4 KiB blocks at the start of
// `pg-dblwrite`, then the batch's page images
//
//    0 magic u32 | 4 version u16 | 6 flags u16
//    8 scope u64 | 16 seq u64 | 24 ingested_lsn u64 | 32 count u32 | 36 header_blocks u32
//   40 entries, count times: segment u32 | page_no u32 | page_lsn u64 | image_crc u32 | reserved u32
//    ... zero padding ...
//    (header_len - 4) crc32 u32 over everything above
//
// `ingested_lsn` is the batch's whole point: once the batch's pages are in
// their home files, the pages on disk are *exactly* the replay of the log
// below it. Recovery finishes an interrupted batch and records that LSN;
// ingest then resumes there, and a segment's fill cursor — read from the
// materialized open page — is exactly where that record sequence left it.
//
// `header_blocks` says how long the header region is, and it sits in the
// first block so that [`dw_header_blocks`] can read it from that block
// alone. Without it, recovery would have to take the region's length from
// the *current* configuration — and a configuration changed between a
// crash and the restart would put the checksum somewhere else, read a
// batch in flight as never synced, and leave its half-written home pages
// ahead of the manifest.
// ---------------------------------------------------------------------------

pub(crate) const DW_MAGIC: u32 = 0x4442_5752;
pub(crate) const DW_VERSION: u16 = 1;
pub(crate) const DW_HEADER_BYTES: usize = 40;
pub(crate) const DW_ENTRY_BYTES: usize = 24;
/// Where in the header `header_blocks` sits.
pub(crate) const DW_HEADER_BLOCKS_AT: usize = 36;
/// The most 4 KiB blocks a header region may span — a limit this build
/// will read whatever a configuration says.
pub(crate) const DW_MAX_HEADER_BLOCKS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DwEntry {
    pub segment: SegmentId,
    pub page_no: PageNo,
    pub page_lsn: Lsn,
    pub image_crc: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DwBatch {
    pub scope: ScopeId,
    pub seq: u64,
    pub ingested: Lsn,
    pub entries: Vec<DwEntry>,
}

/// How many pages a header region of `header_len` bytes can describe.
pub(crate) fn dw_capacity(header_len: usize) -> usize {
    debug_assert!(header_len >= BLOCK && header_len % BLOCK == 0);
    (header_len - 4 - DW_HEADER_BYTES) / DW_ENTRY_BYTES
}

pub(crate) fn encode_dw_header(b: &DwBatch, header_len: usize) -> Vec<u8> {
    assert!(
        b.entries.len() <= dw_capacity(header_len),
        "a batch of {} pages does not fit a {}-byte header (capacity {})",
        b.entries.len(),
        header_len,
        dw_capacity(header_len)
    );
    let mut out = vec![0u8; header_len];
    out[0..4].copy_from_slice(&DW_MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&DW_VERSION.to_le_bytes());
    out[8..16].copy_from_slice(&b.scope.0.to_le_bytes());
    out[16..24].copy_from_slice(&b.seq.to_le_bytes());
    out[24..32].copy_from_slice(&b.ingested.0.to_le_bytes());
    out[32..36].copy_from_slice(&(b.entries.len() as u32).to_le_bytes());
    out[DW_HEADER_BLOCKS_AT..DW_HEADER_BLOCKS_AT + 4]
        .copy_from_slice(&((header_len / BLOCK) as u32).to_le_bytes());
    let mut at = DW_HEADER_BYTES;
    for e in &b.entries {
        out[at..at + 4].copy_from_slice(&e.segment.0.to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&e.page_no.to_le_bytes());
        out[at + 8..at + 16].copy_from_slice(&e.page_lsn.0.to_le_bytes());
        out[at + 16..at + 20].copy_from_slice(&e.image_crc.to_le_bytes());
        at += DW_ENTRY_BYTES;
    }
    let sum = crc(&out[..header_len - 4]);
    out[header_len - 4..].copy_from_slice(&sum.to_le_bytes());
    out
}

/// From the first block of the doublewrite area alone: how many 4 KiB
/// blocks the batch header occupying it spans. Recovery reads the region
/// in this length, not in the one the current configuration asks for.
pub(crate) fn dw_header_blocks(first_block: &[u8]) -> Result<usize, LayoutError> {
    if bytes(first_block, 0, 4)? != DW_MAGIC.to_le_bytes() {
        return Err(LayoutError::Corrupt);
    }
    let version = u16_at(first_block, 4)?;
    if version != DW_VERSION {
        return Err(LayoutError::UnknownVersion(version));
    }
    let blocks = u32_at(first_block, DW_HEADER_BLOCKS_AT)? as usize;
    if blocks == 0 || blocks > DW_MAX_HEADER_BLOCKS {
        return Err(LayoutError::Corrupt);
    }
    Ok(blocks)
}

/// Decodes a header region of exactly the length it says it is.
pub(crate) fn decode_dw_header(buf: &[u8]) -> Result<DwBatch, LayoutError> {
    let len = buf.len();
    if len < BLOCK || len % BLOCK != 0 {
        return Err(LayoutError::Corrupt);
    }
    let blocks = dw_header_blocks(&buf[..BLOCK])?;
    if blocks * BLOCK != len {
        // Read in a length other than its own: the checksum is not where
        // this buffer thinks it is.
        return Err(LayoutError::Corrupt);
    }
    if crc(&buf[..len - 4]) != u32_at(buf, len - 4)? {
        return Err(LayoutError::Corrupt);
    }
    let count = u32_at(buf, 32)? as usize;
    if count > dw_capacity(len) {
        return Err(LayoutError::Corrupt);
    }
    let mut entries = Vec::with_capacity(count);
    let mut at = DW_HEADER_BYTES;
    for _ in 0..count {
        entries.push(DwEntry {
            segment: SegmentId(u32_at(buf, at)?),
            page_no: u32_at(buf, at + 4)?,
            page_lsn: Lsn(u64_at(buf, at + 8)?),
            image_crc: u32_at(buf, at + 16)?,
        });
        at += DW_ENTRY_BYTES;
    }
    // Everything between the last entry and the checksum is reserved, and
    // a reserved byte that is not zero is not a writer this decoder knows.
    if !buf[at..len - 4].iter().all(|&b| b == 0) {
        return Err(LayoutError::Corrupt);
    }
    Ok(DwBatch {
        scope: ScopeId(u64_at(buf, 8)?),
        seq: u64_at(buf, 16)?,
        ingested: Lsn(u64_at(buf, 24)?),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
    }

    /// The CRC is the IEEE polynomial crc32fast implements — pinned against
    /// the standard test vector so a dependency bump cannot change it
    /// silently (every structure's integrity rests on it).
    #[test]
    fn crc_is_the_ieee_802_3_polynomial() {
        assert_eq!(crc(b"123456789"), 0xCBF4_3926);
    }

    // ---- names ------------------------------------------------------------

    #[test]
    fn data_file_names_roundtrip_and_fixed_names_do_not_parse() {
        let (scope, segment) = (ScopeId(7), SegmentId(12));
        assert_eq!(page_file(scope, segment).0, "pg-7-12");
        assert_eq!(meta_file(scope, segment).0, "pgm-7-12");
        assert_eq!(parse_page_file("pg-7-12"), Some((scope, segment)));
        assert_eq!(parse_meta_file("pgm-7-12"), Some((scope, segment)));
        // Each parser accepts only its own form.
        assert_eq!(parse_page_file("pgm-7-12"), None);
        assert_eq!(parse_meta_file("pg-7-12"), None);
        // The fixed names, the log store's files, and garbage.
        for name in [
            MANIFEST_NAME,
            MANIFEST_TMP_NAME,
            DBLWRITE_NAME,
            "MANIFEST",
            "seg-0-3",
            "pg-",
            "pg-7-",
            "pg--7-12",
            "pg-7-12-tmp",
        ] {
            assert_eq!(parse_page_file(name), None, "{name}");
            assert_eq!(parse_meta_file(name), None, "{name}");
        }
    }

    // ---- manifest ---------------------------------------------------------

    fn sample() -> Manifest {
        Manifest {
            instance: 1,
            scopes: vec![ManifestScope {
                scope: ScopeId(7),
                key_id: 9,
                materialized: Lsn(0x1000),
                segments: vec![SegmentId(3)],
            }],
        }
    }

    /// One scope, one segment — the whole 60-byte payload, byte for byte:
    /// a 24-byte header, a 32-byte scope entry, and one 4-byte segment.
    #[test]
    fn manifest_golden_literal() {
        let b = encode_manifest(&sample()).expect("fits");
        assert_eq!(
            hex(&b[..60]),
            "46 4d 53 50 01 00 00 00 \
             01 00 00 00 00 00 00 00 \
             01 00 00 00 00 00 00 00 \
             07 00 00 00 00 00 00 00 \
             09 00 00 00 00 00 00 00 \
             00 10 00 00 00 00 00 00 \
             01 00 00 00 00 00 00 00 \
             03 00 00 00"
        );
        assert_eq!(
            u32_at(&b, 60),
            Ok(crc(&b[..60])),
            "the CRC covers the payload"
        );
        assert!(
            b[64..].iter().all(|&x| x == 0),
            "zero padding to the block's end"
        );
    }

    #[test]
    fn manifest_roundtrips_many_scopes_and_segments() {
        let m = Manifest {
            instance: u64::MAX,
            scopes: (0..20u64)
                .map(|i| ManifestScope {
                    scope: ScopeId(i * 3),
                    key_id: i,
                    materialized: Lsn(1 << i),
                    segments: (0..7u32).map(|s| SegmentId(i as u32 * 100 + s)).collect(),
                })
                .collect(),
        };
        assert_eq!(decode_manifest(&encode_manifest(&m).unwrap()), Ok(m));
    }

    #[test]
    fn manifest_refuses_damage() {
        let good = encode_manifest(&sample()).unwrap();
        // A flipped byte anywhere in the payload.
        for at in [0usize, 9, 17, 25, 33, 39] {
            let mut bad = good;
            bad[at] ^= 0xFF;
            assert_eq!(decode_manifest(&bad), Err(LayoutError::Corrupt), "byte {at}");
        }
        // A flipped version byte is named for what it is — a newer build's
        // manifest — not called damage.
        let mut bad = good;
        bad[5] ^= 0xFF;
        assert_eq!(decode_manifest(&bad), Err(LayoutError::UnknownVersion(0xFF01)));
        // A flipped checksum.
        let mut bad = good;
        bad[36] ^= 1;
        assert_eq!(decode_manifest(&bad), Err(LayoutError::Corrupt));
        // Bytes behind the checksum: not covered, so not tolerated.
        let mut bad = good;
        bad[4000] = 7;
        assert_eq!(decode_manifest(&bad), Err(LayoutError::Corrupt));
        // A declared scope count that runs past the block.
        let mut bad = good;
        bad[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_manifest(&bad), Err(LayoutError::Corrupt));
        // An all-zero block (never written).
        assert_eq!(decode_manifest(&[0u8; BLOCK]), Err(LayoutError::Corrupt));
        // A short buffer.
        assert_eq!(decode_manifest(&good[..100]), Err(LayoutError::Corrupt));
    }

    /// The encoder writes scopes and segments in ascending order, and the
    /// decoder holds the manifest to it: a repeated scope would silently
    /// replace its twin when state is seeded from the manifest.
    #[test]
    fn manifest_refuses_repeated_or_unordered_scopes_and_segments() {
        let scope = |id: u64, segments: Vec<u32>| ManifestScope {
            scope: ScopeId(id),
            key_id: 0,
            materialized: Lsn(0),
            segments: segments.into_iter().map(SegmentId).collect(),
        };
        let decode = |scopes: Vec<ManifestScope>| {
            decode_manifest(&encode_manifest(&Manifest { instance: 0, scopes }).unwrap())
        };
        assert!(matches!(
            decode(vec![scope(1, vec![]), scope(1, vec![])]),
            Err(LayoutError::Corrupt)
        ));
        assert!(matches!(
            decode(vec![scope(2, vec![]), scope(1, vec![])]),
            Err(LayoutError::Corrupt)
        ));
        assert!(matches!(
            decode(vec![scope(1, vec![3, 3])]),
            Err(LayoutError::Corrupt)
        ));
        assert!(matches!(
            decode(vec![scope(1, vec![5, 3])]),
            Err(LayoutError::Corrupt)
        ));
        // The ascending form is what the store writes, and it reads back.
        assert!(decode(vec![scope(1, vec![3, 5]), scope(2, vec![])]).is_ok());
    }

    #[test]
    fn manifest_names_a_newer_version_rather_than_calling_it_damage() {
        let mut b = encode_manifest(&sample()).unwrap();
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        let sum = crc(&b[..36]);
        b[36..40].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_manifest(&b), Err(LayoutError::UnknownVersion(2)));
    }

    /// The manifest is one block; past that it says so instead of
    /// truncating. The room follows from the layout: the 24-byte header,
    /// one 32-byte scope entry, 4 bytes per segment, and the checksum.
    #[test]
    fn manifest_reports_a_capacity_overflow() {
        let room = (BLOCK - MANIFEST_HEADER - MANIFEST_SCOPE - 4) / 4;
        assert_eq!(room, 1009, "a 4 KiB block holds 1009 segments of one scope");
        let with = |segments: usize| Manifest {
            instance: 0,
            scopes: vec![ManifestScope {
                scope: ScopeId(1),
                key_id: 0,
                materialized: Lsn(0),
                segments: (0..segments as u32).map(SegmentId).collect(),
            }],
        };
        assert!(encode_manifest(&with(room)).is_ok(), "{room} segments fit");
        assert!(matches!(
            encode_manifest(&with(room + 1)),
            Err(LayoutError::TooLarge)
        ));
    }

    // ---- page record ------------------------------------------------------

    /// The record's fields sit exactly where the module doc says.
    #[test]
    fn page_record_golden_literal() {
        let r = PageRecord {
            page_lsn: Lsn(0x1000),
            image_crc: 0xDEAD_BEEF,
            key_version: 0,
            nonce: [0; 12],
            tag: [0; 16],
        };
        let b = encode_page_record(&r);
        assert_eq!(b.len(), PAGE_REC_LEN);
        assert_eq!(
            hex(&b[..24]),
            "47 50 01 00 00 00 00 00 \
             00 10 00 00 00 00 00 00 \
             ef be ad de \
             00 00 00 00"
        );
        assert!(b[24..52].iter().all(|&x| x == 0), "nonce, tag: zero");
        assert!(b[52..60].iter().all(|&x| x == 0), "reserved: zero");
        assert_eq!(u32_at(&b, 60), Ok(crc(&b[..60])));
    }

    #[test]
    fn page_record_roundtrips_every_field() {
        let r = PageRecord {
            page_lsn: Lsn(u64::MAX),
            image_crc: 0x0123_4567,
            key_version: 9,
            nonce: core::array::from_fn(|i| i as u8),
            tag: core::array::from_fn(|i| 0xF0 | i as u8),
        };
        assert_eq!(decode_page_record(&encode_page_record(&r)), Ok(Some(r)));
    }

    #[test]
    fn page_record_all_zero_is_never_written() {
        assert_eq!(decode_page_record(&[0u8; PAGE_REC_LEN]), Ok(None));
    }

    /// Both CRCs are load-bearing: flipping the page LSN (a field the image
    /// CRC does not cover) must be caught by the record CRC, and flipping
    /// the image CRC itself must be caught too.
    #[test]
    fn page_record_refuses_a_tampered_field_and_a_tampered_checksum() {
        let good = encode_page_record(&PageRecord::seal(b"image", Lsn(5)));
        for at in [8usize, 12, 16, 20, 30, 44, 58] {
            let mut bad = good;
            bad[at] ^= 0x01;
            assert_eq!(decode_page_record(&bad), Err(LayoutError::Corrupt), "byte {at}");
        }
        let mut bad = good;
        bad[60] ^= 0x80;
        assert_eq!(decode_page_record(&bad), Err(LayoutError::Corrupt));
        let mut bad = good;
        bad[0] = 0; // magic damaged
        assert_eq!(decode_page_record(&bad), Err(LayoutError::Corrupt));
        assert!(decode_page_record(&good[..60]).is_err(), "a short buffer is not a record");
    }

    #[test]
    fn page_record_names_a_newer_version() {
        let mut b = encode_page_record(&PageRecord::seal(b"", Lsn(0)));
        b[2..4].copy_from_slice(&7u16.to_le_bytes());
        let sum = crc(&b[..60]);
        b[60..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_page_record(&b), Err(LayoutError::UnknownVersion(7)));
    }

    // ---- doublewrite batch header ----------------------------------------

    fn batch() -> DwBatch {
        DwBatch {
            scope: ScopeId(7),
            seq: 1,
            ingested: Lsn(0x1000),
            entries: vec![DwEntry {
                segment: SegmentId(3),
                page_no: 5,
                page_lsn: Lsn(0x100),
                image_crc: 0x1122_3344,
            }],
        }
    }

    /// The 64 bytes that carry one batch of one page, byte for byte; the
    /// checksum lands in the last four bytes of the region.
    #[test]
    fn doublewrite_header_golden_literal() {
        let b = encode_dw_header(&batch(), BLOCK);
        assert_eq!(b.len(), BLOCK);
        assert_eq!(
            hex(&b[..64]),
            "52 57 42 44 01 00 00 00 \
             07 00 00 00 00 00 00 00 \
             01 00 00 00 00 00 00 00 \
             00 10 00 00 00 00 00 00 \
             01 00 00 00 01 00 00 00 \
             03 00 00 00 05 00 00 00 \
             00 01 00 00 00 00 00 00 \
             44 33 22 11 00 00 00 00"
        );
        assert!(b[64..BLOCK - 4].iter().all(|&x| x == 0), "zero padding");
        assert_eq!(u32_at(&b, BLOCK - 4), Ok(crc(&b[..BLOCK - 4])));
    }

    #[test]
    fn doublewrite_header_says_how_long_it_is() {
        // Readable from the first block alone, at every legal size.
        for blocks in [1usize, 2, 8, DW_MAX_HEADER_BLOCKS] {
            let bytes = encode_dw_header(&batch(), blocks * BLOCK);
            assert_eq!(dw_header_blocks(&bytes[..BLOCK]), Ok(blocks));
        }
        // A region read in a length other than its own is refused — the
        // checksum is not where the reader thinks it is. This is the case
        // a configuration changed across a crash would hit, if recovery
        // took the length from the configuration.
        let eight = encode_dw_header(&batch(), 8 * BLOCK);
        assert_eq!(decode_dw_header(&eight[..BLOCK]), Err(LayoutError::Corrupt));
        assert_eq!(
            decode_dw_header(&eight[..2 * BLOCK]),
            Err(LayoutError::Corrupt)
        );
        // A count this build will not read, or none at all.
        for bogus in [0u32, (DW_MAX_HEADER_BLOCKS as u32) + 1, u32::MAX] {
            let mut bad = encode_dw_header(&batch(), BLOCK);
            bad[DW_HEADER_BLOCKS_AT..DW_HEADER_BLOCKS_AT + 4]
                .copy_from_slice(&bogus.to_le_bytes());
            assert_eq!(dw_header_blocks(&bad[..BLOCK]), Err(LayoutError::Corrupt));
        }
        // A cleared region, and a version this build does not read.
        assert_eq!(dw_header_blocks(&[0u8; BLOCK]), Err(LayoutError::Corrupt));
        let mut newer = encode_dw_header(&batch(), BLOCK);
        newer[4..6].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(dw_header_blocks(&newer[..BLOCK]), Err(LayoutError::UnknownVersion(2)));
    }

    #[test]
    fn doublewrite_header_roundtrips_up_to_capacity() {
        for header_len in [BLOCK, 8 * BLOCK, 64 * BLOCK] {
            let cap = dw_capacity(header_len);
            let full = DwBatch {
                entries: (0..cap)
                    .map(|i| DwEntry {
                        segment: SegmentId(i as u32 % 7),
                        page_no: i as u32,
                        page_lsn: Lsn(i as u64 * 40),
                        image_crc: i as u32,
                    })
                    .collect(),
                ..batch()
            };
            let bytes = encode_dw_header(&full, header_len);
            assert_eq!(bytes.len(), header_len);
            assert_eq!(decode_dw_header(&bytes), Ok(full), "header {header_len}");
        }
    }

    #[test]
    fn doublewrite_capacity_is_the_headers_actual_room() {
        assert_eq!(dw_capacity(BLOCK), 168);
        assert_eq!(dw_capacity(8 * BLOCK), 1363);
        // The capacity claim is exact: one more entry must not fit.
        let header_len = BLOCK;
        let cap = dw_capacity(header_len);
        let over = DwBatch {
            entries: (0..=cap)
                .map(|i| DwEntry {
                    segment: SegmentId(1),
                    page_no: i as u32,
                    page_lsn: Lsn(0),
                    image_crc: 0,
                })
                .collect(),
            ..batch()
        };
        let plain = std::panic::catch_unwind(|| encode_dw_header(&over, header_len));
        assert!(plain.is_err(), "one past the capacity refuses");
    }

    #[test]
    fn doublewrite_header_refuses_damage_and_lies() {
        let good = encode_dw_header(&batch(), BLOCK);
        for at in [0usize, 9, 17, 25, 44, 48, 56, 60] {
            let mut bad = good.clone();
            bad[at] ^= 0xFF;
            assert_eq!(decode_dw_header(&bad), Err(LayoutError::Corrupt), "byte {at}");
        }
        // A non-zero "reserved" byte between the entries and the checksum.
        let mut bad = good.clone();
        bad[2000] = 1;
        assert_eq!(decode_dw_header(&bad), Err(LayoutError::Corrupt));
        // A count past the header's room, with the checksum restamped so the
        // lie is the only flaw.
        let mut bad = good.clone();
        bad[32..36].copy_from_slice(&(dw_capacity(BLOCK) as u32 + 1).to_le_bytes());
        let sum = crc(&bad[..BLOCK - 4]);
        bad[BLOCK - 4..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_dw_header(&bad), Err(LayoutError::Corrupt));
        // A cleared region — what the store writes after a batch lands.
        assert_eq!(decode_dw_header(&[0u8; BLOCK]), Err(LayoutError::Corrupt));
        // A region that is not whole blocks.
        assert_eq!(decode_dw_header(&good[..BLOCK - 1]), Err(LayoutError::Corrupt));
    }
}
