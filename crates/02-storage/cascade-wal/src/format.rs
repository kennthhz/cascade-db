//! Record encoding/decoding — the only code in the workspace that knows the
//! on-disk WAL byte layout (v1, hand-rolled).
//!
//! Layout: `[u32 LE payload_len][u32 LE version = 1][u8 type][3× u8
//! reserved][u64 LE xid][payload][u32 LE CRC32 of all preceding bytes]`.
//! The reserved bytes and the 32-bit version field are the retrofit
//! surface §6.3 reserves: new record *types* extend `type`; an incompatible
//! *format* change bumps `version` so old decoders fail loudly instead of
//! misreading.

use cascade_types::SegmentId;

use crate::WalRecord;

/// Every record starts with this header and ends with a CRC over
/// `header ++ payload`.
pub(crate) const HEADER_LEN: usize = 20;
pub(crate) const CRC_LEN: usize = 4;
pub(crate) const FORMAT_VERSION: u32 = 1;

pub(crate) const TYPE_BEGIN: u8 = 1;
pub(crate) const TYPE_COMMIT: u8 = 2;
pub(crate) const TYPE_ABORT: u8 = 3;
pub(crate) const TYPE_INSERT: u8 = 4;

/// Encode one record to the v1 wire/byte format. Public for tools that
/// *write or dump* WAL bytes outside this crate (benchmarks, `pg_waldump`
/// equivalents); readers must go through [`crate::WalReader`], which is
/// the only path that validates CRCs.
pub fn encode(rec: &WalRecord) -> Vec<u8> {
    let (type_byte, payload): (u8, Vec<u8>) = match rec {
        WalRecord::BeginTxn { .. } => (TYPE_BEGIN, Vec::new()),
        WalRecord::CommitTxn { .. } => (TYPE_COMMIT, Vec::new()),
        WalRecord::AbortTxn { .. } => (TYPE_ABORT, Vec::new()),
        WalRecord::Insert { table, id, val, .. } => {
            // Full row image (I8): table segment + every column. The v1
            // payload carries the value length as u16 — refuse oversized
            // values loudly instead of letting the cast truncate them into
            // a record that cannot decode.
            assert!(
                val.len() <= u16::MAX as usize,
                "row value of {} bytes exceeds the v1 WAL payload limit ({}); \
                 a future format version lifts this",
                val.len(),
                u16::MAX
            );
            let mut p = Vec::with_capacity(4 + 4 + 2 + val.len());
            p.extend_from_slice(&table.0.to_le_bytes());
            p.extend_from_slice(&id.to_le_bytes());
            p.extend_from_slice(&(val.len() as u16).to_le_bytes());
            p.extend_from_slice(val.as_bytes());
            (TYPE_INSERT, p)
        }
    };
    let xid = match rec {
        WalRecord::BeginTxn { xid }
        | WalRecord::CommitTxn { xid }
        | WalRecord::AbortTxn { xid }
        | WalRecord::Insert { xid, .. } => xid,
    };

    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len() + CRC_LEN);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.push(type_byte);
    buf.extend_from_slice(&[0u8; 3]);
    buf.extend_from_slice(&xid.to_le_bytes());
    buf.extend_from_slice(&payload);
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

/// What stopped a decode: bad bytes (`Corruption`) or a version we cannot
/// read (`UnknownRecordVersion`).
#[derive(Debug)]
pub(crate) enum DecodeError {
    Corruption,
    UnknownRecordVersion(u32),
}

/// Decodes the record starting at `buf[0]`. Returns the record and its
/// total encoded length so the caller can step to the next one.
pub(crate) fn decode(buf: &[u8]) -> Result<(WalRecord, usize), DecodeError> {
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
    let xid = u64::from_le_bytes(buf[12..HEADER_LEN].try_into().expect("8 bytes"));
    let payload = &buf[HEADER_LEN..HEADER_LEN + payload_len];
    let rec = match type_byte {
        TYPE_BEGIN => WalRecord::BeginTxn { xid },
        TYPE_COMMIT => WalRecord::CommitTxn { xid },
        TYPE_ABORT => WalRecord::AbortTxn { xid },
        TYPE_INSERT => {
            let table = SegmentId(u32::from_le_bytes(
                payload[..4].try_into().expect("4 bytes"),
            ));
            let id = i32::from_le_bytes(payload[4..8].try_into().expect("4 bytes"));
            let val_len = u16::from_le_bytes(payload[8..10].try_into().expect("2 bytes")) as usize;
            let val = String::from_utf8_lossy(&payload[10..10 + val_len]).into_owned();
            WalRecord::Insert {
                xid,
                table,
                id,
                val,
            }
        }
        _ => return Err(DecodeError::Corruption),
    };
    Ok((rec, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_types::SegmentId;

    #[test]
    fn insert_roundtrips_with_full_row_image() {
        let rec = WalRecord::Insert {
            xid: 42,
            table: SegmentId(7),
            id: -3,
            val: "hello wal".to_owned(),
        };
        let bytes = encode(&rec);
        let (decoded, total) = decode(&bytes).expect("decodes");
        assert_eq!(total, bytes.len());
        match decoded {
            WalRecord::Insert {
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
            WalRecord::CommitTxn { xid: 2 },
            WalRecord::AbortTxn { xid: 3 },
        ] {
            let bytes = encode(&rec);
            let (decoded, total) = decode(&bytes).expect("decodes");
            assert_eq!(total, bytes.len());
            assert_eq!(format!("{decoded:?}"), format!("{rec:?}"));
        }
    }

    #[test]
    fn flipped_byte_is_corruption() {
        let mut bytes = encode(&WalRecord::CommitTxn { xid: 9 });
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        assert!(matches!(decode(&bytes), Err(DecodeError::Corruption)));
    }

    #[test]
    fn unknown_version_is_reported() {
        let mut bytes = encode(&WalRecord::BeginTxn { xid: 1 });
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(DecodeError::UnknownRecordVersion(99))
        ));
    }
}
