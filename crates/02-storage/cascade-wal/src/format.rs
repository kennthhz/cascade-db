//! Record encoding/decoding — the only code in the workspace that knows the
//! on-disk WAL byte layout (v2, hand-rolled; WP1 F3).
//!
//! Layout: `[u32 LE payload_len][u32 LE format_version = 2][u8 type][3× u8
//! reserved][u64 LE xid][u64 LE lsn][u64 LE epoch][payload][u32 LE CRC32 of
//! all preceding bytes]` — a 36-byte header. The reserved bytes and the
//! 32-bit version field are the retrofit surface §6.3 reserves: new record
//! *types* extend `type` (type bytes are immutable forever, never
//! renumbered); an incompatible *format* change bumps `format_version` so
//! old decoders fail loudly instead of misreading.
//!
//! The explicit `lsn` (this record's first byte) makes records
//! self-describing (§5): recovery and the PageStore can position without
//! trusting the file offset they happened to read at — [`crate::WalReader`]
//! cross-checks the declared LSN against the read position and refuses a
//! transplant. The `epoch` field reserves §4 scope fencing in the format so
//! the multi-owner era needs no format bump (0 until multi-owner). The CRC
//! covers `header ++ payload` — the new header fields are protected exactly
//! like the payload.

use cascade_types::{Hlc, Lsn, SegmentId};

use crate::WalRecord;

/// Every record starts with this header and ends with a CRC over
/// `header ++ payload`. Public: the on-disk layout is a contract —
/// recovery tooling and tests reason about byte offsets in it.
pub const HEADER_LEN: usize = 36;
pub const CRC_LEN: usize = 4;
pub(crate) const FORMAT_VERSION: u32 = 2;

// Record types (WP1 F3's table — bytes stable forever; new types extend,
// never renumber). Header `xid` is 0 for the no-txn types (6–9).
/// §5 BEGIN — kept (begin is optional in §5); empty payload.
pub const TYPE_BEGIN: u8 = 1;
/// §6 COMMIT — payload is the packed `commit_ts` [`Hlc`] u64; the header
/// `xid` is the committing txn. The status table derives commit ordering
/// from commit_ts, not from LSN adjacency.
pub const TYPE_COMMIT: u8 = 2;
/// §5 ABORT — empty payload.
pub const TYPE_ABORT: u8 = 3;
/// The v1 debug row image (LEGACY_INSERT) — retained so existing logs
/// replay verbatim. The runtime stops emitting it when the HEAP_INSERT
/// producer lands (T1.3).
pub const TYPE_LEGACY_INSERT: u8 = 4;
/// §6 HEAP_INSERT (`[segment u32][cell bytes verbatim as cascade-page lays
/// them down]`) — the type byte is reserved here; its variant and payload
/// land with the heap redo producer (T1.3). Until then a decoder meeting
/// byte 5 refuses it as *unknown* (version skew), never as corruption.
pub const TYPE_HEAP_INSERT: u8 = 5;
/// §7 idle-scope advance — payload `closed_ts` u64. Reserved: nothing
/// emits it yet.
pub const TYPE_CLOSED_TS_HEARTBEAT: u8 = 6;
/// §6 primary restart — payload `max_issued` u64. Reserved.
pub const TYPE_HLC_HIGH_WATER: u8 = 7;
/// §17 bulk load — payload shape lands with the bulk era (a non-empty
/// shape bumps `format_version`). Reserved: nothing emits it yet.
pub const TYPE_BULK_LOAD: u8 = 8;
/// §17 non-logged path — payload shape lands with that feature. Reserved.
pub const TYPE_UNLOGGED_MARKER: u8 = 9;

/// The legacy insert's payload length, validating the u16 value-length
/// field's ceiling. The one size gate both sides call: the writer, before
/// it reserves an LSN (a record that cannot encode must refuse *before*
/// consuming LSN space — a reserved-but-unfillable gap wedges the log
/// forever), and the encoder, before it builds bytes.
fn legacy_payload_len(val: &str) -> usize {
    assert!(
        val.len() <= u16::MAX as usize,
        "row value of {} bytes exceeds the legacy WAL payload limit ({}); \
         the heap redo record lifts this",
        val.len(),
        u16::MAX
    );
    4 + 4 + 2 + val.len()
}

/// A record's encoded length — the pure, header-only arithmetic the writer
/// needs *before* it reserves an LSN ([`encode`] then stamps the real one;
/// the length never depends on the LSN). Size validation happens here, so
/// `append_wal` refuses an unencodable record before its `fetch_add`.
pub(crate) fn encoded_len(rec: &WalRecord) -> usize {
    HEADER_LEN
        + CRC_LEN
        + match rec {
            WalRecord::BeginTxn { .. }
            | WalRecord::AbortTxn { .. }
            | WalRecord::BulkLoad
            | WalRecord::UnloggedMarker => 0,
            WalRecord::CommitTxn { .. }
            | WalRecord::ClosedTsHeartbeat { .. }
            | WalRecord::HlcHighWater { .. } => 8,
            WalRecord::LegacyInsert { val, .. } => legacy_payload_len(val),
        }
}

/// Encode one record to the v2 wire/byte format at `lsn` (the record's
/// first byte — the writer stamps the offset `append_wal` reserved; a tool
/// appending to a log knows its own offsets) with the owning scope's
/// `epoch` (0 until multi-owner, §4). Public for tools that *write or
/// dump* WAL bytes outside this crate (benchmarks, `pg_waldump`
/// equivalents); readers must go through [`crate::WalReader`], which is
/// the only path that validates CRCs and cross-checks the declared LSN.
pub fn encode(rec: &WalRecord, lsn: Lsn, epoch: u64) -> Vec<u8> {
    let (type_byte, payload): (u8, Vec<u8>) = match rec {
        WalRecord::BeginTxn { .. } => (TYPE_BEGIN, Vec::new()),
        WalRecord::CommitTxn { commit_ts, .. } => {
            (TYPE_COMMIT, commit_ts.0.to_le_bytes().to_vec())
        }
        WalRecord::AbortTxn { .. } => (TYPE_ABORT, Vec::new()),
        WalRecord::LegacyInsert { table, id, val, .. } => {
            // Full row image (I8): table segment + every column. The u16
            // value-length ceiling is validated by [`legacy_payload_len`]
            // — the same gate the writer's reserve goes through.
            let mut p = Vec::with_capacity(legacy_payload_len(val));
            p.extend_from_slice(&table.0.to_le_bytes());
            p.extend_from_slice(&id.to_le_bytes());
            p.extend_from_slice(&(val.len() as u16).to_le_bytes());
            p.extend_from_slice(val.as_bytes());
            (TYPE_LEGACY_INSERT, p)
        }
        WalRecord::ClosedTsHeartbeat { closed_ts } => (
            TYPE_CLOSED_TS_HEARTBEAT,
            closed_ts.to_le_bytes().to_vec(),
        ),
        WalRecord::HlcHighWater { max_issued } => (
            TYPE_HLC_HIGH_WATER,
            max_issued.to_le_bytes().to_vec(),
        ),
        WalRecord::BulkLoad => (TYPE_BULK_LOAD, Vec::new()),
        WalRecord::UnloggedMarker => (TYPE_UNLOGGED_MARKER, Vec::new()),
    };
    // The txn-carrying types take the variant's xid; the no-txn families
    // (6–9) stamp 0 (F3: "xid u64 (or 0 for no-txn records)").
    let xid = match rec {
        WalRecord::BeginTxn { xid }
        | WalRecord::CommitTxn { xid, .. }
        | WalRecord::AbortTxn { xid }
        | WalRecord::LegacyInsert { xid, .. } => *xid,
        WalRecord::ClosedTsHeartbeat { .. }
        | WalRecord::HlcHighWater { .. }
        | WalRecord::BulkLoad
        | WalRecord::UnloggedMarker => 0,
    };

    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len() + CRC_LEN);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.push(type_byte);
    buf.extend_from_slice(&[0u8; 3]);
    buf.extend_from_slice(&xid.to_le_bytes());
    buf.extend_from_slice(&lsn.0.to_le_bytes());
    buf.extend_from_slice(&epoch.to_le_bytes());
    buf.extend_from_slice(&payload);
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

/// What stopped a decode: bad bytes (`Corruption`), a version we cannot
/// read (`UnknownRecordVersion`), or a type byte written by a producer we
/// do not know (`UnknownRecordType` — the loud refusal half of "extend,
/// never renumber").
#[derive(Debug)]
pub(crate) enum DecodeError {
    Corruption,
    UnknownRecordVersion(u32),
    UnknownRecordType(u8),
}

/// One decoded record: the record itself plus the self-describing header
/// fields ([`Decoded::lsn`], [`Decoded::epoch`]) the reader validates.
pub(crate) struct Decoded {
    pub record: WalRecord,
    pub lsn: u64,
    pub epoch: u64,
    /// Total encoded length so the caller can step to the next record.
    pub total: usize,
}

/// Decodes the record starting at `buf[0]`. Payload shapes are validated
/// strictly (a CRC-valid record with a payload that disagrees with its type
/// is corruption — only a buggy encoder can produce one).
pub(crate) fn decode(buf: &[u8]) -> Result<Decoded, DecodeError> {
    if buf.len() < HEADER_LEN + CRC_LEN {
        return Err(DecodeError::Corruption);
    }
    let version = u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION {
        return Err(DecodeError::UnknownRecordVersion(version));
    }
    let payload_len = u32::from_le_bytes(buf[..4].try_into().expect("4 bytes")) as usize;
    let total = HEADER_LEN + payload_len + CRC_LEN;
    if buf.len() < total {
        return Err(DecodeError::Corruption);
    }
    let stored_crc = u32::from_le_bytes(buf[total - CRC_LEN..total].try_into().expect("4 bytes"));
    if crc32fast::hash(&buf[..total - CRC_LEN]) != stored_crc {
        return Err(DecodeError::Corruption);
    }
    let type_byte = buf[8];
    let xid = u64::from_le_bytes(buf[12..20].try_into().expect("8 bytes"));
    let lsn = u64::from_le_bytes(buf[20..28].try_into().expect("8 bytes"));
    let epoch = u64::from_le_bytes(buf[28..HEADER_LEN].try_into().expect("8 bytes"));
    let payload = &buf[HEADER_LEN..HEADER_LEN + payload_len];
    let record = match type_byte {
        TYPE_BEGIN if payload.is_empty() => WalRecord::BeginTxn { xid },
        TYPE_COMMIT if payload.len() == 8 => WalRecord::CommitTxn {
            xid,
            commit_ts: Hlc(u64::from_le_bytes(payload.try_into().expect("8 bytes"))),
        },
        TYPE_ABORT if payload.is_empty() => WalRecord::AbortTxn { xid },
        TYPE_LEGACY_INSERT if payload.len() >= 10 => {
            let table = SegmentId(u32::from_le_bytes(
                payload[..4].try_into().expect("4 bytes"),
            ));
            let id = i32::from_le_bytes(payload[4..8].try_into().expect("4 bytes"));
            let val_len = u16::from_le_bytes(payload[8..10].try_into().expect("2 bytes")) as usize;
            if payload.len() != 10 + val_len {
                return Err(DecodeError::Corruption);
            }
            let val = String::from_utf8_lossy(&payload[10..]).into_owned();
            WalRecord::LegacyInsert {
                xid,
                table,
                id,
                val,
            }
        }
        TYPE_CLOSED_TS_HEARTBEAT if payload.len() == 8 => WalRecord::ClosedTsHeartbeat {
            closed_ts: u64::from_le_bytes(payload.try_into().expect("8 bytes")),
        },
        TYPE_HLC_HIGH_WATER if payload.len() == 8 => WalRecord::HlcHighWater {
            max_issued: u64::from_le_bytes(payload.try_into().expect("8 bytes")),
        },
        TYPE_BULK_LOAD if payload.is_empty() => WalRecord::BulkLoad,
        TYPE_UNLOGGED_MARKER if payload.is_empty() => WalRecord::UnloggedMarker,
        t if !is_known_type(t) => return Err(DecodeError::UnknownRecordType(t)),
        _ => return Err(DecodeError::Corruption),
    };
    Ok(Decoded {
        record,
        lsn,
        epoch,
        total,
    })
}

/// The type bytes this decoder can actually decode. [`TYPE_HEAP_INSERT`]
/// (5) is deliberately absent until its decode arm lands (T1.3): a byte
/// this build cannot decode must refuse as
/// [`DecodeError::UnknownRecordType`] — version skew, named — never as
/// `Corruption`, which reads as bit rot and sends the operator chasing
/// hardware.
fn is_known_type(t: u8) -> bool {
    matches!(
        t,
        TYPE_BEGIN
            | TYPE_COMMIT
            | TYPE_ABORT
            | TYPE_LEGACY_INSERT
            | TYPE_CLOSED_TS_HEARTBEAT
            | TYPE_HLC_HIGH_WATER
            | TYPE_BULK_LOAD
            | TYPE_UNLOGGED_MARKER
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_types::SegmentId;

    fn legacy_insert(xid: u64, id: i32, val: &str) -> WalRecord {
        WalRecord::LegacyInsert {
            xid,
            table: SegmentId(7),
            id,
            val: val.to_owned(),
        }
    }

    #[test]
    fn legacy_insert_roundtrips_with_full_row_image() {
        let rec = legacy_insert(42, -3, "hello wal");
        let bytes = encode(&rec, Lsn(0x1000), 0);
        let decoded = decode(&bytes).expect("decodes");
        assert_eq!(decoded.total, bytes.len());
        match decoded.record {
            WalRecord::LegacyInsert {
                xid,
                table,
                id,
                val,
            } => {
                assert_eq!((xid, table.0, id, val.as_str()), (42, 7, -3, "hello wal"));
            }
            other => panic!("wrong record: {other:?}"),
        }
    }

    #[test]
    fn txn_markers_roundtrip() {
        for rec in [
            WalRecord::BeginTxn { xid: 1 },
            WalRecord::CommitTxn {
                xid: 2,
                commit_ts: Hlc(0x00AA_BBCC_0000_4321),
            },
            WalRecord::AbortTxn { xid: 3 },
        ] {
            let bytes = encode(&rec, Lsn(0), 0);
            let decoded = decode(&bytes).expect("decodes");
            assert_eq!(decoded.total, bytes.len());
            assert_eq!(format!("{:?}", decoded.record), format!("{rec:?}"));
        }
    }

    /// The reserved no-txn families decode with their pinned payload shapes
    /// and a zero header xid (F3: "xid u64 (or 0 for no-txn records)").
    #[test]
    fn reserved_no_txn_records_roundtrip() {
        for (rec, type_byte) in [
            (
                WalRecord::ClosedTsHeartbeat {
                    closed_ts: 0x1234_5678_9ABC_DEF0,
                },
                TYPE_CLOSED_TS_HEARTBEAT,
            ),
            (
                WalRecord::HlcHighWater {
                    max_issued: 0x0FED_CBA9_8765_4321,
                },
                TYPE_HLC_HIGH_WATER,
            ),
            (WalRecord::BulkLoad, TYPE_BULK_LOAD),
            (WalRecord::UnloggedMarker, TYPE_UNLOGGED_MARKER),
        ] {
            let bytes = encode(&rec, Lsn(0), 0);
            assert_eq!(bytes[8], type_byte, "type byte {type_byte}");
            assert_eq!(
                u64::from_le_bytes(bytes[12..20].try_into().expect("8 bytes")),
                0,
                "no-txn records stamp xid 0"
            );
            let decoded = decode(&bytes).expect("decodes");
            assert_eq!(format!("{:?}", decoded.record), format!("{rec:?}"));
        }
    }

    /// The v2 one-way door, pinned byte for byte: COMMIT at LSN 0x1000,
    /// epoch 0 — payload_len, format_version, type, reserved, xid, lsn,
    /// epoch, packed commit_ts, in exactly these offsets (WP1 F3). The CRC
    /// is verified algorithmically over the pinned prefix.
    #[test]
    fn v2_header_bytes_are_pinned() {
        let bytes = encode(
            &WalRecord::CommitTxn {
                xid: 5,
                commit_ts: Hlc(0x1122_3344_5566_7788),
            },
            Lsn(0x1000),
            0,
        );
        let expected: [u8; 44] = [
            // payload_len = 8; format_version = 2; type = COMMIT; reserved
            0x08, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
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
            "the v2 header layout is a one-way door — this literal is its lock"
        );
        let crc = u32::from_le_bytes(
            bytes[HEADER_LEN + 8..]
                .try_into()
                .expect("4 bytes"),
        );
        assert_eq!(crc, crc32fast::hash(&bytes[..HEADER_LEN + 8]));
    }

    /// The self-describing fields survive the roundtrip: whatever `lsn` and
    /// `epoch` the encoder was given come back out of the decoder.
    #[test]
    fn declared_lsn_and_epoch_roundtrip() {
        let rec = WalRecord::ClosedTsHeartbeat { closed_ts: 42 };
        let bytes = encode(&rec, Lsn(0x0000_1234_5678_9ABC), 0xABCD);
        let decoded = decode(&bytes).expect("decodes");
        assert_eq!(decoded.lsn, 0x0000_1234_5678_9ABC);
        assert_eq!(decoded.epoch, 0xABCD);
    }

    #[test]
    fn flipped_byte_is_corruption() {
        let mut bytes = encode(
            &WalRecord::CommitTxn {
                xid: 9,
                commit_ts: Hlc(1),
            },
            Lsn(0),
            0,
        );
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        assert!(matches!(decode(&bytes), Err(DecodeError::Corruption)));
    }

    #[test]
    fn unknown_version_is_reported() {
        let mut bytes = encode(&WalRecord::BeginTxn { xid: 1 }, Lsn(0), 0);
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(DecodeError::UnknownRecordVersion(99))
        ));
    }

    /// "Extend, never renumber" read side: a CRC-valid record whose type
    /// byte this decoder does not know refuses loudly — named, not lumped
    /// into generic corruption.
    #[test]
    fn unknown_type_byte_is_reported() {
        let mut bytes = encode(&WalRecord::BeginTxn { xid: 1 }, Lsn(0), 0);
        bytes[8] = 200;
        let n = bytes.len();
        let crc = crc32fast::hash(&bytes[..n - CRC_LEN]);
        bytes[n - CRC_LEN..].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(DecodeError::UnknownRecordType(200))
        ));
    }

    /// A CRC-valid record whose payload disagrees with its type (COMMIT
    /// carrying an empty payload here) is corruption — only a buggy
    /// encoder produces one, and the decoder refuses rather than guess.
    #[test]
    fn payload_length_mismatch_is_corruption() {
        let mut bytes = encode(&WalRecord::BeginTxn { xid: 1 }, Lsn(0), 0);
        bytes[8] = TYPE_COMMIT; // COMMIT demands an 8-byte payload; this has none
        let n = bytes.len();
        let crc = crc32fast::hash(&bytes[..n - CRC_LEN]);
        bytes[n - CRC_LEN..].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(decode(&bytes), Err(DecodeError::Corruption)));
    }

    /// The writer reserves by [`encoded_len`] and encodes after — the two
    /// must agree for every variant or the watermark walks off the records.
    #[test]
    fn encoded_len_matches_encoded_size() {
        for rec in [
            WalRecord::BeginTxn { xid: 1 },
            WalRecord::CommitTxn {
                xid: 2,
                commit_ts: Hlc(3),
            },
            WalRecord::AbortTxn { xid: 4 },
            legacy_insert(5, 6, "row image"),
            WalRecord::ClosedTsHeartbeat { closed_ts: 7 },
            WalRecord::HlcHighWater { max_issued: 8 },
            WalRecord::BulkLoad,
            WalRecord::UnloggedMarker,
        ] {
            assert_eq!(encoded_len(&rec), encode(&rec, Lsn(0), 0).len());
        }
    }

    /// The u16 value-length ceiling refuses loudly at the encoder too
    /// (tools call encode directly, without the writer's gate).
    #[test]
    #[should_panic(expected = "exceeds the legacy WAL payload limit")]
    fn oversized_legacy_value_refuses_to_encode() {
        let val = "x".repeat(u16::MAX as usize + 1);
        let _ = encode(&legacy_insert(1, 2, &val), Lsn(0), 0);
    }

    /// The reserved-but-undecodable type byte refuses *by name*: a
    /// CRC-valid record of a type this build cannot decode is version
    /// skew, and lumping it into Corruption misreads it as bit rot
    /// (invariant 7). TYPE_HEAP_INSERT is the pinned case — reserved in
    /// this build, decodable only once T1.3's arm lands.
    #[test]
    fn reserved_type_5_refuses_by_name() {
        let mut bytes = encode(&WalRecord::BeginTxn { xid: 1 }, Lsn(0), 0);
        bytes[8] = TYPE_HEAP_INSERT;
        let n = bytes.len();
        let crc = crc32fast::hash(&bytes[..n - CRC_LEN]);
        bytes[n - CRC_LEN..].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(DecodeError::UnknownRecordType(TYPE_HEAP_INSERT))
        ));
    }
}
