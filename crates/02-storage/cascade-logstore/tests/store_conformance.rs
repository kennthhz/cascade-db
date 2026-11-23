//! M4 slice 3 gates (§9, §10): the real log store passes the same
//! conformance suites the fake passes; crash tests cut at seeded random
//! points; the noisy-neighbor law holds — a heavy scope's backlog cannot
//! starve a committing scope beyond one frame.

use std::sync::Arc;

use cascade_logstore::LogStore;
use cascade_storage_api::conformance::{
    log_sink_suite, log_source_suite, LogStoreHarness,
};
use cascade_storage_api::fake::{test_ctx, MemBlockDevice};
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::log_source::LogSource;
use cascade_storage_api::scope_admin::LogScopes;
use cascade_storage_api::{AppendMeta, KeyRef, StoreError};
use cascade_types::{Epoch, Hlc, Lsn, ScopeId, SegmentId};
use cascade_wal_format::{encode_with_flags, encode, WalRecord, FLAG_GROUP_END};
use futures_lite::future::block_on;

const SCOPE: ScopeId = ScopeId(7);
const SMALL: ScopeId = ScopeId(8);

fn commit_rec(xid: u64, ts: u64) -> WalRecord {
    WalRecord::CommitTxn {
        xid,
        commit_ts: Hlc(ts),
    }
}

/// One mini-transaction at `at` in epoch 1: commit-record only, flag on.
fn commit_bytes(xid: u64, ts: u64, at: u64) -> Vec<u8> {
    commit_bytes_in(xid, ts, at, 1)
}

fn commit_bytes_in(xid: u64, ts: u64, at: u64, epoch: u64) -> Vec<u8> {
    encode_with_flags(&commit_rec(xid, ts), Lsn(at), epoch, FLAG_GROUP_END)
}

/// One mini-transaction at `at`: heap insert + commit, flag on the commit.
fn group_bytes(xid: u64, ts: u64, body: &[u8], at: u64) -> Vec<u8> {
    let ins = encode(
        &WalRecord::HeapInsert {
            xid,
            segment: SegmentId(1),
            cell: body.to_vec(),
        },
        Lsn(at),
        1,
    );
    let com = encode_with_flags(
        &commit_rec(xid, ts),
        Lsn(at + ins.len() as u64),
        1,
        FLAG_GROUP_END,
    );
    let mut b = ins;
    b.extend(com);
    b
}

fn meta_none() -> AppendMeta {
    AppendMeta {
        max_commit_ts: None,
        writer_durable: Lsn(0),
    }
}

/// The harness: a fresh store over one MemBlockDevice; crash discards the
/// device's unsynced blocks at a seeded cut; restart re-opens (the §5.7
/// scan); inject arms the device's one-shot sync failure (S19).
struct RealStoreHarness {
    dev: Arc<MemBlockDevice>,
    seed: u64,
}

impl RealStoreHarness {
    fn new() -> Self {
        RealStoreHarness {
            dev: Arc::new(MemBlockDevice::new()),
            seed: 0xC0F_000,
        }
    }
}

impl LogStoreHarness for RealStoreHarness {
    type Store = LogStore<MemBlockDevice>;

    fn create(&mut self) -> Arc<Self::Store> {
        // A fresh store means a fresh device — the suites create scopes
        // that a re-opened device would already hold.
        self.dev = Arc::new(MemBlockDevice::new());
        block_on(LogStore::open(self.dev.clone())).expect("opens")
    }

    fn crash(&mut self, _store: &Arc<Self::Store>) {
        self.seed += 1;
        self.dev.crash(self.seed);
    }

    fn restart(&mut self, _store: &Arc<Self::Store>) -> Arc<Self::Store> {
        block_on(LogStore::open(self.dev.clone())).expect("reopens")
    }

    fn inject_flush_failure(&mut self, _store: &Arc<Self::Store>) {
        self.dev.inject_sync_failure();
    }
}

#[test]
fn the_real_store_passes_the_log_store_suites() {
    let mut h = RealStoreHarness::new();
    block_on(log_sink_suite(&mut h));
    block_on(log_source_suite(&mut h));
}

/// §10 crash tests: write groups, crash at seeded cut points, recover
/// with no acknowledged record lost and no unacknowledged record
/// half-present.
#[test]
fn crash_at_cut_points_loses_no_acknowledged_record() {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    for seed in 1..=16u64 {
        let dev = Arc::new(MemBlockDevice::new());
        let store = block_on(LogStore::open(dev.clone())).expect("opens");
        block_on(store.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
        block_on(store.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens writer");

        // Twelve commit groups; the cut lands inside the `cut`-th one's
        // flush (the crash hits whatever was unsynced at that moment).
        let cut = (seed % 12) + 1;
        let mut acked_end = Lsn(0);
        let mut acknowledged: Vec<u8> = Vec::new();
        let mut at = 0u64;
        for i in 0..12u64 {
            let bytes = commit_bytes(i + 1, i * 10 + 1, at);
            block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(at), &bytes, meta_none()))
                .expect("appends");
            let end = Lsn(at + bytes.len() as u64);
            block_on(store.wait_durable(&ctx, SCOPE, end)).expect("flushes");
            acked_end = end;
            let chunk =
                block_on(store.read_log(&ctx, SCOPE, Lsn(0), 1 << 20)).expect("reads");
            acknowledged = chunk.bytes.clone();
            at = end.0;
            if i + 1 == cut {
                break;
            }
        }

        // Crash and recover: every acknowledged record survives.
        dev.crash(0xBEEF_0000 + seed);
        let revived = block_on(LogStore::open(dev.clone())).expect("recovers");
        assert_eq!(
            revived.durable_lsn(SCOPE).expect("durable"),
            acked_end,
            "seed {seed}: every acknowledged record survived"
        );
        let back = block_on(revived.read_log(&ctx, SCOPE, Lsn(0), 1 << 20)).expect("reads");
        assert_eq!(
            back.bytes, acknowledged,
            "seed {seed}: the acknowledged log replays byte-identical"
        );
        if acked_end > Lsn(1) {
            let inside = block_on(revived.read_log(&ctx, SCOPE, Lsn(1), 1 << 20));
            assert!(
                matches!(inside, Err(StoreError::NotRecordBoundary { .. })),
                "seed {seed}: interior offsets refuse after recovery — got {inside:?}"
            );
        }
    }
}

/// §5.7 + §4.3 promise 7: a failed sync is permanent for the epoch, and
/// the stream rolls to a fresh segment so a reopen writes into a file
/// the poisoned tail cannot touch.
#[test]
fn a_failed_sync_fails_the_epoch_and_rolls_the_segment() {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let dev = Arc::new(MemBlockDevice::new());
    let store = block_on(LogStore::open(dev.clone())).expect("opens");
    block_on(store.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
    block_on(store.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens");

    let bytes = commit_bytes(1, 1, 0);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(0), &bytes, meta_none())).expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(bytes.len() as u64))).expect("first flush syncs");

    dev.inject_sync_failure();
    let bytes2 = commit_bytes(2, 2, bytes.len() as u64);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(bytes.len() as u64), &bytes2, meta_none()))
        .expect("appends");
    let err = block_on(store.wait_durable(&ctx, SCOPE, Lsn(bytes.len() as u64 + bytes2.len() as u64)));
    assert!(
        matches!(err, Err(StoreError::Failed { .. })),
        "the poisoned sync fails the epoch: {err:?}"
    );
    assert!(matches!(
        block_on(store.append(
            &ctx,
            SCOPE,
            Epoch(1),
            Lsn(bytes.len() as u64 + bytes2.len() as u64),
            &commit_bytes(2, 3, bytes.len() as u64 + bytes2.len() as u64),
            meta_none()
        )),
        Err(StoreError::Failed { .. })
    ));

    // Reopen at a new epoch: the failed frame's bytes were never
    // acknowledged, so the new writer's first append starts at the
    // DURABLE LSN (§4.3 promise 1) — the stream rolls to a fresh segment
    // and the writer works again.
    block_on(store.open_writer(&ctx, SCOPE, Epoch(2))).expect("reopen recovers");
    let at3 = bytes.len() as u64; // group 2 died with its epoch
    assert_eq!(store.durable_lsn(SCOPE).expect("durable"), Lsn(at3));
    let bytes3 = commit_bytes_in(3, 3, at3, 2);
    block_on(store.append(&ctx, SCOPE, Epoch(2), Lsn(at3), &bytes3, meta_none())).expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(at3 + bytes3.len() as u64)))
        .expect("flushes after reopen");
}

/// §10 noisy neighbor: a heavy scope piles up a large backlog; a small
/// scope's commits keep landing — waiters are served before streamed-only
/// scopes (§5.4 item 3), and the heavy scope only ever rides spare frame
/// capacity.
#[test]
fn a_heavy_backlog_cannot_starve_a_committing_scope() {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    let dev = Arc::new(MemBlockDevice::new());
    let store = block_on(LogStore::open(dev.clone())).expect("opens");
    for scope in [SCOPE, SMALL] {
        block_on(store.create_scope(&ctx, scope, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
        block_on(store.open_writer(&ctx, scope, Epoch(1))).expect("opens");
    }

    // The heavy scope stages a large backlog without ever waiting.
    let heavy_body = vec![0xA7u8; 100_000];
    let mut heavy_at = 0u64;
    for i in 0..20u64 {
        let bytes = group_bytes(1_000 + i, i * 10 + 1, &heavy_body, heavy_at);
        block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(heavy_at), &bytes, meta_none()))
            .expect("heavy appends");
        heavy_at += bytes.len() as u64;
    }

    // The small scope commits five times; each commit lands durably
    // despite the backlog.
    let mut small_end = Lsn(0);
    for i in 0..5u64 {
        let bytes = group_bytes(2_000 + i, i * 7 + 1, b"small", small_end.0);
        block_on(store.append(&ctx, SMALL, Epoch(1), small_end, &bytes, meta_none()))
            .expect("small appends");
        small_end = block_on(store.wait_durable(&ctx, SMALL, Lsn(small_end.0 + bytes.len() as u64)))
            .expect("small commit lands");
        assert_eq!(
            store.durable_lsn(SMALL).expect("small durable"),
            small_end,
            "commit {i}: the small scope's bytes are durable now"
        );
    }

    // And the heavy scope did not monopolize: it still has an unflushed
    // backlog (only spare frame capacity carried its bytes).
    let heavy_durable = store.durable_lsn(SCOPE).expect("heavy durable");
    let heavy_appended = store.appended_lsn(SCOPE).expect("appended");
    assert!(
        heavy_durable < heavy_appended,
        "the backlog was not drained at the small scope's expense"
    );
}

/// The cross-executor lifecycle the runtime boot exercises: open on one
/// ring, append+flush on the SAME Arc from another ring, reopen on a
/// third — the scan must find the flushed frame.
#[test]
fn reopen_across_executors_sees_the_flushed_frame() {
    use std::sync::Arc;
    let dev = Arc::new(MemBlockDevice::new());
    let store = block_on(cascade_logstore::LogStore::open(dev.clone())).expect("opens");
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    block_on(store.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
    block_on(store.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens");
    let bytes = commit_bytes(1, 1, 0);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(0), &bytes, meta_none())).expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(bytes.len() as u64))).expect("durables");

    let reopened = block_on(cascade_logstore::LogStore::open(dev.clone())).expect("reopens");
    assert_eq!(
        cascade_storage_api::LogSink::durable_lsn(&*reopened, SCOPE).expect("durable"),
        Lsn(bytes.len() as u64),
        "the scan finds the flushed frame across the reopen"
    );
}
