//! The conformance suites (§4.10): one suite per contract, written once as
//! generic async functions over a harness that can create, crash and
//! restart an instance. The suites cover every numbered promise of §4.3
//! through §4.5 and §4.7–§4.8; they run against the fakes in this crate,
//! against the local implementations (M3/M4), and later against the remote
//! clients over the simulated network — a promise is only kept if every
//! implementation passes the same suite.

use std::sync::Arc;

use cascade_types::{Epoch, Hlc, Lsn, ScopeId, SegmentId};
use cascade_wal_format::{encode, encode_with_flags, FLAG_GROUP_END};

use crate::block_device::BlockDevice;
use crate::error::StoreError;
use crate::fake::test_ctx;
use crate::key_provider::{KeyProvider, KeyRef, KeyVersion};
use crate::log_sink::LogSink;
use crate::log_source::LogSource;
use crate::page_source::PageSource;
use crate::scope_admin::{LogScopes, PageScopes};
use crate::types::{AppendMeta, FileName, RetentionHolder};


/// The scope every suite runs in.
pub const SCOPE: ScopeId = ScopeId(7);
const E1: Epoch = Epoch(1);

/// One mini-transaction at `offset`: a HEAP_INSERT for segment 1 carrying
/// `body`, then a COMMIT with the group-end flag (S16 — every append ends
/// on a group end). Frames carry `epoch` in their headers.
fn group_at(offset: u64, epoch: u64, xid: u64, ts: u64, body: &[u8]) -> Vec<u8> {
    let ins = encode(
        &cascade_wal_format::WalRecord::HeapInsert {
            xid,
            segment: SegmentId(1),
            cell: body.to_vec(),
        },
        Lsn(offset),
        epoch,
    );
    let com = encode_with_flags(
        &cascade_wal_format::WalRecord::CommitTxn {
            xid,
            commit_ts: Hlc(ts),
        },
        Lsn(offset + ins.len() as u64),
        epoch,
        FLAG_GROUP_END,
    );
    let mut b = ins;
    b.extend(com);
    b
}

fn meta(ts: u64) -> AppendMeta {
    AppendMeta {
        max_commit_ts: Some(Hlc(ts)),
        writer_durable: Lsn(0),
    }
}

async fn append_group<S: LogSink + LogSource + LogScopes>(
    store: &S,
    span: &cascade_telemetry::Span,
    start: u64,
    epoch: Epoch,
    xid: u64,
    ts: u64,
    body: &[u8],
) -> Result<Lsn, StoreError> {
    let ctx = test_ctx(span, 1);
    let bytes = group_at(start, epoch.0, xid, ts, body);
    (store.append(&ctx, SCOPE, epoch, Lsn(start), &bytes, meta(ts))).await?;
    (store.wait_durable(&ctx, SCOPE, Lsn(start + bytes.len() as u64))).await
}

/// The log store harness (§4.10): create, crash, restart, inject.
pub trait LogStoreHarness {
    type Store: LogSink + LogSource + LogScopes;
    fn create(&mut self) -> Arc<Self::Store>;
    fn crash(&mut self, store: &Arc<Self::Store>);
    fn restart(&mut self, store: &Arc<Self::Store>) -> Arc<Self::Store>;
    fn inject_flush_failure(&mut self, store: &Arc<Self::Store>);
}

/// The in-memory harness used by this crate's own tests (and any
/// implementation's smoke check).
pub struct MemLogStoreHarness;

impl LogStoreHarness for MemLogStoreHarness {
    type Store = crate::fake::MemLogStore;
    fn create(&mut self) -> Arc<Self::Store> {
        Arc::new(crate::fake::MemLogStore::new())
    }
    fn crash(&mut self, store: &Arc<Self::Store>) {
        store.crash();
    }
    fn restart(&mut self, store: &Arc<Self::Store>) -> Arc<Self::Store> {
        Arc::new(store.restart())
    }
    fn inject_flush_failure(&mut self, store: &Arc<Self::Store>) {
        store.inject_flush_failure();
    }
}

/// §4.3, promises 1–7, against any log store.
pub async fn log_sink_suite<H: LogStoreHarness>(h: &mut H) {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let store = h.create();

    (store.create_scope(&ctx, SCOPE, E1, KeyRef { key_id: 1 })).await
        .expect("scope creates");

    // Promise 1 — fencing, retry, discard of unflushed older-epoch bytes.
    let d = (store.open_writer(&ctx, SCOPE, E1)).await.expect("first open");
    assert_eq!(d, Lsn(0), "a fresh scope's durable LSN starts at 0");
    let again = (store.open_writer(&ctx, SCOPE, E1)).await.expect("same epoch is a retry");
    assert_eq!(again, d, "a retried open returns the durable LSN again");

    // Accepted but never flushed: the fence discards it (§4.3 promise 1 —
    // only *acknowledged* bytes survive an epoch change).
    let raw = group_at(0, 1, 1, 10, b"one");
    (store.append(&ctx, SCOPE, E1, Lsn(0), &raw, meta(10))).await
        .expect("accepted, not flushed");
    // A newer epoch fences the old one and discards its unflushed bytes.
    let d2 = (store.open_writer(&ctx, SCOPE, Epoch(2))).await.expect("upgrade epoch");
    assert_eq!(d2, Lsn(0), "epoch 1's bytes were never acknowledged: discarded");
    let fenced = (store.append(
        &ctx,
        SCOPE,
        E1,
        Lsn(0),
        &group_at(0, 1, 1, 10, b"one"),
        meta(10),
    )).await;
    assert!(
        matches!(fenced, Err(StoreError::Fenced { current }) if current == Epoch(2)),
        "a fenced epoch fails Fenced — even an identical re-send"
    );
    assert!(matches!(
        (store.open_writer(&ctx, SCOPE, E1)).await,
        Err(StoreError::Fenced { current }) if current == Epoch(2)
    ));

    // Promise 2 — contiguity.
    let e2 = Epoch(2);
    let end2 = append_group(&*store, &span, 0, e2, 2, 20, b"two").await.expect("append at 0");
    assert_eq!(end2, end_of(1, b"two"));
    let gap = (store.append(
        &ctx,
        SCOPE,
        e2,
        Lsn(end2.0 + 8),
        &group_at(end2.0 + 8, 2, 3, 30, b"three"),
        meta(30),
    )).await;
    assert!(matches!(gap, Err(StoreError::Gap { expected }) if expected == end2));

    // Promise 3 — idempotence: identical overlap succeeds, has no effect;
    // differing overlap conflicts at the first differing byte.
    let bytes = group_at(0, 2, 2, 20, b"two");
    (store.append(&ctx, SCOPE, e2, Lsn(0), &bytes, meta(20))).await
        .expect("identical re-send succeeds");
    assert_eq!(
        store.durable_lsn(SCOPE).expect("durable"),
        end2,
        "the re-send had no further effect"
    );
    // A differing overlap conflicts — at a *payload* byte, with record 1's
    // CRC restamped, so framing passes and the overlap check is what
    // refuses (a framing-invalid re-send fails BadFrame first, promise 4).
    let mut lie = bytes.clone();
    let at = 40usize; // record 1's payload = [segment u32][cell "two"]: byte 40 is 't'
    lie[at] ^= 0xFF;
    let r1_crc_at = 43usize; // record 1: header 36 + payload (4 seg + 3 cell) = CRC at 43
    let r1_crc = crc32(&lie[..r1_crc_at]);
    lie[r1_crc_at..r1_crc_at + 4].copy_from_slice(&r1_crc.to_le_bytes());
    let conflict = (store.append(&ctx, SCOPE, e2, Lsn(0), &lie, meta(20))).await;
    assert!(matches!(conflict, Err(StoreError::Conflict { .. })), "conflict case returned {conflict:?}");

    // Promise 4 — framing. (Payload-blindness: arbitrary cell bytes pass.)
    // a. empty bytes
    assert!(matches!(
        (store.append(&ctx, SCOPE, e2, Lsn(end2.0), &[], meta(0))).await,
        Err(StoreError::BadFrame { reason: crate::FrameError::BadLength, .. })
    ));
    // b. last record without the group-end flag (S16)
    let interior = cascade_wal_format::encode(
        &cascade_wal_format::WalRecord::CommitTxn { xid: 4, commit_ts: Hlc(1) },
        Lsn(end2.0),
        2,
    );
    assert!(matches!(
        (store.append(&ctx, SCOPE, e2, Lsn(end2.0), &interior, meta(0))).await,
        Err(StoreError::BadFrame { reason: crate::FrameError::NotGroupEnd, .. })
    ));
    // c. corrupted CRC — flipped inside record 2's commit_ts payload (a
    // flip in a length field would refuse as BadLength, its own named law)
    let mut bad = group_at(end2.0, 2, 5, 50, b"five");
    let payload_byte = bad.len() - 8 - 4; // commit_ts's first byte, before the CRC
    bad[payload_byte] ^= 0xFF;
    assert!(matches!(
        (store.append(&ctx, SCOPE, e2, Lsn(end2.0), &bad, meta(50))).await,
        Err(StoreError::BadFrame { reason: crate::FrameError::BadCrc, .. })
    ));
    // d. header LSN not equal to position (record 1's CRC restamped: the
    // header lie is the only flaw left)
    let mut lsn_lie = group_at(end2.0, 2, 6, 60, b"six");
    lsn_lie[20..28].copy_from_slice(&99u64.to_le_bytes());
    // Record 1's CRC sits at (its encoded length − 4): 36 + (4 + cell).
    let r1_end = 36 + 4 + 3;
    let r1_crc = crc32(&lsn_lie[..r1_end]);
    lsn_lie[r1_end..r1_end + 4].copy_from_slice(&r1_crc.to_le_bytes());
    assert!(matches!(
        (store.append(&ctx, SCOPE, e2, Lsn(end2.0), &lsn_lie, meta(60))).await,
        Err(StoreError::BadFrame { reason: crate::FrameError::BadLsn, .. })
    ));
    // e. header epoch not equal to the append's epoch
    let mut epoch_lie = group_at(end2.0, 9, 7, 70, b"seven");
    let r1_end = 36 + 4 + 5;
    let r1_crc = crc32(&epoch_lie[..r1_end]);
    epoch_lie[r1_end..r1_end + 4].copy_from_slice(&r1_crc.to_le_bytes());
    assert!(matches!(
        (store.append(&ctx, SCOPE, e2, Lsn(end2.0), &epoch_lie, meta(70))).await,
        Err(StoreError::BadFrame { reason: crate::FrameError::BadEpoch, .. })
    ));

    // Promise 5 — durability is monotonic and lands on a group end.
    let before = store.durable_lsn(SCOPE).expect("durable");
    let d3 = append_group(&*store, &span, end2.0, e2, 8, 80, b"eight").await.expect("append");
    assert!(d3 >= before, "durable never moves backwards");
    let last_end = walk_last_group_end(&*store, d3).await;
    assert_eq!(d3, last_end, "the durable LSN falls on a group end (S16)");

    // Promise 6 — acknowledged means durable: a crash loses nothing below
    // the durable LSN.
    let acked = store.durable_lsn(SCOPE).expect("durable");
    let acknowledged = read_all(&*store, acked).await;
    h.crash(&store);
    let revived = h.restart(&store);
    let after = read_all(&*revived, acked).await;
    assert_eq!(
        acknowledged, after,
        "every acknowledged record survives a crash"
    );

    // Promise 7 — a flush failure fails the epoch until reopen (S19's
    // blast radius at store scale; §8.1 step 1).
    h.inject_flush_failure(&revived);
    let failed = (revived.wait_durable(&ctx, SCOPE, Lsn(1 << 40))).await;
    assert!(matches!(failed, Err(StoreError::Failed { .. })));
    assert!(matches!(
        (revived.append(&ctx, SCOPE, e2, Lsn(d3.0), &group_at(d3.0, 2, 9, 90, b"nine"), meta(90))).await,
        Err(StoreError::Failed { .. })
    ));
    let reopened = (revived.open_writer(&ctx, SCOPE, Epoch(3))).await.expect("reopen recovers");
    assert_eq!(reopened, d3, "reopen resumes at the durable LSN");
    append_group(&*revived, &span, d3.0, Epoch(3), 10, 100, b"ten")
        .await
        .expect("appends work again after reopen");
}

/// §4.4 promises 1–6 (the caller-side halves — persist-before-report —
/// are the holders' obligations, pinned in M4's integration).
pub async fn log_source_suite<H: LogStoreHarness>(h: &mut H) {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let store = h.create();
    (store.create_scope(&ctx, SCOPE, E1, KeyRef { key_id: 1 })).await.expect("creates");
    (store.open_writer(&ctx, SCOPE, E1)).await.expect("opens");

    // Promise 1 — only durable records: an accepted-but-unflushed append
    // is never served.
    let g1 = group_at(0, 1, 1, 10, b"alpha");
    (store.append(&ctx, SCOPE, E1, Lsn(0), &g1, meta(10))).await.expect("accepted");
    let early = (store.read_log(&ctx, SCOPE, Lsn(0), 1 << 20)).await.expect("reads");
    assert!(
        early.bytes.is_empty(),
        "nothing above the durable LSN is ever returned"
    );
    (store.wait_durable(&ctx, SCOPE, Lsn(g1.len() as u64))).await.expect("flushes");

    // Promises 2 + subscription contiguity — chunks chain with no gaps or
    // duplicates, and an empty chunk marks the durable point.
    let e1_end = append_group(&*store, &span, g1.len() as u64, E1, 2, 20, b"beta").await
        .expect("append 2");
    let mut pos = Lsn(0);
    let mut seen = Vec::new();
    loop {
        let chunk = (store.read_log(&ctx, SCOPE, pos, 1)).await.expect("chunk");
        assert_eq!(chunk.start, pos, "chunks start where the last ended");
        assert_eq!(chunk.end.0, chunk.start.0 + chunk.bytes.len() as u64);
        assert_eq!(chunk.durable, e1_end, "chunk.durable is the scope's durable LSN");
        seen.extend_from_slice(&chunk.bytes);
        pos = chunk.end;
        if pos >= e1_end {
            break;
        }
    }
    assert_eq!(seen.len(), e1_end.0 as usize, "the whole log, no gaps, no duplicates");
    let tail = (store.read_log(&ctx, SCOPE, e1_end, 1 << 20)).await.expect("tail");
    assert!(tail.bytes.is_empty(), "from == durable is an empty chunk");

    let mut sub = store.subscribe(&ctx, SCOPE, Lsn(0));
    let mut streamed = Vec::new();
    while streamed.len() < e1_end.0 as usize {
        let chunk = (sub.next()).await.expect("sub chunk");
        assert_eq!(chunk.start.0, streamed.len() as u64, "subscription is contiguous");
        streamed.extend_from_slice(&chunk.bytes);
    }
    assert_eq!(streamed, seen, "the subscription sees the same durable log");

    // Promise 3 — record boundaries.
    let inside = (store.read_log(&ctx, SCOPE, Lsn(3), 1 << 20)).await;
    assert!(matches!(inside, Err(StoreError::NotRecordBoundary { .. })));

    // Promise 4 — retention: the floor moves only forward, reads below it
    // truncate, and with no holder nothing is discardable.
    assert_eq!(store.retained_floor(SCOPE).expect("floor"), Lsn(0));
    (store.set_retained_from(&ctx, SCOPE, RetentionHolder::Archive, Lsn(0))).await
        .expect("archive registers");
    (store.set_retained_from(&ctx, SCOPE, RetentionHolder::Archive, e1_end)).await
        .expect("archive advances");
    assert_eq!(store.retained_floor(SCOPE).expect("floor"), e1_end);
    let below = (store.read_log(&ctx, SCOPE, Lsn(0), 1 << 20)).await;
    assert!(matches!(below, Err(StoreError::Truncated { floor }) if floor == e1_end));
    // A new holder must register at or above the current floor; a lower
    // value clamps — the floor never regresses.
    (store.set_retained_from(
        &ctx,
        SCOPE,
        RetentionHolder::PageStore { instance: 1 },
        Lsn(0),
    ))
    .await
    .expect("clamped registration");
    assert_eq!(store.retained_floor(SCOPE).expect("floor"), e1_end);
    (store.release_holder(&ctx, SCOPE, RetentionHolder::Archive)).await.expect("release");
    assert_eq!(
        store.retained_floor(SCOPE).expect("floor"),
        e1_end,
        "the surviving holder keeps the floor"
    );
    (store.release_holder(&ctx, SCOPE, RetentionHolder::PageStore { instance: 1 })).await
        .expect("release");
    assert_eq!(store.retained_floor(SCOPE).expect("floor"), Lsn(0));

    // Point-in-time lookup: the earliest append whose highest commit
    // timestamp is above `ts`; below the floor it truncates.
    let boundary = (store.append_before_time(&ctx, SCOPE, Hlc(15))).await.expect("lookup");
    assert_eq!(boundary, Lsn(g1.len() as u64), "beta (ts 20) is the first append above 15");
    let boundary = (store.append_before_time(&ctx, SCOPE, Hlc(5))).await.expect("lookup");
    assert_eq!(boundary, Lsn(0), "alpha (ts 10) is the first append above 5");
    let boundary = (store.append_before_time(&ctx, SCOPE, Hlc(25))).await.expect("lookup");
    assert_eq!(
        boundary, e1_end,
        "no append is above 25: the log end is the boundary"
    );
    let boundary = (store.append_before_time(&ctx, SCOPE, Hlc(u64::MAX >> 2))).await
        .expect("lookup");
    assert_eq!(boundary, e1_end, "no append is above: the log end is the boundary");
    (store.set_retained_from(&ctx, SCOPE, RetentionHolder::Archive, e1_end)).await
        .expect("raise floor");
    let gone = (store.append_before_time(&ctx, SCOPE, Hlc(15))).await;
    assert!(matches!(gone, Err(StoreError::Truncated { .. })));
}

fn end_of(xid: u64, body: &[u8]) -> Lsn {
    let g = group_at(0, 2, xid, 0, body);
    Lsn(g.len() as u64)
}

/// A local CRC32 (IEEE) — the same polynomial crc32fast uses, kept local so
/// the contract crate carries no direct checksum dependency; the golden
/// tests of cascade-wal-format pin the real encoder's CRC against this
/// algorithm's behavior.
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

async fn walk_last_group_end<S: LogSource>(store: &S, _durable: Lsn) -> Lsn {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let chunk = (store.read_log(&ctx, SCOPE, Lsn(0), usize::MAX)).await.expect("read all");
    let mut at = 0usize;
    let mut last_end = Lsn(0);
    let mut last_group = false;
    while at < chunk.bytes.len() {
        let f = cascade_wal_format::walk(&chunk.bytes[at..]).expect("served records frame");
        at += f.total;
        last_end = Lsn(last_end.0 + f.total as u64);
        last_group = f.group_end || f.format_version == 2;
    }
    assert!(last_group, "the last record below durable ends a group");
    last_end
}

async fn read_all<S: LogSource>(store: &S, durable: Lsn) -> Vec<u8> {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let mut pos = Lsn(0);
    let mut out = Vec::new();
    while pos < durable {
        let chunk = (store.read_log(&ctx, SCOPE, pos, 64)).await.expect("chunk");
        assert_eq!(chunk.start, pos);
        if chunk.bytes.is_empty() {
            break;
        }
        pos = chunk.end;
        out.extend_from_slice(&chunk.bytes);
    }
    out
}

/// The block device harness (§4.8): a device plus its crash and sync-
/// failure injectors — the fake models them (§4.10); a real device's
/// harness cannot inject, so it opts out honestly and the S19 section
/// skips (the impl's poisoning-on-genuine-failure is its own duty).
pub trait BlockDeviceHarness {
    type Dev: BlockDevice;
    fn create(&mut self) -> Arc<Self::Dev>;
    fn crash(&mut self, dev: &Arc<Self::Dev>, seed: u64);
    fn inject_sync_failure(&mut self, dev: &Arc<Self::Dev>);
    /// Whether `inject_sync_failure` actually arms a failure. Default:
    /// no — a real device's harness reports false and skips S19.
    fn supports_sync_injection(&self) -> bool {
        false
    }
}

pub struct MemBlockDeviceHarness;

impl BlockDeviceHarness for MemBlockDeviceHarness {
    type Dev = crate::fake::MemBlockDevice;
    fn create(&mut self) -> Arc<Self::Dev> {
        Arc::new(crate::fake::MemBlockDevice::new())
    }
    fn crash(&mut self, dev: &Arc<Self::Dev>, seed: u64) {
        dev.crash(seed);
    }
    fn inject_sync_failure(&mut self, dev: &Arc<Self::Dev>) {
        dev.inject_sync_failure();
    }
}

/// §4.8 promises, against any block device.
pub async fn block_device_suite<H: BlockDeviceHarness>(h: &mut H) {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let dev = h.create();
    let handle = dev.local();

    let name = FileName("seg-0-0".to_string());
    let f = (handle.open(&ctx, &name, true)).await.expect("creates");
    let mut a = cascade_types::AlignedBuf::zeroed(4096);
    a.as_mut_slice().fill(0xAA);
    (handle.write_at(&ctx, f, 0, a)).await.expect("writes");

    // Alignment is a rule on callers; the device enforces it.
    let junk = cascade_types::AlignedBuf::zeroed(4096);
    assert!(
        (handle.write_at(&ctx, f, 1, junk)).await.is_err(),
        "offsets are multiples of 4 KiB"
    );

    // Unsynced writes are readable on the same device instance.
    let back = (handle.read_at(&ctx, f, 0, cascade_types::AlignedBuf::zeroed(4096))).await
        .expect("reads back");
    assert!(back.as_slice().iter().all(|&b| b == 0xAA));

    // A synced write survives any crash; an unsynced block at a cut point
    // is either fully there or fully gone — never torn.
    (handle.sync(&ctx, f)).await.expect("syncs");
    let mut b = cascade_types::AlignedBuf::zeroed(4096);
    b.as_mut_slice().fill(0xBB);
    (handle.write_at(&ctx, f, 4096, b)).await.expect("writes unsynced");
    h.crash(&dev, 0xC0FFEE);
    let back = (handle.read_at(&ctx, f, 0, cascade_types::AlignedBuf::zeroed(4096))).await
        .expect("synced block survives");
    assert!(back.as_slice().iter().all(|&b| b == 0xAA), "synced survives");
    let cut = (handle.read_at(&ctx, f, 4096, cascade_types::AlignedBuf::zeroed(4096))).await
        .expect("reads");
    let all_bb = cut.as_slice().iter().all(|&b| b == 0xBB);
    let all_zero = cut.as_slice().iter().all(|&b| b == 0);
    assert!(
        all_bb || all_zero,
        "an unsynced block survives whole or not at all — never torn"
    );

    // S19 — a failed sync is permanent: the file refuses everything after.
    // Only harnesses that can genuinely inject run this section.
    if !h.supports_sync_injection() {
        return;
    }
    h.inject_sync_failure(&dev);
    let mut c = cascade_types::AlignedBuf::zeroed(4096);
    c.as_mut_slice().fill(0xCC);
    (handle.write_at(&ctx, f, 8192, c)).await.expect("writes before the doomed sync");
    assert!((handle.sync(&ctx, f)).await.is_err(), "the injected sync fails");
    assert!(
        (handle.sync(&ctx, f)).await.is_err(),
        "retrying the sync proves nothing — it keeps failing"
    );
    assert!(
        (handle.read_at(&ctx, f, 0, cascade_types::AlignedBuf::zeroed(4096))).await.is_err(),
        "every later operation on the failed file reports failure"
    );

    // Names, not paths: rename is atomic and lands in the namespace.
    let tmp = FileName("MANIFEST.tmp".to_string());
    let dst = FileName("MANIFEST".to_string());
    let m = (handle.open(&ctx, &tmp, true)).await.expect("creates manifest");
    let mut mbuf = cascade_types::AlignedBuf::zeroed(4096);
    mbuf.as_mut_slice()[0] = 0x5A;
    (handle.write_at(&ctx, m, 0, mbuf)).await.expect("writes");
    (handle.sync(&ctx, m)).await.expect("syncs");
    (handle.rename(&ctx, &tmp, &dst)).await.expect("renames atomically");
    (handle.sync_namespace()).await.expect("namespace durable");
    let names = (handle.list()).await.expect("lists");
    assert!(names.contains(&dst) && !names.contains(&tmp));
    assert!(
        (handle.remove(&ctx, &tmp)).await.is_err(),
        "the old name is gone"
    );
    (handle.remove(&ctx, &dst)).await.expect("remove");
    assert!(!(handle.list()).await.expect("lists").contains(&dst));
}

/// The key provider harness: create plus a rotation hook — key management
/// is the implementation's control plane, so the suite drives versions
/// through it (§7).
pub trait KeyProviderHarness {
    type P: KeyProvider;
    fn create(&mut self) -> Arc<Self::P>;
    fn rotate(&mut self, p: &Arc<Self::P>, key: KeyRef) -> KeyVersion;
}

pub struct MemKeyProviderHarness;

impl KeyProviderHarness for MemKeyProviderHarness {
    type P = crate::fake::MemKeyProvider;
    fn create(&mut self) -> Arc<Self::P> {
        Arc::new(crate::fake::MemKeyProvider::new())
    }
    fn rotate(&mut self, p: &Arc<Self::P>, key: KeyRef) -> KeyVersion {
        p.rotate(key, vec![0x42u8; 32])
    }
}

/// §4.7 promises: versions, rotation readability, material hygiene.
pub async fn key_provider_suite<H: KeyProviderHarness>(h: &mut H) {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let p = h.create();
    let key = KeyRef { key_id: 9 };

    assert!(matches!(
        (p.current(&ctx, key)).await,
        Err(crate::error::KeyError::UnknownKey)
    ));
    let v1 = h.rotate(&p, key);
    let (cur, mat1) = (p.current(&ctx, key)).await.expect("current after rotate");
    assert_eq!(cur, v1);
    assert_eq!(mat1.material().len(), 32);
    let v2 = h.rotate(&p, key);
    let (cur, _) = (p.current(&ctx, key)).await.expect("current after second rotate");
    assert_eq!(cur, v2, "new writes seal with the current version");
    let old = (p.get(&ctx, key, v1)).await.expect("old version stays readable");
    assert_eq!(old.material(), mat1.material(), "old data keeps its material");
    assert!(matches!(
        (p.get(&ctx, key, KeyVersion(999))).await,
        Err(crate::error::KeyError::UnknownVersion)
    ));

    // Key material never appears in debug output (§4.7).
    let dbg = format!("{mat1:?}");
    assert!(!dbg.contains("0x42"), "Debug redacts key material: {dbg}");
    drop(mat1);
    drop(old);
}

/// The page store harness: a log sink/source pair and a page store over
/// it (§4.10 — MemPageStore takes any LogSource and injected redo).
pub trait PageStoreHarness {
    type Log: LogSink + LogSource + LogScopes;
    type Store: PageSource + PageScopes;
    fn create(&mut self) -> (Arc<Self::Log>, Arc<Self::Store>);
}

/// Toy redo semantics for the fake page store (§4.10: the contract crate
/// never depends on cascade-redo — real tests pass the real apply):
/// a HEAP_INSERT cell is `[page_no u32][content..]` for its record's
/// segment; apply writes the content at page offset 0 and stamps the
/// page LSN — idempotent by LSN (§12.4).
pub fn toy_route(rec: &cascade_wal_format::WalRecord) -> Vec<crate::types::PageKey> {
    use cascade_wal_format::WalRecord;
    match rec {
        WalRecord::HeapInsert { segment, cell, .. } if cell.len() >= 4 => {
            let page_no = u32::from_le_bytes(cell[..4].try_into().expect("4 bytes"));
            vec![crate::types::PageKey {
                segment: *segment,
                page_no,
            }]
        }
        _ => Vec::new(),
    }
}

pub fn toy_apply(
    page: &mut [u8],
    page_lsn: &mut Lsn,
    rec: &cascade_wal_format::WalRecord,
    lsn: Lsn,
) {
    use cascade_wal_format::WalRecord;
    if *page_lsn > lsn {
        return; // redo is idempotent by LSN (§12.4, S17)
    }
    if let WalRecord::HeapInsert { cell, .. } = rec {
        if cell.len() >= 4 {
            let content = &cell[4..];
            let n = content.len().min(page.len());
            page[..n].copy_from_slice(&content[..n]);
            *page_lsn = Lsn(lsn.0 + cascade_wal_format::encoded_len(rec) as u64);
        }
    }
}

pub struct MemPageStoreHarness;

impl PageStoreHarness for MemPageStoreHarness {
    type Log = crate::fake::MemLogStore;
    type Store = crate::fake::MemPageStore<crate::fake::MemLogStore>;

    fn create(&mut self) -> (Arc<Self::Log>, Arc<Self::Store>) {
        let log = Arc::new(crate::fake::MemLogStore::new());
        let route: crate::fake::PageRouter = Arc::new(toy_route);
        let apply: crate::fake::PageApply = Arc::new(toy_apply);
        let store = crate::fake::MemPageStore::new(log.clone(), route, apply);
        (log, Arc::new(store))
    }
}

/// §4.5 promises, against any page store over its log.
pub async fn page_source_suite<H: PageStoreHarness>(h: &mut H) {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let (log, store) = h.create();
    (store.attach(&ctx, SCOPE, None, KeyRef { key_id: 1 })).await.expect("attaches");
    (log.create_scope(&ctx, SCOPE, E1, KeyRef { key_id: 1 })).await.expect("log scope");
    (log.open_writer(&ctx, SCOPE, E1)).await.expect("opens");

    fn page_body(page_no: u32, content: &[u8]) -> Vec<u8> {
        let mut cell = page_no.to_le_bytes().to_vec();
        cell.extend_from_slice(content);
        cell
    }
    let page5 = crate::types::PageKey { segment: SegmentId(1), page_no: 5 };

    // Promise 4 — an unwritten page is zeros with page LSN 0.
    let empty = (store.get_page(
        &ctx,
        SCOPE,
        crate::types::PageKey { segment: SegmentId(1), page_no: 6 },
        Lsn(0),
    )).await
    .expect("unwritten page");
    assert!(empty.buf.as_slice().iter().all(|&b| b == 0));
    assert_eq!(empty.page_lsn, Lsn(0));

    // Promise 1 — exact as of an LSN, across time; promise 3 — catch-up
    // on demand: the second get_page applies records it never ingested.
    let end1 = append_page_group(&*log, &span, 0, 1, 10, &page_body(5, b"one")).await;
    let img = (store.get_page(&ctx, SCOPE, page5, end1)).await.expect("page as of end1");
    assert_eq!(&img.buf.as_slice()[..3], b"one");
    // S17: the page LSN is the END of the last record *applied to it* —
    // the heap-insert record, not the commit that closes its group.
    let ins_len = cascade_wal_format::encoded_len(&cascade_wal_format::WalRecord::HeapInsert {
        xid: 1,
        segment: SegmentId(1),
        cell: page_body(5, b"one"),
    }) as u64;
    assert_eq!(img.page_lsn, Lsn(ins_len), "page LSN = the heap-insert record's end");
    let end2 = append_page_group(&*log, &span, end1.0, 2, 20, &page_body(5, b"two")).await;
    let img = (store.get_page(&ctx, SCOPE, page5, end2)).await.expect("page as of end2");
    assert_eq!(&img.buf.as_slice()[..3], b"two", "the missing records were applied, not failed");
    assert_eq!(store.materialized_lsn(SCOPE).expect("materialized"), end2);
    assert_eq!(store.oldest_readable_lsn(SCOPE).expect("oldest"), end2);

    // Promise 5 — never a newer-than-requested answer: without page
    // versions an older point is TooOld.
    let stale = (store.get_page(&ctx, SCOPE, page5, end1)).await;
    assert!(matches!(stale, Err(StoreError::TooOld { .. })));

    // Promise 2 — nothing non-durable is ever applied.
    let beyond = (store.get_page(&ctx, SCOPE, page5, Lsn(end2.0 + 1))).await;
    assert!(matches!(beyond, Err(StoreError::BeyondDurable { .. })));

    // Read floors register (page versions arrive with the managed service).
    (store.set_read_floor(&ctx, SCOPE, crate::types::ReaderId(1), end2)).await
        .expect("floor registers");

    // §4.6 — lineage is the branch hook; v1 refuses it loudly, and a
    // double attach is a ScopeExists.
    let lineage = crate::types::Lineage { parent: SCOPE, fork_lsn: end2 };
    assert!(matches!(
        (store.attach(&ctx, SCOPE, Some(lineage), KeyRef { key_id: 1 })).await,
        Err(StoreError::Unsupported(_))
    ));
    assert!(matches!(
        (store.attach(&ctx, SCOPE, None, KeyRef { key_id: 1 })).await,
        Err(StoreError::ScopeExists(_))
    ));
    (store.detach(&ctx, SCOPE)).await.expect("detaches");
    assert!(matches!(
        (store.get_page(&ctx, SCOPE, page5, end2)).await,
        Err(StoreError::UnknownScope(_))
    ));
}

async fn append_page_group<L: LogSink>(
    log: &L,
    span: &cascade_telemetry::Span,
    start: u64,
    xid: u64,
    ts: u64,
    cell: &[u8],
) -> Lsn {
    let ctx = test_ctx(span, 1);
    let ins = encode(
        &cascade_wal_format::WalRecord::HeapInsert {
            xid,
            segment: SegmentId(1),
            cell: cell.to_vec(),
        },
        Lsn(start),
        1,
    );
    let com = encode_with_flags(
        &cascade_wal_format::WalRecord::CommitTxn { xid, commit_ts: Hlc(ts) },
        Lsn(start + ins.len() as u64),
        1,
        FLAG_GROUP_END,
    );
    let mut bytes = ins;
    bytes.extend(com);
    (log.append(&ctx, SCOPE, E1, Lsn(start), &bytes, meta(ts))).await.expect("appends");
    (log.wait_durable(&ctx, SCOPE, Lsn(start + bytes.len() as u64))).await.expect("durable")
}
