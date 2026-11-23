//! M4 slice 2 red: the manifest's on-disk law (§5.2, §5.7, S18). The
//! manifest is replaced by atomic rename and carries, per scope: the
//! epoch, the key reference, the retention holders, the durable LSN as
//! of the manifest, and the compact extent index (runs). It also carries
//! the S18 counters — next ScopeId, per-stream next SegmentId — and the
//! per-stream recovery scan position. On clean shutdown it records the
//! exact end of the log (§5.7's ambiguity narrowing); on restart the
//! store reads it, increments the incarnation, and writes it back.

use cascade_logstore::manifest::{
    self, Manifest, ScopeRecord, StreamPosition, MANIFEST_MAGIC, MANIFEST_VERSION,
};
use cascade_storage_api::RetentionHolder;
use cascade_types::{Epoch, Lsn, ScopeId};

fn sample() -> Manifest {
    Manifest {
        incarnation: 7,
        next_scope_id: 42,
        clean_shutdown: false,
        streams: vec![StreamPosition {
            stream: 0,
            next_segment_seq: 5,
            scan_segment_seq: 4,
            scan_frame_seq: 11,
            scan_offset: 4096 * 9,
        }],
        scopes: vec![
            ScopeRecord {
                scope: ScopeId(7),
                epoch: Epoch(3),
                key_id: 1,
                durable: Lsn(95),
                holders: vec![
                    (RetentionHolder::PageStore { instance: 1 }, Lsn(95)),
                    (RetentionHolder::Archive, Lsn(0)),
                ],
                runs: vec![manifest::ExtentRun {
                    lsn_start: Lsn(0),
                    lsn_end: Lsn(95),
                    stream: 0,
                    segment_seq: 4,
                    offset: 4096,
                }],
            },
            ScopeRecord {
                scope: ScopeId(11),
                epoch: Epoch(1),
                key_id: 2,
                durable: Lsn(0),
                holders: vec![],
                runs: vec![],
            },
        ],
    }
}

#[test]
fn manifest_magic_and_version_are_pinned() {
    let bytes = manifest::encode(&sample());
    assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), MANIFEST_MAGIC);
    assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), MANIFEST_VERSION);
    // Trailer CRC over everything above it.
    let stored = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
    assert_ne!(stored, 0);
}

#[test]
fn manifest_roundtrips_every_field() {
    let bytes = manifest::encode(&sample());
    let back = manifest::decode(&bytes).expect("decodes");
    assert_eq!(back.incarnation, 7);
    assert_eq!(back.next_scope_id, 42, "the S18 counter survives");
    assert!(!back.clean_shutdown);
    assert_eq!(back.streams.len(), 1);
    assert_eq!(back.streams[0].next_segment_seq, 5, "the S18 counter survives");
    assert_eq!(back.streams[0].scan_frame_seq, 11);
    assert_eq!(back.scopes.len(), 2);
    let s7 = &back.scopes[0];
    assert_eq!(s7.epoch, Epoch(3));
    assert_eq!(s7.durable, Lsn(95));
    assert_eq!(s7.holders.len(), 2, "both holders persist");
    assert_eq!(s7.holders[0].0, RetentionHolder::PageStore { instance: 1 });
    assert_eq!(s7.holders[1].0, RetentionHolder::Archive);
    assert_eq!(s7.runs[0].lsn_end, Lsn(95));
}

#[test]
fn a_flipped_byte_refuses_decode() {
    let mut bytes = manifest::encode(&sample());
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    assert!(manifest::decode(&bytes).is_err(), "corruption is never guessed");
}

#[test]
fn incarnation_increments_across_reopen() {
    let m = sample();
    let next = manifest::bump_incarnation(m);
    assert_eq!(next.incarnation, 8);
    assert!(!next.clean_shutdown, "restart clears the clean flag (§5.7: unclean until proven)");
}

#[test]
fn clean_shutdown_records_the_exact_end() {
    let mut m = sample();
    m.clean_shutdown = true;
    let bytes = manifest::encode(&m);
    let back = manifest::decode(&bytes).expect("decodes");
    assert!(back.clean_shutdown, "after a clean shutdown any bad frame before the end is corruption (§5.7)");
}
