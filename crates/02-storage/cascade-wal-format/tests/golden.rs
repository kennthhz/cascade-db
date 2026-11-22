//! M1 golden tests: the v3 layout is pinned byte for byte, v2 bytes still
//! decode (each treated as a group of one — S16), and unknown flag bits
//! refuse loudly. The literals here are one-way doors: changing the layout
//! means rewriting them deliberately, never accidentally.

use cascade_types::{Hlc, Lsn};
use cascade_wal_format::{
    decode, encode, encode_with_flags, walk, CRC_LEN, DecodeError, FLAG_GROUP_END, FORMAT_VERSION,
    HEADER_LEN, WalkError, WalRecord,
};

const COMMIT: WalRecord = WalRecord::CommitTxn {
    xid: 5,
    commit_ts: Hlc(0x1122_3344_5566_7788),
};

/// The v3 one-way door, pinned byte for byte: COMMIT at LSN 0x1000, epoch
/// 0, group-end set — payload_len, format_version, type, *flags*, reserved,
/// xid, lsn, epoch, packed commit_ts, in exactly these offsets (WP1 F3 as
/// amended by M1: the first reserved byte became the flags byte; the header
/// stays 36 bytes). The CRC is verified algorithmically over the pinned
/// prefix.
#[test]
fn v3_header_bytes_are_pinned_group_end_set() {
    let bytes = encode_with_flags(&COMMIT, Lsn(0x1000), 0, FLAG_GROUP_END);
    let expected: [u8; 44] = [
        // payload_len = 8; format_version = 3; type = COMMIT; flags = GROUP_END; reserved
        0x08, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x01, 0x00, 0x00,
        // xid = 5
        0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // lsn = 0x1000
        0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // epoch = 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // commit_ts LE
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
    ];
    assert_eq!(bytes.len(), HEADER_LEN + 8 + CRC_LEN);
    assert_eq!(
        bytes[..HEADER_LEN + 8],
        expected[..],
        "the v3 header layout is a one-way door — this literal is its lock"
    );
    let crc = u32::from_le_bytes(bytes[HEADER_LEN + 8..].try_into().expect("4 bytes"));
    assert_eq!(crc, crc32fast::hash(&bytes[..HEADER_LEN + 8]));
}

/// Plain `encode` emits flags 0: the interior records of a mini-transaction
/// carry no group-end (S16: the flag rides the *last* record of a group).
#[test]
fn plain_encode_leaves_the_flags_byte_zero() {
    let bytes = encode(&COMMIT, Lsn(0x1000), 0);
    assert_eq!(bytes[9], 0x00, "flags byte");
    assert_eq!(bytes[10..12], [0u8; 2], "the remaining reserved bytes");
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), FORMAT_VERSION);
}

/// The v2 golden, kept: hand-built v2 bytes (format_version = 2, the flags
/// position a reserved zero) still decode, and both the decoder and the
/// walker report them as a group of one — pre-S16 logs replay as if every
/// record ended its own mini-transaction.
#[test]
fn v2_golden_still_decodes_as_a_group_of_one() {
    let mut bytes: Vec<u8> = [
        // payload_len = 8; format_version = 2; type = COMMIT; reserved (3)
        0x08, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
        // xid = 5
        0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // lsn = 0x1000
        0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // epoch = 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // commit_ts LE
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
    ]
    .into();
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());

    let d = decode(&bytes).expect("v2 bytes decode");
    assert_eq!(d.format_version, 2);
    assert!(!d.group_end, "v2 records are groups of one");
    assert_eq!(d.lsn, 0x1000);
    assert_eq!(d.epoch, 0);
    match d.record {
        WalRecord::CommitTxn { xid, commit_ts } => {
            assert_eq!((xid, commit_ts.0), (5, 0x1122_3344_5566_7788));
        }
        other => panic!("wrong record: {other:?}"),
    }

    let f = walk(&bytes).expect("v2 bytes walk");
    assert_eq!(f.format_version, 2);
    assert!(!f.group_end);
    assert_eq!(f.total, bytes.len());
}

/// An unknown flag bit is a refusal, never a shrug: a producer writing
/// flags this decoder does not know cannot have its group semantics guessed
/// — treating an unknown word as "not a group end" risks splitting a
/// mini-transaction across a durability boundary (S16).
#[test]
fn unknown_flag_bits_refuse_on_decode_and_walk() {
    let mut bytes = encode_with_flags(&COMMIT, Lsn(0), 0, FLAG_GROUP_END);
    bytes[9] = 0b0000_0010; // bit 1: undefined in v3
    let n = bytes.len();
    let crc = crc32fast::hash(&bytes[..n - CRC_LEN]);
    bytes[n - CRC_LEN..].copy_from_slice(&crc.to_le_bytes());

    assert!(matches!(decode(&bytes), Err(DecodeError::Corruption)));
    assert!(matches!(walk(&bytes), Err(WalkError::UnknownFlags(0b10))));
}

/// A v3 stream ends its mini-transactions on group ends; stepping the
/// stream with the walker's `total` reports the flag on exactly the records
/// that carry it.
#[test]
fn group_end_rides_exactly_the_flagged_records() {
    let interior = encode(&COMMIT, Lsn(0), 0);
    let terminal = encode_with_flags(&COMMIT, Lsn(interior.len() as u64), 0, FLAG_GROUP_END);
    let mut stream = interior.clone();
    stream.extend_from_slice(&terminal);

    let first = walk(&stream).expect("first record walks");
    assert!(!first.group_end);
    let second = walk(&stream[first.total..]).expect("second record walks");
    assert!(second.group_end);
    assert_eq!(first.total + second.total, stream.len());
}
