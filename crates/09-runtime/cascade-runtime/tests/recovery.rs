//! T3.3: boot recovery — the plan [`recovery::prepare`] produces for every
//! crash/clean shape in the matrix, and what boot does with it. The matrix:
//!
//! | log shape                          | plan                          |
//! |------------------------------------|-------------------------------|
//! | no log                             | fresh: first boot, resume 0   |
//! | committed + uncommitted txns       | committed replayed, rest drop |
//! | torn tail (crash mid-record)       | truncated, boot continues     |
//! | mid-log CRC failure                | refuse — never serve over it  |
//! | clean marker at log end            | clean — replay still runs*    |
//! | bytes past the marker              | crash after clean: unclean    |
//!
//! \* the debug slice's WAL is the only durable state home (and since
//! storage M8 there are no page checkpoints at all — pages are never
//! written back), so replay is not optional even after a clean
//! stop; the marker decides the boot's *character*, not whether to walk.
//!
//! The end-to-end test runs the real loop: crash-shaped log → prepare →
//! boot → serve → more commits → restart → everything still there, and the
//! writer provably continued the log instead of overwriting it.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    recovery::{self},
    session,
    tenant,
    AcceptLoop, Runtime, RuntimeConfig,
};
use cascade_types::{DatabaseId, Lsn, SegmentId, VersionWord, TxnId};
use cascade_blockdev::GlommioBlockDevice;
use cascade_logstore::LogStore;
use cascade_storage_api::KeyRef;
use cascade_telemetry::Span;
use cascade_wal::WalRecord;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_wal_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cascade-recovery-test-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn db() -> DatabaseId {
    tenant::dev_context().db
}

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("recovery-test")
        .make()
        .expect("executor builds")
}

// The legacy matrix (torn tails, mid-log CRC refusals, clean markers,
// byte-position transplants) died with the file path: its laws now live
// in the store-backed suites — cascade-logstore's recovery scan tests
// (torn vs corruption), cascade-storage-api's framing law (LsnMismatch
// moved to append-time BadLsn), and recovery_source.rs (replay + refusals
// through LogSource). A development data directory written by the old
// single-file WAL is recreated, not converted (§9, M6).

struct Fixture {
    url: String,
    rt: Runtime,
}

/// Every insert record in the log (either shape), in log order — the
/// read path a recovery walk uses. M6: through the LOCAL LOG STORE
/// (durable, gap-free; the store handled its tail at open).
fn wal_rows(dir: &Path) -> Vec<(i32, String)> {
    use std::sync::Arc;
    reactor().run(async {
        let dev = Arc::new(GlommioBlockDevice::new(dir.join("store")));
        let store = LogStore::open(dev).await.expect("store opens");
        let span = cascade_telemetry::Span::noop();
        let ctx = cascade_storage_api::OpCtx::new(
            cascade_storage_api::TenantContext::for_database(db()),
            &span,
        );
        let scope = cascade_types::ScopeId(u64::from(db().0));
        use cascade_storage_api::LogSource as _;
        let durable = cascade_storage_api::LogSink::durable_lsn(&*store, scope).expect("durable");
        let mut rows = Vec::new();
        let mut pos = Lsn(0);
        while pos < durable {
            let chunk = store.read_log(&ctx, scope, pos, 1 << 20).await.expect("chunk");
            if chunk.bytes.is_empty() {
                break;
            }
            let mut at = 0usize;
            while at < chunk.bytes.len() {
                let d = cascade_wal_format::decode(&chunk.bytes[at..]).expect("record");
                match d.record {
                    WalRecord::LegacyInsert { id, val, .. } => rows.push((id, val)),
                    WalRecord::HeapInsert { cell, .. } => {
                        rows.push(session::parse_heap_insert_row(&cell).expect("row image parses"));
                    }
                    _ => {}
                }
                at += d.total;
                pos = Lsn(pos.0 + d.total as u64);
            }
        }
        rows
    })
}

fn table_rows(url: &str) -> Vec<(i32, String)> {
    let mut c = postgres::Client::connect(url, postgres::NoTls).expect("client connects");
    let rows = c.simple_query("SELECT * FROM debug_table").expect("select");
    rows.iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some((
                r.get("id").unwrap().parse().expect("int id"),
                r.get("val").unwrap().to_owned(),
            )),
            _ => None,
        })
        .collect()
}

fn boot_with_recovery(wal_dir: &Path) -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = cascade_runtime::SyntheticTopology::new(allowed.iter().copied());
    // M6: the plan comes from the LOCAL LOG STORE (the same device boot
    // will open) — the legacy file walk no longer decides boots.
    let plan = {
        use std::sync::Arc;
        reactor().run(async {
            let dev = Arc::new(GlommioBlockDevice::new(wal_dir.join("store")));
            let store = LogStore::open(dev).await.expect("store opens");
            let span = Span::noop();
            let _ctx = cascade_storage_api::OpCtx::new(
                cascade_storage_api::TenantContext::for_database(db()),
                &span,
            );
            let scope = cascade_types::ScopeId(u64::from(db().0));
            
            let durable =
                cascade_storage_api::LogSink::durable_lsn(&*store, scope).expect("durable");
            recovery::prepare_from_source(&*store, scope, durable)
                .await
                .expect("prepares")
        })
    };
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![allowed[0]],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: vec![allowed[1], allowed[2]],
        }],
        numa_pools: vec![NumaPool {
            numa: 0,
            size_mb: 1,
        }],
        wal_dir: Some(wal_dir.to_path_buf()),
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(
        &cfg,
        &topo,
        Some(listener),
        AuthConfig::default(),
        Some(&plan),
    )
    .expect("boot routed");
    Fixture {
        url: format!("postgres://alice:pw@127.0.0.1:{port}/dev"),
        rt,
    }
}


#[test]
fn boot_replays_then_restarts_preserve_everything() {
    let dir = temp_wal_dir();
    // The pre-crash transaction rides the STORE (M6): the legacy file
    // format is gone, and the store is what a real crash left behind.
    {
        use std::sync::Arc;
        let dev = Arc::new(GlommioBlockDevice::new(dir.join("store")));
        let store = reactor().run(async {
            let s = LogStore::open(dev).await.expect("opens");
            let span = Span::noop();
            let ctx = cascade_storage_api::OpCtx::new(
                cascade_storage_api::TenantContext::for_database(db()),
                &span,
            );
            let scope = cascade_types::ScopeId(u64::from(db().0));
            let _ = cascade_storage_api::LogScopes::create_scope(
                &*s,
                &ctx,
                scope,
                cascade_types::Epoch::FIRST,
                KeyRef { key_id: 1 },
            )
            .await;
            s
        });
        let scope = cascade_types::ScopeId(u64::from(db().0));
        let ins = cascade_wal_format::encode(
            &WalRecord::HeapInsert {
                xid: 1,
                segment: SegmentId(1),
                cell: cascade_page::encode_cell(
                    VersionWord::uncommitted(TxnId(1)),
                    &{
                        let mut img = Vec::new();
                        img.extend_from_slice(&1i32.to_le_bytes());
                        img.extend_from_slice(&("pre-crash".len() as u16).to_le_bytes());
                        img.extend_from_slice(b"pre-crash");
                        img
                    },
                )
                .expect("cell"),
            },
            Lsn(0),
            1,
        );
        let com = cascade_wal_format::encode_with_flags(
            &WalRecord::CommitTxn {
                xid: 1,
                commit_ts: cascade_types::Hlc(1),
            },
            Lsn(ins.len() as u64),
            1,
            cascade_wal_format::FLAG_GROUP_END,
        );
        let mut bytes = ins;
        bytes.extend(com);
        let meta = cascade_storage_api::AppendMeta {
            max_commit_ts: None,
            writer_durable: Lsn(0),
        };
        let end = Lsn(bytes.len() as u64);
        reactor().run(async {
            use cascade_storage_api::LogSink as _;
            let span = Span::noop();
            let ctx = cascade_storage_api::OpCtx::new(
                cascade_storage_api::TenantContext::for_database(db()),
                &span,
            );
            store
                .append(&ctx, scope, cascade_types::Epoch::FIRST, Lsn(0), &bytes, meta)
                .await
                .expect("appends");
            store.wait_durable(&ctx, scope, end).await.expect("durables");
        });
    }

    // Boot 1 over the crash-shaped log: the replayed row is served, and
    // the writer's watermarks continue the log instead of restarting.
    let fx = boot_with_recovery(&dir);
    assert_eq!(table_rows(&fx.url), vec![(1, "pre-crash".to_owned())]);
    let w = fx.rt.debug_wal().expect("wal writer").clone();
    assert!(
        w.reserved().0 > 0,
        "the writer resumes at the replayed end, not 0"
    );

    // This boot commits through the resumed writer — appending, never
    // overwriting, the replayed prefix.
    let mut c = postgres::Client::connect(&fx.url, postgres::NoTls).expect("client");
    c.simple_query("INSERT INTO debug_table VALUES (2, 'post-crash')")
        .expect("insert");
    drop(c);
    assert_eq!(wal_rows(&dir).len(), 2, "both txns decode in one log");
    drop(fx.rt);

    // Boot 2: the restart replays everything the previous boot acked.
    let fx = boot_with_recovery(&dir);
    assert_eq!(
        table_rows(&fx.url),
        vec![(1, "pre-crash".to_owned()), (2, "post-crash".to_owned())]
    );
    drop(fx.rt);
    let _ = std::fs::remove_dir_all(&dir);
}

// The two legacy-law tests this file used to carry — LEGACY_INSERT
// replay retention and the non-UTF-8 row-image refusal — are migrated to
// `recovery_source.rs`, where they run against the log store's
// LogSource walk (M6: recovery reads through LogSource, not files).
