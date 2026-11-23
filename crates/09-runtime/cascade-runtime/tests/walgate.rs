//! T3.2 smoke: the runtime's gate glue — a page-stamped boundary waits on
//! the same durable watermark a committing session waits on. Gate
//! semantics live in cascade-bpm's tests; watermark semantics in
//! cascade-wal's; this file proves the adapter composes both.

use std::{cell::Cell, rc::Rc, sync::atomic::{AtomicU32, Ordering}, sync::Arc, time::Duration};

use cascade_types::Lsn;
use cascade_wal::WalRecord;

use cascade_runtime::walgate::wal_gate;

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("walgate-test")
        .make()
        .expect("executor builds")
}


/// M6: durability rides the sink — a MemLogStore-backed writer for tests
/// that need real flushes without a filesystem WAL.
fn sink_writer(
) -> (std::sync::Arc<cascade_storage_api::fake::MemLogStore>, std::sync::Arc<cascade_wal::WalWriter>) {
    use cascade_storage_api::fake::{test_ctx as sink_ctx, MemLogStore};
    use cascade_storage_api::LogScopes as _;
    futures_lite::future::block_on(async {
        let store = std::sync::Arc::new(MemLogStore::new());
        let span = cascade_telemetry::Span::noop();
        let ctx = sink_ctx(&span, 0);
        let _ = store
            .create_scope(&ctx, cascade_types::ScopeId(0), cascade_types::Epoch::FIRST,
                          cascade_storage_api::KeyRef { key_id: 1 })
            .await;
        let writer = cascade_wal::WalWriter::open_with_sink(
            store.clone(),
            &ctx,
            cascade_types::ScopeId(0),
            cascade_types::Epoch::FIRST,
        )
        .await
        .expect("writer opens");
        (store, std::sync::Arc::new(writer))
    })
}

#[test]
fn gate_resolves_only_when_the_writer_publishes_durability() {
    let dir = std::env::temp_dir().join(format!(
        "cascade-walgate-test-{}-{}",
        std::process::id(),
        AtomicU32::new(0).fetch_add(1, Ordering::Relaxed)
    ));
    let (_store, writer) = sink_writer();
    let gate = wal_gate(&writer);
    reactor().run(async {
        let r = writer.append_wal(&WalRecord::BeginTxn { xid: 1 });
        let end = r.end();
        let _ = r.fill();

        // The gate reports the writer's durable watermark — 0, nothing
        // flushed — so a page stamped at `end` must stay parked.
        assert_eq!(gate.durable_lsn(), Lsn(0));
        let parked = Rc::new(Cell::new(false));
        let flag = Rc::clone(&parked);
        // Detached on purpose: the flag below is polled, not awaited.
        let _waiter = {
            let gate = Arc::clone(&gate);
            glommio::spawn_local(async move {
                gate.wait_durable(end).await.expect("gate resolves");
                flag.set(true);
            })
        };
        glommio::timer::sleep(Duration::from_millis(5)).await;
        assert!(!parked.get(), "no durability yet — the gate must stay parked");

        // A commit flush publishes durability; the gate's parked waiter
        // resolves off the same watermark the session acked against.
        writer.flush_wal_sink(end).await.expect("flush");
        glommio::timer::sleep(Duration::from_millis(5)).await;
        assert!(parked.get(), "the gate resolved once the writer published");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
