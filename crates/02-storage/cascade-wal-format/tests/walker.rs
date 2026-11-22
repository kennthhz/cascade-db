//! M1 walker tests: `walk` validates framing (length, version, CRC, flags)
//! and reports the self-describing header fields *without decoding
//! payloads* — S5's foundation. The discriminating case is a record whose
//! payload shape is invalid for its type: the walker accepts it, the
//! decoder refuses.

use cascade_types::Lsn;
use cascade_wal_format::{
    decode, encode, encode_with_flags, walk, DecodeError, FLAG_GROUP_END, HEADER_LEN,
    TYPE_BULK_LOAD, TYPE_COMMIT, WalkError, WalRecord,
};

fn commit_bytes() -> Vec<u8> {
    encode(
        &WalRecord::CommitTxn {
            xid: 9,
            commit_ts: cascade_types::Hlc(1),
        },
        Lsn(0),
        0,
    )
}

/// The walker reports every self-describing field and hands back the raw
/// payload slice — nothing more.
#[test]
fn walk_reports_header_fields_and_the_raw_payload() {
    let mut bytes = encode_with_flags(
        &WalRecord::CommitTxn {
            xid: 0x0102_0304_0506_0708,
            commit_ts: cascade_types::Hlc(0x1122_3344_5566_7788),
        },
        Lsn(0x0000_1234_5678_9ABC),
        0xABCD,
        FLAG_GROUP_END,
    );
    bytes[8] = TYPE_COMMIT; // no-op, pins the intent
    let f = walk(&bytes).expect("walks");
    assert_eq!(f.type_byte, TYPE_COMMIT);
    assert_eq!(f.lsn, 0x0000_1234_5678_9ABC);
    assert_eq!(f.epoch, 0xABCD);
    assert_eq!(f.xid, 0x0102_0304_0506_0708);
    assert_eq!(f.format_version, 3);
    assert!(f.group_end);
    assert_eq!(f.total, bytes.len());
    assert_eq!(f.payload, &bytes[HEADER_LEN..HEADER_LEN + 8]);
}

/// Payload-blindness, discriminated: a BULK_LOAD header carrying a
/// non-empty payload is shape-invalid, so `decode` refuses — while `walk`
/// accepts it, because framing never interprets payloads (S5).
#[test]
fn walk_accepts_a_payload_shape_decode_refuses() {
    // Start from BEGIN's header (empty payload), then declare a 4-byte
    // payload under BULK_LOAD — shape-invalid, but perfectly good framing.
    let mut bytes = encode(&WalRecord::BeginTxn { xid: 1 }, Lsn(0), 0);
    bytes.truncate(HEADER_LEN); // drop the CRC; restamped below
    bytes[8] = TYPE_BULK_LOAD;
    bytes[..4].copy_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());

    assert!(matches!(decode(&bytes), Err(DecodeError::Corruption)));
    let f = walk(&bytes).expect("the walker is payload-blind");
    assert_eq!(f.type_byte, TYPE_BULK_LOAD);
    assert_eq!(f.payload, &[0xDE, 0xAD, 0xBE, 0xEF]);
}

/// Truncation refuses at both boundaries: shorter than the header itself,
/// and shorter than the declared total.
#[test]
fn truncation_refuses() {
    let bytes = commit_bytes();
    assert!(matches!(walk(&bytes[..HEADER_LEN - 1]), Err(WalkError::Truncated)));
    assert!(matches!(walk(&bytes[..bytes.len() - 1]), Err(WalkError::Truncated)));
    // A declared length running past the buffer is the same refusal —
    // a header that promises more than it holds.
    let mut over = bytes.clone();
    over[..4].copy_from_slice(&(u32::MAX).to_le_bytes());
    assert!(matches!(walk(&over), Err(WalkError::Truncated)));
}

/// A flipped byte is a bad CRC to the walker, the same way it is corruption
/// to the decoder.
#[test]
fn flipped_byte_is_bad_crc() {
    let mut bytes = commit_bytes();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    assert!(matches!(walk(&bytes), Err(WalkError::BadCrc)));
    assert!(matches!(decode(&bytes), Err(DecodeError::Corruption)));
}

/// Unknown format versions refuse loudly, named.
#[test]
fn unknown_version_refuses() {
    let mut bytes = commit_bytes();
    bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
    assert!(matches!(walk(&bytes), Err(WalkError::UnknownVersion(99))));
    assert!(matches!(
        decode(&bytes),
        Err(DecodeError::UnknownRecordVersion(99))
    ));
}
