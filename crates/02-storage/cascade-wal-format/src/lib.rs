//! Record encoding/decoding — the only code in the workspace that knows the
//! on-disk WAL byte layout (WP1 F3; format version 3 since M1).
//!
//! Layout: `[u32 LE payload_len][u32 LE format_version][u8 type][u8 flags]
//! [2× u8 reserved][u64 LE xid][u64 LE lsn][u64 LE epoch][payload]
//! [u32 LE CRC32 of all preceding bytes]` — a 36-byte header. v3's one
//! change from v2: the first of the three reserved bytes became the *flags*
//! byte, whose bit 0 is the **group-end flag** (S16 — the mini-transaction
//! is the unit of durability: every append ends on a group end, so the
//! durable LSN always falls on a mini-transaction boundary). The header
//! length is unchanged. **v2 records still decode**, each treated as a
//! group of one; the explicit `lsn` keeps records self-describing (§5) and
//! the `epoch` field reserves scope fencing (0 until multi-owner). The
//! reserved bytes and the 32-bit version field remain the retrofit surface
//! §6.3 reserves: new record *types* extend `type` (type bytes are
//! immutable forever, never renumbered); an incompatible *format* change
//! bumps `format_version` so old decoders fail loudly instead of
//! misreading. Unknown *flag* bits refuse on both decode and walk — a
//! producer writing flags this decoder does not know cannot have its group
//! semantics guessed.
//!
//! Two read shapes live here. [`decode`] interprets payloads strictly
//! (shape errors are corruption — only a buggy encoder produces them) and
//! is what recovery and CDC use. [`walk`] is the payload-blind frame
//! walker (M1): it validates framing — length, version, CRC, flags — and
//! reports the self-describing header fields and the raw payload slice,
//! never interpreting a byte of payload. The log store (S5) reasons about
//! records through [`walk`] only: it checks framing and cuts extents at
//! group ends without ever seeing what a record *means*.
//!
//! This crate depends on nothing but `cascade-types` — it is the byte
//! contract every storage component shares, and the thing `cascade-wal`
//! re-exports so pre-M1 callers keep their paths.

use cascade_types::{Hlc, Lsn, SegmentId};

/// Every record starts with this header and ends with a CRC over
/// `header ++ payload`. Public: the on-disk layout is a contract —
/// recovery tooling and tests reason about byte offsets in it.
pub const HEADER_LEN: usize = 36;
pub const CRC_LEN: usize = 4;
pub const FORMAT_VERSION: u32 = 3;
/// The version whose bytes predate the flags byte; still decodes.
pub const V2_FORMAT_VERSION: u32 = 2;

/// Header flags byte (v3, byte offset 9): bit 0 marks the **last record of
/// a mini-transaction** (S16). Bits 1–7 are reserved and must be zero.
pub const FLAG_GROUP_END: u8 = 1 << 0;

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

/// A decoded WAL record — WP1 F3's type table. `BeginTxn`/`CommitTxn`/
/// `AbortTxn`/`HeapInsert` are the debug slice's records; `LegacyInsert`
/// (type 4) is retained for replay of pre-T1.3 logs; the reserved families
/// (heartbeat, high-water, bulk load, unlogged) decode today and
/// gain producers when their features land. Type bytes are stable forever:
/// new records extend the `type` byte, never renumber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    BeginTxn {
        xid: u64,
    },
    /// §6: the commit record carries txn_id (header `xid`) + commit_ts
    /// (payload, the packed [`Hlc`]). Commit ordering comes from
    /// commit_ts, never from LSN adjacency.
    CommitTxn {
        xid: u64,
        commit_ts: Hlc,
    },
    AbortTxn {
        xid: u64,
    },
    /// Type 4 (LEGACY_INSERT): the v1 debug row image — decode retained so
    /// logs written before T1.3 still replay. Nothing emits it anymore;
    /// the runtime's row image is [`WalRecord::HeapInsert`]. This variant
    /// is on its way out.
    LegacyInsert {
        xid: u64,
        /// The table's storage segment — where the row image lives.
        table: SegmentId,
        id: i32,
        val: String,
    },
    /// Type 5 (HEAP_INSERT, §6): the heap redo record — payload
    /// `[segment u32][cell bytes verbatim as cascade-page lays them down]`.
    /// The cell is copied as-is from `cascade_page::encode_cell` (fresh:
    /// live, chain-end); recovery and the future PageStore parse it with
    /// `cascade_page::parse_cell`, never by hand. The header `xid` names
    /// the inserting txn and the cell's `xmin` carries
    /// `VersionWord::uncommitted(xid)` — visibility resolves through the
    /// status table built from COMMIT/ABORT records (WP2), never by
    /// rewriting cells.
    HeapInsert {
        xid: u64,
        /// The storage segment the row image belongs to.
        segment: SegmentId,
        /// The cell bytes verbatim — opaque here; parsing is
        /// cascade-page's.
        cell: Vec<u8>,
    },
    /// §7 idle-scope advance (reserved): the scope's closed_ts moved to
    /// `closed_ts` with no txn activity. Nothing emits it yet.
    ClosedTsHeartbeat {
        closed_ts: u64,
    },
    /// §6 primary restart (reserved): the scope's HLC had issued up to
    /// `max_issued`. Nothing emits it yet.
    HlcHighWater {
        max_issued: u64,
    },
    /// §17 bulk load (reserved): payload shape lands with the bulk era.
    BulkLoad,
    /// §17 non-logged path (reserved).
    UnloggedMarker,
}

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
/// the length never depends on the LSN or the flags). Size validation
/// happens here, so `append_wal` refuses an unencodable record before its
/// `fetch_add`.
pub fn encoded_len(rec: &WalRecord) -> usize {
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

/// Encode one record to the v3 wire/byte format at `lsn` (the record's
/// first byte — the writer stamps the offset `append_wal` reserved; a tool
/// appending to a log knows its own offsets) with the owning scope's
/// `epoch` (0 until multi-owner, §4). No flags: the interior records of a
/// mini-transaction — use [`encode_with_flags`] with [`FLAG_GROUP_END`]
/// for the record that ends one (S16). Public for tools that *write or
/// dump* WAL bytes outside this crate; readers must go through [`decode`]
/// or [`walk`], which validate CRCs and cross-check the declared LSN.
pub fn encode(rec: &WalRecord, lsn: Lsn, epoch: u64) -> Vec<u8> {
    encode_with_flags(rec, lsn, epoch, 0)
}

/// [`encode`] with header flags. The only defined flag is
/// [`FLAG_GROUP_END`] (S16); unknown flag bits assert — the flags surface
/// is a format contract, not scratch space.
pub fn encode_with_flags(rec: &WalRecord, lsn: Lsn, epoch: u64, flags: u8) -> Vec<u8> {
    assert!(
        flags & !FLAG_GROUP_END == 0,
        "unknown header flags {flags:#04x} — only FLAG_GROUP_END is defined in v3"
    );
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
        WalRecord::ClosedTsHeartbeat { closed_ts } => {
            (TYPE_CLOSED_TS_HEARTBEAT, closed_ts.to_le_bytes().to_vec())
        }
        WalRecord::HlcHighWater { max_issued } => {
            (TYPE_HLC_HIGH_WATER, max_issued.to_le_bytes().to_vec())
        }
        WalRecord::BulkLoad => (TYPE_BULK_LOAD, Vec::new()),
        WalRecord::UnloggedMarker => (TYPE_UNLOGGED_MARKER, Vec::new()),
    };
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
    buf.push(flags);
    buf.extend_from_slice(&[0u8; 2]);
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
pub enum DecodeError {
    Corruption,
    UnknownRecordVersion(u32),
    UnknownRecordType(u8),
}

/// One decoded record: the record itself plus the self-describing header
/// fields ([`Decoded::lsn`], [`Decoded::epoch`]) the reader validates, the
/// format version it arrived in, and whether it ends a mini-transaction
/// (S16 — v2 records decode as groups of one).
#[derive(Debug)]
pub struct Decoded {
    pub record: WalRecord,
    pub lsn: u64,
    pub epoch: u64,
    pub format_version: u32,
    pub group_end: bool,
    /// Total encoded length so the caller can step to the next record.
    pub total: usize,
}

/// Decodes the record starting at `buf[0]`. Payload shapes are validated
/// strictly (a CRC-valid record with a payload that disagrees with its type
/// is corruption — only a buggy encoder can produce one). Accepts v2 (the
/// flags byte was reserved zero there; each record is a group of one) and
/// v3; unknown flag bits in v3 refuse as [`DecodeError::Corruption`] —
/// group semantics must never be guessed (S16).
pub fn decode(buf: &[u8]) -> Result<Decoded, DecodeError> {
    if buf.len() < HEADER_LEN + CRC_LEN {
        return Err(DecodeError::Corruption);
    }
    let version = u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION && version != V2_FORMAT_VERSION {
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
    let flags = buf[9];
    if version == FORMAT_VERSION && flags & !FLAG_GROUP_END != 0 {
        return Err(DecodeError::Corruption);
    }
    let group_end = version == FORMAT_VERSION && flags & FLAG_GROUP_END != 0;
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
        format_version: version,
        group_end,
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

/// What stopped a walk. [`WalkError::Truncated`] covers both "shorter than
/// a header" and "declared length runs past the buffer" — framing cannot
/// tell a torn write from a short buffer, and neither is corruption.
#[derive(Debug)]
pub enum WalkError {
    Truncated,
    UnknownVersion(u32),
    /// A v3 flags byte with bits this version does not define.
    UnknownFlags(u8),
    BadCrc,
}

/// One walked frame: the self-describing header fields and the raw payload
/// slice. Nothing here interprets a payload byte — that is [`decode`]'s
/// job, and the log store must never need it (S5). `total` steps to the
/// next record; an extent boundary in the log store is a boundary this
/// walker drew at a group end (S16).
#[derive(Debug)]
pub struct Frame<'a> {
    pub type_byte: u8,
    pub lsn: u64,
    pub epoch: u64,
    pub xid: u64,
    pub format_version: u32,
    pub group_end: bool,
    pub total: usize,
    pub payload: &'a [u8],
}

/// Validates framing and reports the header — no payload interpretation.
/// Accepts the same versions as [`decode`] (v2 bytes walk too; their flags
/// byte was a reserved zero). Checks, in order: length, version, CRC,
/// flags — so a caller holding a `Frame` knows the bytes under `payload`
/// are exactly what a complete, checksummed record declared.
pub fn walk(buf: &[u8]) -> Result<Frame<'_>, WalkError> {
    if buf.len() < HEADER_LEN + CRC_LEN {
        return Err(WalkError::Truncated);
    }
    let payload_len = u32::from_le_bytes(buf[..4].try_into().expect("4 bytes")) as usize;
    let total = HEADER_LEN + payload_len + CRC_LEN;
    if buf.len() < total {
        return Err(WalkError::Truncated);
    }
    let version = u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION && version != V2_FORMAT_VERSION {
        return Err(WalkError::UnknownVersion(version));
    }
    let stored_crc = u32::from_le_bytes(buf[total - CRC_LEN..total].try_into().expect("4 bytes"));
    if crc32fast::hash(&buf[..total - CRC_LEN]) != stored_crc {
        return Err(WalkError::BadCrc);
    }
    let flags = buf[9];
    if version == FORMAT_VERSION && flags & !FLAG_GROUP_END != 0 {
        return Err(WalkError::UnknownFlags(flags));
    }
    Ok(Frame {
        type_byte: buf[8],
        lsn: u64::from_le_bytes(buf[20..28].try_into().expect("8 bytes")),
        epoch: u64::from_le_bytes(buf[28..HEADER_LEN].try_into().expect("8 bytes")),
        xid: u64::from_le_bytes(buf[12..20].try_into().expect("8 bytes")),
        format_version: version,
        group_end: version == FORMAT_VERSION && flags & FLAG_GROUP_END != 0,
        total,
        payload: &buf[HEADER_LEN..HEADER_LEN + payload_len],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_types::{SegmentId, VersionWord, TxnId};

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

    /// The v3 encode output is pinned by `tests/golden.rs` (the one-way
    /// door moved with the crate); here the self-describing fields
    /// roundtrip: whatever `lsn` and `epoch` the encoder was given come
    /// back out of the decoder.
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
    /// byte for byte in the v3 layout: HeapInsert { xid: 5, segment: 7,
    /// cell = a fresh 3-byte-payload cell } at LSN 0x1000, epoch 0. The
    /// cell bytes are cascade-page's `[len][xmin = uncommitted(5)][xmax 0]
    /// [next 0][payload]` verbatim; the CRC is verified algorithmically
    /// over the pinned prefix.
    #[test]
    fn heap_insert_bytes_are_pinned() {
        let cell = cascade_page::encode_cell(VersionWord::uncommitted(TxnId(5)), b"abc")
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
            // payload_len = 4 + 29 = 33; format_version = 3; type = 5; flags 0; reserved
            0x21, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
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
        let cell = cascade_page::encode_cell(VersionWord::uncommitted(TxnId(77)), payload)
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
                assert_eq!(r.xmin, VersionWord::uncommitted(TxnId(77)));
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
                VersionWord::uncommitted(TxnId(100 + i)),
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

    /// The flags byte carries only defined bits, and the two bytes behind
    /// it stay reserved-zero in every encoding — they are the §6.3 retrofit
    /// surface, not scratch space. The group-end bit appears exactly where
    /// the caller put it (S16).
    #[test]
    fn flags_byte_and_reserved_bytes_are_pinned() {
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
                cell: cell.clone(),
            },
            WalRecord::BulkLoad,
        ] {
            let b = encode(&rec, Lsn(0), 0);
            assert_eq!(&b[9..12], &[0u8; 3], "{rec:?}: flags 0, reserved zero");
            let g = encode_with_flags(&rec, Lsn(0), 0, FLAG_GROUP_END);
            assert_eq!(g[9], FLAG_GROUP_END, "{rec:?}: group-end set");
            assert_eq!(&g[10..12], &[0u8; 2], "{rec:?}: reserved still zero");
        }
    }

    /// v2 bytes hand-built (pre-flags layout) decode as groups of one
    /// through the same `decode` — the compat half of the v3 transition.
    /// The full literal lives in `tests/golden.rs`; this pins the decoder's
    /// version dispatch on a second shape.
    #[test]
    fn v2_dispatch_on_a_second_shape() {
        let mut bytes: Vec<u8> = [
            // payload_len = 0; format_version = 2; type = BEGIN; reserved (3)
            0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            // xid = 11
            0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // lsn = 0; epoch = 3
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ]
        .into();
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        let d = decode(&bytes).expect("v2 BEGIN decodes");
        assert_eq!(d.format_version, 2);
        assert!(!d.group_end, "v2: group of one");
        assert_eq!(d.record, WalRecord::BeginTxn { xid: 11 });
    }
}
