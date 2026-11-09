//! T3.2 smoke: the runtime's gate glue — a page-stamped boundary waits on
//! the same durable watermark a committing session waits on. Gate
//! semantics live in cascade-bpm's tests; watermark semantics in
//! cascade-wal's; this file proves the adapter composes both.

use std::{cell::Cell, rc::Rc, sync::atomic::{AtomicU32, Ordering}, sync::Arc, time::Duration};

use cascade_types::{DatabaseId, Lsn};
use cascade_wal::{WalHandle, WalRecord, WalWriter};

use cascade_runtime::walgate::wal_gate;

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("walgate-test")
        .make()
        .expect("executor builds")
}

#[test]
fn gate_resolves_only_when_the_writer_publishes_durability() {
    let dir = std::env::temp_dir().join(format!(
        "cascade-walgate-test-{}-{}",
        std::process::id(),
        AtomicU32::new(0).fetch_add(1, Ordering::Relaxed)
    ));
    let writer = Arc::new(WalWriter::new());
    let gate = wal_gate(&writer);
    reactor().run(async {
        let h = WalHandle::open(&dir, DatabaseId(5)).await.expect("handle opens");

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
        writer.flush_wal(&h, end).await.expect("flush");
        glommio::timer::sleep(Duration::from_millis(5)).await;
        assert!(parked.get(), "the gate resolved once the writer published");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
