//! The flush frame's on-disk format (§5.2) — the unit of one shared
//! flush. Pure byte functions: encode/decode/walk, no I/O. A frame is a
//! 64-byte header, `extent_count` extents, a CRC32 trailer over
//! everything above, and padding to the next 4 KiB boundary — the pad is
//! what lets every segment position stay 4 KiB-aligned for `O_DIRECT`.
//!
//! `frame_seq` continues its stream (recovery walks frame_seq + 1 — a gap
//! after a bad frame means the bad frame was synced and later damaged:
//! corruption, not a torn write, §5.7); `segment_seq` must equal the
//! sequence in the segment file's name (a stale frame left in a reused
//! segment cannot pass); `incarnation` increases every open (recorded in
//! the manifest before any frame is written) so encryption nonces stay
//! unique across a torn frame's rewritten position (§5.2, §7).
//!
//! An extent's `kind`: *records* carries one scope's log bytes; *control*
//! records an epoch change, a scope creation or a drop — control extents
//! are what make fencing durable (`open_writer` returns only after its
//! control extent's frame is synced, §4.3 promise 1).

use cascade_types::{Epoch, Lsn, ScopeId};

/// The header magic — one-way door.
pub const FRAME_MAGIC: u32 = 0x4341_4C46; // "CALF" — Cascade log frame
pub const FRAME_VERSION: u16 = 1;
/// §5.2: the header is 64 bytes.
pub const FRAME_HEADER_LEN: usize = 64;
/// The fixed head of one extent, before its bytes.
pub const EXTENT_HEAD_LEN: usize = 40;
/// Extent kinds: records = 0, control = 1 (§5.2).
pub const KIND_RECORDS: u8 = 0;
pub const KIND_CONTROL: u8 = 1;
/// 4 KiB — frame padding and segment alignment (§5.2).
pub const PAD: usize = 4096;

/// The 64-byte frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub stream: u32,
    pub segment_seq: u64,
    pub frame_seq: u64,
    pub incarnation: u64,
    /// Extent heads + extent bytes, trailer excluded.
    pub payload_len: u32,
    pub extent_count: u32,
}

/// One scope's piece of a flush (or a control record for the stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extent {
    pub scope: ScopeId,
    pub epoch: Epoch,
    pub start_lsn: Lsn,
    /// The key version the bytes were sealed with (§7; plaintext passes
    /// version 0 through until real sealing lands).
    pub key_version: u32,
    pub kind: ExtentKind,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentKind {
    Records,
    Control,
}

impl ExtentKind {
    const fn byte(self) -> u8 {
        match self {
            ExtentKind::Records => KIND_RECORDS,
            ExtentKind::Control => KIND_CONTROL,
        }
    }
    fn from_byte(b: u8) -> Option<ExtentKind> {
        match b {
            KIND_RECORDS => Some(ExtentKind::Records),
            KIND_CONTROL => Some(ExtentKind::Control),
            _ => None,
        }
    }
}

/// What stopped a frame decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    Truncated,
    BadMagic,
    BadVersion,
    BadHeaderCrc,
    BadCrc,
    BadLength,
    UnknownKind,
}

/// One decoded frame: the header, the extents, and the total step
/// (padding included) to the next frame position.
#[derive(Debug)]
pub struct DecodedFrame {
    pub header: FrameHeader,
    pub extents: Vec<Extent>,
    pub total: usize,
}

/// Encodes the 64-byte header; `header_crc` covers the first 44 bytes.
pub fn encode_header(h: &FrameHeader) -> [u8; FRAME_HEADER_LEN] {
    let mut b = [0u8; FRAME_HEADER_LEN];
    b[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
    b[4..6].copy_from_slice(&FRAME_VERSION.to_le_bytes());
    b[6..8].copy_from_slice(&0u16.to_le_bytes()); // flags: none in v1
    b[8..12].copy_from_slice(&h.stream.to_le_bytes());
    b[12..20].copy_from_slice(&h.segment_seq.to_le_bytes());
    b[20..28].copy_from_slice(&h.frame_seq.to_le_bytes());
    b[28..36].copy_from_slice(&h.incarnation.to_le_bytes());
    b[36..40].copy_from_slice(&h.payload_len.to_le_bytes());
    b[40..44].copy_from_slice(&h.extent_count.to_le_bytes());
    let crc = crc32(&b[..44]);
    b[44..48].copy_from_slice(&crc.to_le_bytes());
    // [48..64] reserved zeros.
    b
}

/// Encodes one extent: the 40-byte head, then its bytes.
pub fn encode_extent(e: &Extent) -> Vec<u8> {
    let mut b = Vec::with_capacity(EXTENT_HEAD_LEN + e.bytes.len());
    b.extend_from_slice(&e.scope.0.to_le_bytes());
    b.extend_from_slice(&e.epoch.0.to_le_bytes());
    b.extend_from_slice(&e.start_lsn.0.to_le_bytes());
    b.extend_from_slice(&(e.bytes.len() as u32).to_le_bytes());
    b.extend_from_slice(&e.key_version.to_le_bytes());
    b.push(e.kind.byte());
    b.extend_from_slice(&[0u8; 7]); // reserved
    b.extend_from_slice(&e.bytes);
    b
}

/// Encodes the payload region: extent heads + bytes, concatenated.
pub fn encode_payload(extents: &[Extent]) -> Vec<u8> {
    let mut b = Vec::new();
    for e in extents {
        b.extend(encode_extent(e));
    }
    b
}

/// Encodes a whole frame: header, payload, trailer CRC, padding to 4 KiB.
/// Returns the padded frame bytes; `total` for the next position is the
/// padded length.
pub fn encode_frame(header: &FrameHeader, extents: &[Extent]) -> Vec<u8> {
    let payload = encode_payload(extents);
    debug_assert_eq!(payload.len() as u32, {
        let mut want = 0u32;
        for e in extents {
            want += (EXTENT_HEAD_LEN + e.bytes.len()) as u32;
        }
        want
    });
    let mut body = Vec::with_capacity(FRAME_HEADER_LEN + payload.len() + 4);
    body.extend_from_slice(&encode_header(&FrameHeader {
        payload_len: payload.len() as u32,
        extent_count: extents.len() as u32,
        ..*header
    }));
    body.extend_from_slice(&payload);
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    // Pad to the next 4 KiB boundary (zero pad is fine: the trailer CRC
    // covers only everything above it, and the padding is never read).
    let pad = (PAD - body.len() % PAD) % PAD;
    body.extend(std::iter::repeat(0u8).take(pad));
    body
}

/// Walks the payload region into extents, in order. A declared length
/// outrunning the region is corruption (`BadLength`), never a guess —
/// recovery rebuilds extent indexes through this walk (§5.7).
pub fn walk_payload(payload: &[u8]) -> Result<Vec<Extent>, FrameError> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < payload.len() {
        if payload.len() - at < EXTENT_HEAD_LEN {
            return Err(FrameError::BadLength);
        }
        let scope = ScopeId(u64::from_le_bytes(
            payload[at..at + 8].try_into().expect("8 bytes"),
        ));
        let epoch = Epoch(u64::from_le_bytes(
            payload[at + 8..at + 16].try_into().expect("8 bytes"),
        ));
        let start_lsn = Lsn(u64::from_le_bytes(
            payload[at + 16..at + 24].try_into().expect("8 bytes"),
        ));
        let len =
            u32::from_le_bytes(payload[at + 24..at + 28].try_into().expect("4 bytes")) as usize;
        let key_version =
            u32::from_le_bytes(payload[at + 28..at + 32].try_into().expect("4 bytes"));
        let kind = ExtentKind::from_byte(payload[at + 32]).ok_or(FrameError::UnknownKind)?;
        let bytes_at = at + EXTENT_HEAD_LEN;
        let end = bytes_at + len;
        if payload.len() < end {
            return Err(FrameError::BadLength);
        }
        out.push(Extent {
            scope,
            epoch,
            start_lsn,
            key_version,
            kind,
            bytes: payload[bytes_at..end].to_vec(),
        });
        at = end;
    }
    Ok(out)
}

/// Decodes only the 64-byte header: magic, version, header CRC — the
/// recovery scan's cheap probe before committing to a full-frame read.
pub fn decode_header_only(b: &[u8]) -> Result<FrameHeader, FrameError> {
    if b.len() < FRAME_HEADER_LEN {
        return Err(FrameError::Truncated);
    }
    let magic = u32::from_le_bytes(b[0..4].try_into().expect("4 bytes"));
    if magic != FRAME_MAGIC {
        return Err(FrameError::BadMagic);
    }
    let version = u16::from_le_bytes(b[4..6].try_into().expect("2 bytes"));
    if version != FRAME_VERSION {
        return Err(FrameError::BadVersion);
    }
    let stored_hcrc = u32::from_le_bytes(b[44..48].try_into().expect("4 bytes"));
    if crc32(&b[..44]) != stored_hcrc {
        return Err(FrameError::BadHeaderCrc);
    }
    Ok(FrameHeader {
        stream: u32::from_le_bytes(b[8..12].try_into().expect("4 bytes")),
        segment_seq: u64::from_le_bytes(b[12..20].try_into().expect("8 bytes")),
        frame_seq: u64::from_le_bytes(b[20..28].try_into().expect("8 bytes")),
        incarnation: u64::from_le_bytes(b[28..36].try_into().expect("8 bytes")),
        payload_len: u32::from_le_bytes(b[36..40].try_into().expect("4 bytes")),
        extent_count: u32::from_le_bytes(b[40..44].try_into().expect("4 bytes")),
    })
}

/// Decodes a whole frame from `buf[0]`: length, magic, version, header
/// CRC, payload bounds, trailer CRC — then the payload walk.
pub fn decode_frame(buf: &[u8]) -> Result<DecodedFrame, FrameError> {
    if buf.len() < FRAME_HEADER_LEN + 4 {
        return Err(FrameError::Truncated);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes"));
    if magic != FRAME_MAGIC {
        return Err(FrameError::BadMagic);
    }
    let version = u16::from_le_bytes(buf[4..6].try_into().expect("2 bytes"));
    if version != FRAME_VERSION {
        return Err(FrameError::BadVersion);
    }
    let stored_hcrc = u32::from_le_bytes(buf[44..48].try_into().expect("4 bytes"));
    if crc32(&buf[..44]) != stored_hcrc {
        return Err(FrameError::BadHeaderCrc);
    }
    let header = FrameHeader {
        stream: u32::from_le_bytes(buf[8..12].try_into().expect("4 bytes")),
        segment_seq: u64::from_le_bytes(buf[12..20].try_into().expect("8 bytes")),
        frame_seq: u64::from_le_bytes(buf[20..28].try_into().expect("8 bytes")),
        incarnation: u64::from_le_bytes(buf[28..36].try_into().expect("8 bytes")),
        payload_len: u32::from_le_bytes(buf[36..40].try_into().expect("4 bytes")),
        extent_count: u32::from_le_bytes(buf[40..44].try_into().expect("4 bytes")),
    };
    let total_unpadded = FRAME_HEADER_LEN + header.payload_len as usize + 4;
    if buf.len() < total_unpadded {
        return Err(FrameError::Truncated);
    }
    let stored_crc = u32::from_le_bytes(
        buf[FRAME_HEADER_LEN + header.payload_len as usize..total_unpadded]
            .try_into()
            .expect("4 bytes"),
    );
    if crc32(&buf[..FRAME_HEADER_LEN + header.payload_len as usize]) != stored_crc {
        return Err(FrameError::BadCrc);
    }
    let extents = walk_payload(&buf[FRAME_HEADER_LEN..FRAME_HEADER_LEN + header.payload_len as usize])?;
    if extents.len() as u32 != header.extent_count {
        return Err(FrameError::BadLength);
    }
    // The step to the next frame: padded to the 4 KiB boundary (§5.2).
    let total = total_unpadded + (PAD - total_unpadded % PAD) % PAD;
    Ok(DecodedFrame {
        header,
        extents,
        total,
    })
}

/// CRC32 (IEEE) — the same polynomial `crc32fast` uses. The golden tests
/// cross-check the encoder's CRCs against this function, so a drift in
/// either is caught at the boundary.
pub(crate) fn crc32(data: &[u8]) -> u32 {
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
