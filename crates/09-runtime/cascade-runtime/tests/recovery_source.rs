//! M6 red (storage §9, step M6): recovery reads the log through
//! `LogSource::read_log` instead of `DebugWalReader` — the log store
//! handles its own tail at open, so torn-tail handling and truncation
//! leave the replay walk entirely. The replay law itself (committed
//! transactions replay in commit order; uncommitted buffers drop; a
//! buggy encoder's unparseable cell refuses the boot) is unchanged.

use std::sync::Arc;

use cascade_runtime::recovery::{self, RecoveryError};
use cascade_storage_api::fake::MemBlockDevice;
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::scope_admin::LogScopes;
use cascade_storage_api::{KeyRef};
use cascade_types::{DatabaseId, Epoch, Hlc, Lsn, ScopeId, SegmentId, TxnId};
use cascade_wal_format::{encode_with_flags, encode, WalRecord, FLAG_GROUP_END};
use futures_lite::future::block_on;

const SCOPE: ScopeId = ScopeId(0);

fn store() -> Arc<cascade_logstore::LogStore<MemBlockDevice>> {
    block_on(cascade_logstore::LogStore::open(Arc::new(MemBlockDevice::new())))
        .expect("opens")
}

fn ctx() -> cascade_storage_api::OpCtx<'static> {
    let span = cascade_telemetry::Span::noop();
    // The span is a zero-cost no-op Copy — leaking one per test is fine.
    OpCtxLeak::leak(span)
}

/// Small shim: recovery tests want a `'static` context and the span is a
/// no-op, so lease one per call site instead of threading lifetimes.
struct OpCtxLeak;
impl OpCtxLeak {
    fn leak(span: cascade_telemetry::Span) -> cascade_storage_api::OpCtx<'static> {
        let leaked: &'static cascade_telemetry::Span = Box::leak(Box::new(span));
        cascade_storage_api::OpCtx::new(
            cascade_storage_api::TenantContext::for_database(cascade_types::DatabaseId(1)),
            leaked,
        )
    }
}

/// Stages one committed mini-transaction (insert + commit, flag on the
/// commit) and flushes it — the durable law the writer guarantees (M5).
fn commit_group(
    store: &cascade_logstore::LogStore<MemBlockDevice>,
    xid: u64,
    id: i32,
    val: &str,
    at: u64,
) -> u64 {
    let ctx = ctx();
    let ins = encode(
        &WalRecord::HeapInsert {
            xid,
            segment: SegmentId(1),
            cell: session_row_cell(xid, id, val),
        },
        Lsn(at),
        1,
    );
    let com = encode_with_flags(
        &WalRecord::CommitTxn {
            xid,
            commit_ts: Hlc(at),
        },
        Lsn(at + ins.len() as u64),
        1,
        FLAG_GROUP_END,
    );
    let mut bytes = ins;
    bytes.extend(com);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(at), &bytes, meta()))
        .expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(at + bytes.len() as u64))).expect("durables");
    at + bytes.len() as u64
}

fn meta() -> cascade_storage_api::AppendMeta {
    cascade_storage_api::AppendMeta {
        max_commit_ts: None,
        writer_durable: Lsn(0),
    }
}

/// The same row-image cell the session layer emits (the runtime's
/// `parse_heap_insert_row` decode target): a cascade-page cell whose
/// payload is `[id i32][val_len u16][val bytes]`.
fn session_row_cell(xid: u64, id: i32, val: &str) -> Vec<u8> {
    let mut image = Vec::new();
    image.extend_from_slice(&id.to_le_bytes());
    image.extend_from_slice(&(val.len() as u16).to_le_bytes());
    image.extend_from_slice(val.as_bytes());
    cascade_page::encode_cell(cascade_types::VersionWord::uncommitted(cascade_types::TxnId(xid)), &image)
        .expect("cell fits")
}

#[test]
fn recovery_reads_committed_rows_through_the_log_source() {
    let store = store();
    let ctx = ctx();
    block_on(store.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
    block_on(store.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens");

    let mut at = 0u64;
    at = commit_group(&store, 1, 1, "alpha", at);
    at = commit_group(&store, 2, 2, "beta", at);
    // An uncommitted insert: its row must drop.
    let ins = encode(
        &WalRecord::HeapInsert {
            xid: 99,
            segment: SegmentId(1),
            cell: session_row_cell(99, 99, "ghost"),
        },
        Lsn(at),
        1,
    );
    let com = encode_with_flags(
        &WalRecord::BeginTxn { xid: 99 },
        Lsn(at + ins.len() as u64),
        1,
        FLAG_GROUP_END,
    );
    let mut bytes = ins;
    bytes.extend(com);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(at), &bytes, meta())).expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(at + bytes.len() as u64))).expect("durables");

    let plan = block_on(recovery::prepare_from_source(&*store, SCOPE, store.durable_lsn(SCOPE).expect("durable")))
        .expect("prepares");
    assert_eq!(plan.resume_offset, at + bytes.len() as u64, "the durable log end is the resume point");
    assert_eq!(
        plan.seed_rows,
        vec![(1, "alpha".to_string()), (2, "beta".to_string())],
        "committed rows replay in commit order; the uncommitted buffer drops"
    );
}

#[test]
fn recovery_after_crash_replays_the_acknowledged_log() {
    let dev = Arc::new(MemBlockDevice::new());
    let store0 = block_on(cascade_logstore::LogStore::open(dev.clone())).expect("opens");
    let ctx = ctx();
    block_on(store0.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
    block_on(store0.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens");
    let end = commit_group(&store0, 5, 7, "survivor", 0);

    // Crash: unsynced bytes die; the acknowledged group is durable.
    dev.crash(0xCAFE);
    let revived = block_on(cascade_logstore::LogStore::open(dev.clone())).expect("recovers");
    let plan = block_on(recovery::prepare_from_source(&*revived, SCOPE, revived.durable_lsn(SCOPE).expect("durable")))
        .expect("prepares");
    assert_eq!(plan.resume_offset, end, "every acknowledged byte survived");
    assert_eq!(plan.seed_rows, vec![(7, "survivor".to_string())]);

    // A first boot: no log, fresh plan.
    let fresh = store();
    let plan = block_on(recovery::prepare_from_source(&*fresh, SCOPE, Lsn(0))).expect("prepares");
    assert_eq!(plan.resume_offset, 0);
    assert!(plan.seed_rows.is_empty());
}

#[test]
fn recovery_refuses_a_buggy_encoders_unparseable_cell() {
    let store = store();
    let ctx = ctx();
    block_on(store.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
    block_on(store.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens");
    // A CRC-valid HEAP_INSERT whose cell parses to nothing: refuse, never
    // serve a row nobody can vouch for (the replay walk's law).
    let bad: Vec<u8> = vec![0xFF; 8];
    let ins = encode(
        &WalRecord::HeapInsert {
            xid: 3,
            segment: SegmentId(1),
            cell: bad,
        },
        Lsn(0),
        1,
    );
    let com = encode_with_flags(
        &WalRecord::CommitTxn {
            xid: 3,
            commit_ts: Hlc(1),
        },
        Lsn(ins.len() as u64),
        1,
        FLAG_GROUP_END,
    );
    let mut bytes = ins;
    bytes.extend(com);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(0), &bytes, meta())).expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(bytes.len() as u64))).expect("durables");
    let plan = block_on(recovery::prepare_from_source(&*store, SCOPE, store.durable_lsn(SCOPE).expect("durable")));
    assert!(matches!(plan, Err(RecoveryError::Corrupt { .. })));
}

/// Migrated from the legacy matrix (M6): a log written before T1.3
/// carries LEGACY_INSERT (type 4) — F3 retains its decode so old logs
/// keep replaying alongside HEAP_INSERT ones, through the LogSource walk.
#[test]
fn legacy_insert_replays_through_the_log_source() {
    let store = store();
    let ctx = ctx();
    block_on(store.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
    block_on(store.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens");
    let ins = cascade_wal_format::encode(
        &WalRecord::LegacyInsert {
            xid: 42,
            table: SegmentId(1),
            id: 3,
            val: "legacy row".to_string(),
        },
        Lsn(0),
        1,
    );
    let com = cascade_wal_format::encode_with_flags(
        &WalRecord::CommitTxn {
            xid: 42,
            commit_ts: Hlc(1),
        },
        Lsn(ins.len() as u64),
        1,
        FLAG_GROUP_END,
    );
    let mut bytes = ins;
    bytes.extend(com);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(0), &bytes, meta())).expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(bytes.len() as u64))).expect("durables");

    let plan = block_on(recovery::prepare_from_source(
        &*store,
        SCOPE,
        store.durable_lsn(SCOPE).expect("durable"),
    ))
    .expect("prepares");
    assert_eq!(plan.seed_rows, vec![(3, "legacy row".to_string())]);
}


/// A CRC-valid record declaring a foreign LSN is a transplant: the
/// replay's declared-LSN cross-check refuses it by name (the law the
/// legacy reader enforced — the store's framing suite pins the append-
/// time half; this pins the replay half for any LogSource).
#[test]
fn a_transplanted_record_refuses_by_name() {
    struct ShiftedSource;

    use cascade_storage_api::LogSource;
    impl LogSource for ShiftedSource {
        fn read_log<'a>(
            &'a self,
            _ctx: &'a cascade_storage_api::OpCtx<'_>,
            _scope: ScopeId,
            from: Lsn,
            _max_bytes: usize,
        ) -> cascade_types::LocalBoxFuture<'a, Result<cascade_storage_api::LogChunk, cascade_storage_api::StoreError>>
        {
            Box::pin(async move {
                // A valid record encoded for LSN 100, served at cursor 0.
                let bytes = cascade_wal_format::encode(
                    &WalRecord::CommitTxn {
                        xid: 1,
                        commit_ts: Hlc(1),
                    },
                    Lsn(100),
                    1,
                );
                Ok(cascade_storage_api::LogChunk {
                    start: from,
                    bytes,
                    end: Lsn(from.0 + 52),
                    durable: Lsn(from.0 + 52),
                })
            })
        }
        fn subscribe(
            &self,
            _ctx: &cascade_storage_api::OpCtx<'_>,
            _scope: ScopeId,
            _from: Lsn,
        ) -> Box<dyn cascade_storage_api::log_source::LogSubscription> {
            unreachable!("the transplant walk uses read_log only")
        }
        fn set_retained_from<'a>(
            &'a self,
            _ctx: &'a cascade_storage_api::OpCtx<'_>,
            _scope: ScopeId,
            _holder: cascade_storage_api::RetentionHolder,
            _lsn: Lsn,
        ) -> cascade_types::LocalBoxFuture<'a, Result<(), cascade_storage_api::StoreError>> {
            Box::pin(async { Ok(()) })
        }
        fn release_holder<'a>(
            &'a self,
            _ctx: &'a cascade_storage_api::OpCtx<'_>,
            _scope: ScopeId,
            _holder: cascade_storage_api::RetentionHolder,
        ) -> cascade_types::LocalBoxFuture<'a, Result<(), cascade_storage_api::StoreError>> {
            Box::pin(async { Ok(()) })
        }
        fn retained_floor(&self, _scope: ScopeId) -> Result<Lsn, cascade_storage_api::StoreError> {
            Ok(Lsn(0))
        }
        fn retention_holders(
            &self,
            _scope: ScopeId,
        ) -> Result<Vec<cascade_storage_api::HolderReport>, cascade_storage_api::StoreError> {
            Ok(Vec::new())
        }
        fn append_before_time<'a>(
            &'a self,
            _ctx: &'a cascade_storage_api::OpCtx<'_>,
            _scope: ScopeId,
            _ts: Hlc,
        ) -> cascade_types::LocalBoxFuture<'a, Result<Lsn, cascade_storage_api::StoreError>> {
            Box::pin(async { Ok(Lsn(0)) })
        }
    }

    let span = cascade_telemetry::Span::noop();
    let ctx = cascade_storage_api::OpCtx::new(
        cascade_storage_api::TenantContext::for_database(DatabaseId(0)),
        &span,
    );
    let _ = ctx;
    let src = ShiftedSource;
    let plan = block_on(recovery::prepare_from_source(&src, SCOPE, Lsn(52)));
    assert!(matches!(
        plan,
        Err(RecoveryError::LsnMismatch { at, declared }) if at == Lsn(0) && declared == Lsn(100)
    ));
}

/// Migrated from the legacy matrix: a CRC-valid HEAP_INSERT whose cell
/// parses but whose row image is not UTF-8 refuses as Corrupt — replay
/// never coerces to U+FFFD and seeds a row nobody can vouch for.
#[test]
fn a_non_utf8_row_image_refuses_as_corrupt() {
    use cascade_page::encode_cell;
    let store = store();
    let ctx = ctx();
    block_on(store.create_scope(&ctx, SCOPE, Epoch(1), KeyRef { key_id: 1 })).expect("creates");
    block_on(store.open_writer(&ctx, SCOPE, Epoch(1))).expect("opens");
    // Shape-valid debug row image: [id i32][len u16][3 bytes of 0xFF] —
    // the length matches, the bytes are not UTF-8.
    let mut image = Vec::new();
    image.extend_from_slice(&1i32.to_le_bytes());
    image.extend_from_slice(&3u16.to_le_bytes());
    image.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    let cell = encode_cell(cascade_types::VersionWord::uncommitted(TxnId(8)), &image)
        .expect("cell fits");
    let ins = cascade_wal_format::encode(
        &WalRecord::HeapInsert {
            xid: 8,
            segment: SegmentId(1),
            cell,
        },
        Lsn(0),
        1,
    );
    let com = cascade_wal_format::encode_with_flags(
        &WalRecord::CommitTxn {
            xid: 8,
            commit_ts: Hlc(1),
        },
        Lsn(ins.len() as u64),
        1,
        FLAG_GROUP_END,
    );
    let mut bytes = ins;
    bytes.extend(com);
    block_on(store.append(&ctx, SCOPE, Epoch(1), Lsn(0), &bytes, meta())).expect("appends");
    block_on(store.wait_durable(&ctx, SCOPE, Lsn(bytes.len() as u64))).expect("durables");
    let plan = block_on(recovery::prepare_from_source(
        &*store,
        SCOPE,
        store.durable_lsn(SCOPE).expect("durable"),
    ));
    assert!(matches!(plan, Err(RecoveryError::Corrupt { .. })));
}
