//! T1.5: the F3 read-side contract as an executable spec — **no single
//! byte of corruption is ever silently accepted**. The CRC covers
//! `header ++ payload`, so flipping ANY byte of a flushed record must land
//! in a named refusal (`Corruption`, `TornTail`, `LsnMismatch`, or one of
//! the loud version/type refusals) — never in a decoded record and never
//! in a clean end-of-log. The sweep walks every byte position of a real
//! flushed record through the public reader path.

use std::{io::Write, path::PathBuf, sync::atomic::{AtomicU32, Ordering}};

use cascade_types::{DatabaseId, Lsn, SegmentId, VersionWord, Xid};
use cascade_wal::{
    DebugWalReader, WalError, WalHandle, WalReader, WalRecord, WalWriter,
};

const DB: DatabaseId = DatabaseId(5);

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "cascade-wal-contract-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("wal-contract")
        .make()
        .expect("executor builds")
}

/// One committed-looking HeapInsert record with a multi-byte payload —
/// enough bytes that the sweep covers header fields, the segment prefix,
/// the cell, and the CRC.
fn staged_record() -> WalRecord {
    WalRecord::HeapInsert {
        xid: 9,
        segment: SegmentId(3),
        cell: cascade_page::encode_cell(VersionWord::uncommitted(Xid(9)), b"payload bytes")
            .expect("fits"),
    }
}

/// The sweep: every byte position of the flushed record, flipped one at a
/// time, must read as a named refusal. This is the executable form of
/// "CRC on every record" (wal invariant 4) — not one sampled flip, all of
/// them, deterministic and reproducible.
#[test]
fn no_single_byte_corruption_reads_as_a_record() {
    let dir = temp_dir();
    let path = dir.join(format!("db_{}.log", DB.0));
    let file_len = reactor().run(async {
        let h = WalHandle::open(&dir, DB).await.expect("handle opens");
        let w = WalWriter::new();
        let r = w.append_wal(&staged_record());
        let end = r.end();
        let _ = r.fill();
        w.flush_wal(&h, end).await.expect("flush");
        end.0
    });
    let orig = std::fs::read(&path).expect("log reads");
    assert_eq!(orig.len() as u64, file_len, "the record is the whole log");

    reactor().run(async {
        for pos in 0..orig.len() {
            let mut bytes = orig.clone();
            bytes[pos] ^= 0x01;
            {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&path)
                    .expect("log reopens");
                f.write_all(&bytes).expect("rewrite");
                f.sync_all().expect("persist the corruption");
            }
            let reader = DebugWalReader::open(&dir, DB)
                .await
                .unwrap_or_else(|e| panic!("byte {pos}: reader opens: {e:?}"));
            match reader.read_record(Lsn(0)).await {
                Err(
                    WalError::Corruption(_)
                    | WalError::TornTail(_)
                    | WalError::LsnMismatch { .. }
                    | WalError::UnknownRecordType(_)
                    | WalError::UnknownRecordVersion(_),
                ) => {}
                Ok(Some(entry)) => panic!(
                    "byte {pos} flipped and the record still decoded as {:?} — \
                     a corruption the reader accepted",
                    entry.record
                ),
                Ok(None) => panic!("byte {pos} flipped and the log read as empty"),
            }
        }
    });

    // The pristine log still reads — the sweep left it restored.
    std::fs::write(&path, &orig).expect("restore");
    reactor().run(async {
        let reader = DebugWalReader::open(&dir, DB).await.expect("reader opens");
        let entry = reader
            .read_record(Lsn(0))
            .await
            .expect("pristine log decodes")
            .expect("the record is there");
        assert_eq!(entry.record, staged_record(), "byte-exact roundtrip");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
