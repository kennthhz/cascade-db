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
/// them down]`) — the heap redo record, produced since T1.3; the cell is
/// opaque here and parses only through cascade-page.
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
            // `[segment u32][cell bytes verbatim]` — the cell is pre-built
            // bytes, so the length is pure arithmetic with no failure path
            // (the u16 ceiling is cascade-page's `encode_cell` gate, applied
            // before a producer ever holds a cell).
            WalRecord::HeapInsert { cell, .. } => 4 + cell.len(),
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
        WalRecord::HeapInsert { segment, cell, .. } => {
            let mut p = Vec::with_capacity(4 + cell.len());
            p.extend_from_slice(&segment.0.to_le_bytes());
            p.extend_from_slice(cell);
            (TYPE_HEAP_INSERT, p)
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
        | WalRecord::LegacyInsert { xid, .. }
        | WalRecord::HeapInsert { xid, .. } => *xid,
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
            // Strict UTF-8: a CRC-valid record whose value bytes are not
            // UTF-8 is a buggy encoder's output — corruption, never a
            // coerced-to-U+FFFD row replay would serve.
            let val = match String::from_utf8(payload[10..].to_vec()) {
                Ok(val) => val,
                Err(_) => return Err(DecodeError::Corruption),
            };
            WalRecord::LegacyInsert {
                xid,
                table,
                id,
                val,
            }
        }
        // `[segment u32][cell bytes verbatim]` — the cell is opaque at this
        // boundary (cascade-page invariant): its structural validity is
        // parse_cell's, at apply time. A CRC-valid record shorter than the
        // segment field is a buggy encoder — the `_ => Corruption` arm
        // below refuses it.
        TYPE_HEAP_INSERT if payload.len() >= 4 => {
            let segment = SegmentId(u32::from_le_bytes(
                payload[..4].try_into().expect("4 bytes"),
            ));
            WalRecord::HeapInsert {
                xid,
                segment,
                cell: payload[4..].to_vec(),
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

/// The type bytes this decoder can actually decode. A byte absent from
/// this list must refuse as [`DecodeError::UnknownRecordType`] — version
/// skew, named — never as `Corruption`, which reads as bit rot and sends
/// the operator chasing hardware. All nine F3 bytes decode since T1.3
/// (HEAP_INSERT's arm landed); the next addition is a *new* type byte.
fn is_known_type(t: u8) -> bool {
    matches!(
        t,
        TYPE_BEGIN
            | TYPE_COMMIT
            | TYPE_ABORT
            | TYPE_LEGACY_INSERT
            | TYPE_HEAP_INSERT
            | TYPE_CLOSED_TS_HEARTBEAT
            | TYPE_HLC_HIGH_WATER
            | TYPE_BULK_LOAD
            | TYPE_UNLOGGED_MARKER
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_types::{SegmentId, VersionWord, Xid};

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
            WalRecord::HeapInsert {
                xid: 9,
                segment: SegmentId(2),
                cell: cascade_page::encode_cell(VersionWord::EMPTY, b"xyz").unwrap(),
            },
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

    /// The HEAP_INSERT golden literal — the type-5 one-way door, pinned
    /// byte for byte: HeapInsert { xid: 5, segment: 7, cell = a fresh
    /// 3-byte-payload cell } at LSN 0x1000, epoch 0. The cell bytes are
    /// cascade-page's `[len][xmin = uncommitted(5)][xmax 0][next 0]
    /// [payload]` verbatim; the CRC is verified algorithmically over the
    /// pinned prefix.
    #[test]
    fn heap_insert_bytes_are_pinned() {
        let cell = cascade_page::encode_cell(VersionWord::uncommitted(Xid(5)), b"abc")
            .expect("3-byte payload fits");
        let bytes = encode(
            &WalRecord::HeapInsert {
                xid: 5,
                segment: SegmentId(7),
                cell: cell.clone(),
            },
            Lsn(0x1000),
            0,
        );
        assert_eq!(cell.len(), 26 + 3, "test sanity: CELL_FIXED + payload");
        let expected: [u8; 69] = [
            // payload_len = 4 + 29 = 33; format_version = 2; type = 5; reserved
            0x21, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
            // xid = 5
            0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // lsn = 0x1000
            0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // epoch = 0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // payload: segment = 7
            0x07, 0x00, 0x00, 0x00,
            // cell: len = 29
            0x1D, 0x00,
            // xmin = uncommitted(5): xid with bit 63 set
            0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80,
            // xmax: live; next: chain ends
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
            // payload: "abc"
            0x61, 0x62, 0x63,
        ];
        assert_eq!(bytes.len(), 69 + CRC_LEN);
        assert_eq!(
            bytes[..69],
            expected[..],
            "the HEAP_INSERT layout is a one-way door — this literal is its lock"
        );
        let crc = u32::from_le_bytes(bytes[69..].try_into().expect("4 bytes"));
        assert_eq!(crc, crc32fast::hash(&bytes[..69]));
    }

    /// The HEAP_INSERT seam (WP1 F3 ↔ cascade-page): a cell built by
    /// cascade-page rides through encode → decode byte-identical and
    /// `parse_cell` reads it back, words and payload intact. "Verbatim"
    /// pinned — the WAL never re-encodes a cell, so any drift here is a
    /// bug, not a format decision.
    #[test]
    fn heap_insert_cell_is_verbatim_through_encode_decode() {
        let payload = b"the debug row image";
        let cell = cascade_page::encode_cell(VersionWord::uncommitted(Xid(77)), payload)
            .expect("small payload fits");
        let bytes = encode(
            &WalRecord::HeapInsert {
                xid: 77,
                segment: SegmentId(3),
                cell: cell.clone(),
            },
            Lsn(0x2000),
            0,
        );
        let decoded = decode(&bytes).expect("decodes");
        assert_eq!(decoded.total, bytes.len());
        match decoded.record {
            WalRecord::HeapInsert {
                xid,
                segment,
                cell: out,
            } => {
                assert_eq!((xid, segment.0), (77, 3));
                assert_eq!(out, cell, "cell bytes verbatim");
                let r = cascade_page::parse_cell(&out).expect("parses");
                assert_eq!(r.xmin, VersionWord::uncommitted(Xid(77)));
                assert_eq!(r.xmax, VersionWord::EMPTY);
                assert_eq!(r.next, cascade_page::NextWord::NONE);
                assert_eq!(r.payload, payload);
            }
            other => panic!("wrong record: {other:?}"),
        }
    }

    /// A CRC-valid HEAP_INSERT shorter than its segment field is corruption
    /// — payload shape is part of the type contract, and the decoder
    /// refuses rather than guess (only a buggy encoder produces one). The
    /// pre-T1.3 twin of this test asserted byte 5 as *unknown*; the decode
    /// arm landed, so the same byte now fails one step later, on shape.
    #[test]
    fn heap_insert_short_payload_is_corruption() {
        let mut bytes = encode(&WalRecord::BeginTxn { xid: 1 }, Lsn(0), 0);
        bytes[8] = TYPE_HEAP_INSERT;
        let n = bytes.len();
        let crc = crc32fast::hash(&bytes[..n - CRC_LEN]);
        bytes[n - CRC_LEN..].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(decode(&bytes), Err(DecodeError::Corruption)));
    }

    /// Decode law (T1.5): a CRC-valid LEGACY_INSERT whose value bytes are
    /// not UTF-8 is corruption — the decoder refuses, it never coerces to
    /// U+FFFD and hands replay a row nobody wrote. The CRC is restamped so
    /// the value bytes are the *only* flaw.
    #[test]
    fn legacy_insert_decode_refuses_non_utf8_value() {
        let rec = legacy_insert(1, 5, "victim");
        let mut bytes = encode(&rec, Lsn(0x2000), 0);
        // The value is the payload's last 6 bytes, ending right before the
        // CRC — overwrite them with 0xFD, which is invalid in any UTF-8
        // position, then re-stamp the CRC over the mutated prefix.
        let crc_at = bytes.len() - CRC_LEN;
        for b in &mut bytes[crc_at - 6..crc_at] {
            *b = 0xFD;
        }
        let crc = crc32fast::hash(&bytes[..crc_at]);
        bytes[crc_at..].copy_from_slice(&crc.to_le_bytes());
        match decode(&bytes) {
            Err(DecodeError::Corruption) => {}
            other => panic!("non-UTF-8 value must refuse as Corruption, got {other:?}"),
        }
    }

    /// T1.5 roundtrip law: decode inverts encode for EVERY variant — the
    /// reserved families (6–9) included, sampled payloads (deterministic
    /// xorshift, replayable from the seed) — and `encoded_len` agrees with
    /// the encoded size throughout (the reserve-before-encode precondition).
    #[test]
    fn every_variant_roundtrips_with_sampled_payloads() {
        let mut s = 0x57A1_C0DE_F00D_BEEFu64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut records = Vec::new();
        for i in 0..4u64 {
            let len = (next() % 96) as usize;
            let cell = cascade_page::encode_cell(
                VersionWord::uncommitted(Xid(100 + i)),
                &next().to_le_bytes().repeat(len / 8 + 1)[..len],
            )
            .expect("small cells fit");
            records.push(WalRecord::HeapInsert {
                xid: 100 + i,
                segment: SegmentId(next() as u32),
                cell,
            });
        }
        records.push(WalRecord::BeginTxn { xid: next() });
        records.push(WalRecord::AbortTxn { xid: next() });
        records.push(WalRecord::CommitTxn {
            xid: next() % (1 << 63),
            commit_ts: Hlc(next() % (1 << 63)),
        });
        records.push(legacy_insert(
            next(),
            next() as i32,
            &"row image".repeat((next() % 8) as usize),
        ));
        records.push(WalRecord::ClosedTsHeartbeat {
            closed_ts: next(),
        });
        records.push(WalRecord::HlcHighWater { max_issued: next() });
        records.push(WalRecord::BulkLoad);
        records.push(WalRecord::UnloggedMarker);

        for rec in &records {
            let (lsn, epoch) = (Lsn(next()), next());
            let bytes = encode(rec, lsn, epoch);
            assert_eq!(encoded_len(rec), bytes.len(), "{rec:?}: reserve == encode");
            let d = decode(&bytes).expect("every variant decodes");
            assert_eq!(d.record, *rec, "{rec:?}: roundtrip is exact");
            assert_eq!(d.total, bytes.len());
            assert_eq!(d.lsn, lsn.0, "self-describing lsn");
            assert_eq!(d.epoch, epoch, "self-describing epoch");
        }
    }

    /// Header-xid law (F3): the txn-carrying types (1–5) stamp the
    /// variant's xid; the no-txn families (6–9) stamp 0 — a decoder can
    /// tell them apart from the header alone.
    #[test]
    fn header_xid_is_variant_xid_or_zero_for_no_txn_families() {
        let cell = cascade_page::encode_cell(VersionWord::EMPTY, b"c").expect("fits");
        let cases: [(WalRecord, u64); 9] = [
            (WalRecord::BeginTxn { xid: 7 }, 7),
            (
                WalRecord::CommitTxn {
                    xid: 7,
                    commit_ts: Hlc(1),
                },
                7,
            ),
            (WalRecord::AbortTxn { xid: 7 }, 7),
            (legacy_insert(7, 1, "v"), 7),
            (
                WalRecord::HeapInsert {
                    xid: 7,
                    segment: SegmentId(1),
                    cell: cell.clone(),
                },
                7,
            ),
            (WalRecord::ClosedTsHeartbeat { closed_ts: 1 }, 0),
            (WalRecord::HlcHighWater { max_issued: 1 }, 0),
            (WalRecord::BulkLoad, 0),
            (WalRecord::UnloggedMarker, 0),
        ];
        for (rec, want) in cases {
            let b = encode(&rec, Lsn(0), 0);
            assert_eq!(
                u64::from_le_bytes(b[12..20].try_into().expect("8 bytes")),
                want,
                "{rec:?}: header xid"
            );
        }
    }

    /// The three reserved header bytes stay zero in every encoding — they
    /// are the §6.3 retrofit surface, not scratch space, and a producer
    /// that dirties them would pin garbage into every future reader's
    /// assumptions.
    #[test]
    fn reserved_header_bytes_stay_zero() {
        let cell = cascade_page::encode_cell(VersionWord::EMPTY, b"c").expect("fits");
        for rec in [
            WalRecord::BeginTxn { xid: 1 },
            WalRecord::CommitTxn {
                xid: 2,
                commit_ts: Hlc(3),
            },
            WalRecord::HeapInsert {
                xid: 4,
                segment: SegmentId(5),
                cell,
            },
            WalRecord::BulkLoad,
        ] {
            let b = encode(&rec, Lsn(0), 0);
            assert_eq!(&b[9..12], &[0u8; 3], "{rec:?}: reserved bytes");
        }
    }
}
