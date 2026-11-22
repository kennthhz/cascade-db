//! M4 red: the flush frame's on-disk law (§5.2), pinned before the
//! encoder exists. A frame is the unit of one shared flush: a 64-byte
//! header, `extent_count` extents (each scope's log bytes for one piece
//! of the flush, or a control payload), a CRC32 trailer, and padding to
//! the next 4 KiB boundary. `frame_seq` continues its stream;
//! `segment_seq` must equal the segment file's sequence; `incarnation`
//! makes nonces unique across re-opens after a torn write.

use cascade_logstore::frame::{
    self, Extent, ExtentKind, FrameHeader, FRAME_HEADER_LEN, FRAME_MAGIC, FRAME_VERSION,
};
use cascade_types::{Epoch, Lsn, ScopeId};

/// The golden header, pinned byte for byte: magic, version, stream 1,
/// segment_seq 3, frame_seq 17, incarnation 5, payload 61, one extent,
/// header CRC over the first 60 bytes.
#[test]
fn frame_header_bytes_are_pinned() {
    let header = FrameHeader {
        stream: 1,
        segment_seq: 3,
        frame_seq: 17,
        incarnation: 5,
        payload_len: 61,
        extent_count: 1,
    };
    let bytes = frame::encode_header(&header);
    assert_eq!(bytes.len(), FRAME_HEADER_LEN);
    assert_eq!(bytes.len(), 64, "§5.2: the header is 64 bytes");
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    assert_eq!(magic, FRAME_MAGIC);
    let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    assert_eq!(version, FRAME_VERSION);
    // flags u16 at [6..8] is zero in v1
    assert_eq!(&bytes[6..8], &[0; 2]);
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 1);
    assert_eq!(u64::from_le_bytes(bytes[12..20].try_into().unwrap()), 3);
    assert_eq!(u64::from_le_bytes(bytes[20..28].try_into().unwrap()), 17);
    assert_eq!(u64::from_le_bytes(bytes[28..36].try_into().unwrap()), 5);
    assert_eq!(u32::from_le_bytes(bytes[36..40].try_into().unwrap()), 61);
    assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 1);
    let stored_crc = u32::from_le_bytes(bytes[44..48].try_into().unwrap());
    assert_eq!(stored_crc, crc32(&bytes[..44]), "header_crc covers the first 44 bytes");
    // [48..64] reserved zeros.
    assert_eq!(&bytes[48..64], &[0; 16]);
}

/// An extent: scope, epoch, start_lsn, len, key_version, kind — then the
/// bytes. Records and control are the two kinds (§5.2); control is what
/// makes fencing durable (§4.3 promise 1).
#[test]
fn extent_bytes_are_pinned() {
    let extent = Extent {
        scope: ScopeId(9),
        epoch: Epoch(2),
        start_lsn: Lsn(4096),
        key_version: 1,
        kind: ExtentKind::Records,
        bytes: vec![0xAB; 32],
    };
    let bytes = frame::encode_extent(&extent);
    assert_eq!(u64::from_le_bytes(bytes[0..8].try_into().unwrap()), 9);
    assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 2);
    assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 4096);
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 32);
    assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 1);
    assert_eq!(bytes[32], frame::KIND_RECORDS);
    assert_eq!(&bytes[33..40], &[0; 7], "extent reserved");
    assert_eq!(bytes[40..72], vec![0xAB; 32][..]);
    assert_eq!(bytes.len(), 72, "40-byte extent head + bytes");
    assert_eq!(frame::KIND_CONTROL, 1, "control is kind 1");
}

/// A whole frame round-trips: header + extents + trailer CRC + padding to
/// 4 KiB. Decode rejects a flipped byte (trailer CRC), a wrong magic, and
/// a payload_len that outruns the buffer.
#[test]
fn frame_roundtrips_and_refuses() {
    let header = FrameHeader {
        stream: 0,
        segment_seq: 1,
        frame_seq: 2,
        incarnation: 1,
        payload_len: 0, // computed by the encoder
        extent_count: 2,
    };
    let extents = vec![
        Extent {
            scope: ScopeId(1),
            epoch: Epoch(1),
            start_lsn: Lsn(0),
            key_version: 1,
            kind: ExtentKind::Records,
            bytes: b"records bytes".to_vec(),
        },
        Extent {
            scope: ScopeId(2),
            epoch: Epoch(1),
            start_lsn: Lsn(0),
            key_version: 1,
            kind: ExtentKind::Control,
            bytes: b"control".to_vec(),
        },
    ];
    let frame_bytes = frame::encode_frame(&header, &extents);
    assert_eq!(frame_bytes.len() % 4096, 0, "frames pad to 4 KiB");
    assert!(frame_bytes.len() >= 64 + 40 + 72 + 4);

    let decoded = frame::decode_frame(&frame_bytes).expect("decodes");
    assert_eq!(decoded.header.stream, 0);
    assert_eq!(decoded.header.frame_seq, 2);
    assert_eq!(decoded.header.extent_count, 2);
    assert_eq!(decoded.header.payload_len, 40 + 13 + 40 + 7, "payload = extent heads + bytes");
    assert_eq!(decoded.extents.len(), 2);
    assert_eq!(decoded.extents[0].bytes, b"records bytes");
    assert_eq!(decoded.extents[1].kind, ExtentKind::Control);
    assert_eq!(decoded.total, frame_bytes.len(), "padding included in the step");

    // A flipped payload byte fails the trailer CRC.
    let mut torn = frame_bytes.clone();
    let at = 64 + 45;
    torn[at] ^= 0xFF;
    assert!(matches!(
        frame::decode_frame(&torn),
        Err(frame::FrameError::BadCrc)
    ));
    // Wrong magic.
    let mut bad_magic = frame_bytes.clone();
    bad_magic[0] ^= 0xFF;
    assert!(matches!(
        frame::decode_frame(&bad_magic),
        Err(frame::FrameError::BadMagic)
    ));
    // Truncated.
    assert!(matches!(
        frame::decode_frame(&frame_bytes[..100]),
        Err(frame::FrameError::Truncated)
    ));
}

/// The payload extent walk: given the payload region, extents decode in
/// order — recovery rebuilds extent indexes from this (§5.7).
#[test]
fn extents_walk_in_order() {
    let extents = vec![Extent {
        scope: ScopeId(3),
        epoch: Epoch(4),
        start_lsn: Lsn(8192),
        key_version: 2,
        kind: ExtentKind::Records,
        bytes: vec![1; 100],
    }];
    let payload = frame::encode_payload(&extents);
    let walked = frame::walk_payload(&payload).expect("walks");
    assert_eq!(walked.len(), 1);
    assert_eq!(walked[0].scope, ScopeId(3));
    assert_eq!(walked[0].start_lsn, Lsn(8192));
    assert_eq!(walked[0].bytes.len(), 100);
    // A payload whose last extent declares more bytes than remain is
    // corruption, never a guess.
    let mut short = payload.clone();
    short[24..28].copy_from_slice(&1_000_000u32.to_le_bytes());
    assert!(matches!(
        frame::walk_payload(&short),
        Err(frame::FrameError::BadLength)
    ));
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
