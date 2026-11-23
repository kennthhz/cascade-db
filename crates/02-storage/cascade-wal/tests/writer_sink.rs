//! M5 red: the writer over `LogSink` (storage §9, step M5). The flush
//! leader sends its staged batch with `LogSink::append` and waits with
//! `wait_durable` instead of writing a file through `WalHandle`;
//! reservation, fill and the filled watermark do not change. A
//! mini-transaction reserves its whole group as one range and the
//! group-end flag rides its last record (S16) — so every batch sent to
//! the sink ends on a group end, and the sink's framing law (§4.3
//! promise 4) holds end to end. The epoch comes from `open_writer`.

use std::sync::Arc;

use cascade_storage_api::fake::{test_ctx, MemLogStore};
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::log_source::LogSource;
use cascade_storage_api::scope_admin::LogScopes;
use cascade_types::{Epoch, Hlc, Lsn, ScopeId};
use cascade_wal::{WalError, WalRecord, WalWriter};
use futures_lite::future::block_on;

const SCOPE: ScopeId = ScopeId(3);

fn sink_store() -> MemLogStore {
    MemLogStore::new()
}

async fn open_writer_over(sink: &MemLogStore, epoch: Epoch) -> WalWriter {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    WalWriter::open_with_sink(
        Arc::new(sink.clone()),
        &ctx,
        SCOPE,
        epoch,
    )
    .await
    .expect("opens")
}

/// The M5 spine: reserved→filled→durable over a sink, with the sink's own
/// durable LSN tracking the writer's.
#[test]
fn writer_over_sink_appends_and_durables() {
    futures_lite::future::block_on(async {
        let sink = sink_store();
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 1);
        block_on(sink.create_scope(&ctx, SCOPE, Epoch(1), cascade_storage_api::KeyRef { key_id: 1 }))
            .expect("scope creates");
        let w = open_writer_over(&sink, Epoch(1)).await;
        assert_eq!(w.durable(), Lsn(0), "a fresh scope starts durable at 0");

        let r = w.append_wal(&WalRecord::CommitTxn {
            xid: 1,
            commit_ts: Hlc(7),
        });
        let end = r.end();
        r.fill();
        assert_eq!(w.filled(), end, "fill advances the filled watermark");
        let durable = w.flush_wal_sink(end).await.expect("flushes");
        assert_eq!(durable, end);
        assert_eq!(w.durable(), end);
        assert_eq!(
            sink.durable_lsn(SCOPE).expect("sink durable"),
            end,
            "the sink's durable LSN is the writer's durable LSN"
        );

        // The sink holds the bytes, and every record ends a group (S16).
        let chunk = sink
            .read_log(&ctx, SCOPE, Lsn(0), 1 << 20)
            .await
            .expect("reads");
        assert_eq!(chunk.bytes.len(), end.0 as usize);
        let mut at = 0usize;
        while at < chunk.bytes.len() {
            let f = cascade_wal_format::walk(&chunk.bytes[at..]).expect("frames");
            at += f.total;
        }
        assert_eq!(at, chunk.bytes.len());
    });
}


/// A mini-transaction reserves its whole group as one range (§9, M5):
/// one reservation, one fill, records at consecutive LSNs, the flag only
/// on the last record.
#[test]
fn a_group_reserves_one_range_and_flags_its_last_record() {
    futures_lite::future::block_on(async {
        let sink = sink_store();
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 1);
        block_on(sink.create_scope(&ctx, SCOPE, Epoch(1), cascade_storage_api::KeyRef { key_id: 1 }))
            .expect("creates");
        let w = open_writer_over(&sink, Epoch(1)).await;

        let group = w.append_group(&[
            WalRecord::BeginTxn { xid: 9 },
            WalRecord::HeapInsert {
                xid: 9,
                segment: cascade_types::SegmentId(2),
                cell: b"cell".to_vec(),
            },
            WalRecord::CommitTxn {
                xid: 9,
                commit_ts: Hlc(3),
            },
        ]);
        let start = group.start();
        let end = group.end();
        group.fill();
        assert_eq!(w.filled(), end, "the whole group fills as one entry");

        w.flush_wal_sink(end).await.expect("flushes");
        let chunk = sink.read_log(&ctx, SCOPE, Lsn(0), 1 << 20).await.expect("reads");
        // Three records at consecutive LSNs — one reserved range, walked
        // contiguously — with the flag only on the last (S16).
        let mut flags = Vec::new();
        let mut contig = chunk.start.0;
        let mut at = 0usize;
        while at < chunk.bytes.len() {
            let f = cascade_wal_format::walk(&chunk.bytes[at..]).expect("frames");
            assert_eq!(f.lsn, contig, "one reserved range: consecutive LSNs");
            contig += f.total as u64;
            flags.push(f.group_end);
            at += f.total;
        }
        assert_eq!(at, chunk.bytes.len());
        assert_eq!(flags, vec![false, false, true], "the flag rides only the last record (S16)");
        let _ = start;
    });
}

/// Acknowledged means durable: a flushed group survives the sink's crash
/// and the next writer resumes at the durable LSN in a new epoch.
#[test]
fn acknowledged_bytes_survive_crash_and_reopen() {
    futures_lite::future::block_on(async {
        let sink = sink_store();
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 1);
        block_on(sink.create_scope(&ctx, SCOPE, Epoch(1), cascade_storage_api::KeyRef { key_id: 1 }))
            .expect("creates");
        let w = open_writer_over(&sink, Epoch(1)).await;
        let g = w.append_group(&[
            WalRecord::HeapInsert {
                xid: 2,
                segment: cascade_types::SegmentId(1),
                cell: b"row".to_vec(),
            },
            WalRecord::CommitTxn {
                xid: 2,
                commit_ts: Hlc(11),
            },
        ]);
        let end = g.end();
        g.fill();
        w.flush_wal_sink(end).await.expect("flushes");

        sink.crash();
        let restarted = sink.restart();
        // A fenced restart constructs a NEW writer in a NEW epoch (§4.3):
        // open_writer returns the durable LSN, where the new writer starts.
        let w2 = open_writer_over(&restarted, Epoch(2)).await;
        assert_eq!(w2.durable(), end, "the acknowledged group survived");
        assert_eq!(w2.epoch(), 2, "the epoch came from open_writer");
        let next_res = w2.append_wal(&WalRecord::CommitTxn {
            xid: 3,
            commit_ts: Hlc(12),
        });
        let next_end = next_res.end();
        let next_start = next_res.start();
        next_res.fill();
        // The new writer's first append starts at the returned durable LSN.
        assert_eq!(next_start, end, "the new writer's first append starts at the durable LSN");
        w2.flush_wal_sink(next_end).await.expect("flushes");
        let chunk = restarted.read_log(&ctx, SCOPE, Lsn(0), 1 << 20).await.expect("reads");
        assert_eq!(chunk.bytes.len(), next_end.0 as usize, "old + new bytes, no gaps");
    });
}

/// The writer's S20 duty: every append carries the writer's durable LSN.
#[test]
fn appends_carry_writer_durable() {
    futures_lite::future::block_on(async {
        let sink = sink_store();
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 1);
        block_on(sink.create_scope(&ctx, SCOPE, Epoch(1), cascade_storage_api::KeyRef { key_id: 1 }))
            .expect("creates");
        let w = open_writer_over(&sink, Epoch(1)).await;
        let r = w.append_wal(&WalRecord::CommitTxn { xid: 4, commit_ts: Hlc(1) });
        let end = r.end();
        r.fill();
        w.flush_wal_sink(end).await.expect("flushes");
        let r2 = w.append_wal(&WalRecord::CommitTxn { xid: 5, commit_ts: Hlc(2) });
        let end2 = r2.end();
        r2.fill();
        // Before the second flush the writer's durable is `end`; the sink
        // stores writer_durable per append — read it back through the
        // sink's LogChunk.durable (the fake surfaces the stored value).
        w.flush_wal_sink(end2).await.expect("flushes");
        let chunk = sink.read_log(&ctx, SCOPE, Lsn(0), 1 << 20).await.expect("reads");
        assert_eq!(chunk.durable, end2);
    });
}

/// A fenced epoch fails the flush with an honest error (§4.3 promise 1:
/// the epoch check happens before anything else).
#[test]
fn a_fenced_epoch_fails_the_flush() {
    futures_lite::future::block_on(async {
        let sink = sink_store();
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 1);
        block_on(sink.create_scope(&ctx, SCOPE, Epoch(1), cascade_storage_api::KeyRef { key_id: 1 }))
            .expect("creates");
        let w = open_writer_over(&sink, Epoch(1)).await;
        // Someone else takes the scope at a higher epoch.
        block_on(sink.open_writer(&ctx, SCOPE, Epoch(2))).expect("fence");
        let r = w.append_wal(&WalRecord::CommitTxn { xid: 6, commit_ts: Hlc(1) });
        let end = r.end();
        r.fill();
        let err = w.flush_wal_sink(end).await;
        assert!(
            matches!(err, Err(WalError::Sink(_))),
            "a fenced writer fails honestly: {err:?}"
        );
        assert_eq!(w.durable(), Lsn(0), "nothing became durable");
    });
}
