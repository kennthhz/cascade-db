//! The writeback-gate glue (T3.2, FR-24): [`cascade_bpm`] defines the
//! [`WalDurability`] contract and deliberately never depends on the WAL
//! crate — this adapter closes the loop over the per-DB `WalWriter`. A
//! dirty page stamped with WAL boundary `L` waits here exactly where a
//! committing session waits: on the writer's durable watermark (FR-26).
//!
//! The behavioral coverage lives on either side of this file — gate
//! semantics (blocking, refusal, re-modified stamps) in cascade-bpm's
//! tests against fakes, watermark semantics in cascade-wal's tests against
//! the real writer. This adapter adds only the error mapping and the
//! `Arc<dyn WalDurability>` handoff (the gate became `Send + Sync` with
//! the cross-core pool, T6.2 — pools ride `Arc` now); the smoke test in
//! `tests/walgate.rs` proves the composition resolves.

use std::{future::Future, pin::Pin, sync::Arc};

use cascade_bpm::{BpmError, WalDurability};
use cascade_types::Lsn;
use cascade_wal::WalWriter;

/// The gate a pool's BPM holds: the DB's shared writer behind an `Arc`
/// (pool side shares it across reactors; the writer itself is
/// `Arc`-shared).
pub struct WalGate(Arc<WalWriter>);

impl WalDurability for WalGate {
    fn durable_lsn(&self) -> Lsn {
        self.0.durable()
    }

    fn wait_durable(
        &self,
        lsn: Lsn,
    ) -> Pin<Box<dyn Future<Output = Result<(), BpmError>> + '_>> {
        Box::pin(async move {
            // Same wait a committing session parks on — a WAL failure
            // surfaces as a gate refusal: the writeback must not proceed.
            self.0
                .wait_durable(lsn)
                .await
                .map_err(|e| BpmError::WalGate(format!("wal durability wait failed: {e}")))
        })
    }
}

/// The gate for a DB's writer, ready for `Bpm::set_gate`.
pub fn wal_gate(writer: &Arc<WalWriter>) -> Arc<dyn WalDurability> {
    Arc::new(WalGate(Arc::clone(writer)))
}
