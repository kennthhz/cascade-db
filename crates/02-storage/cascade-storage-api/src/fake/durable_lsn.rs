//! [`MemDurableLsn`] (§4.10): the [`DurableLsn`] view over per-scope
//! atomics — the test stand-in for the log store's published durable LSN.
//! `advance` is monotone (a `fetch_max`), mirroring the real prefix: it
//! grows, never rewinds. Tests that model a slow log set it by hand.

use std::collections::HashMap;
use std::sync::Mutex;

use cascade_types::{Lsn, ScopeId};

use crate::page_source::DurableLsn;

#[derive(Default)]
pub struct MemDurableLsn {
    scopes: Mutex<HashMap<ScopeId, u64>>,
}

impl MemDurableLsn {
    pub fn new() -> Self {
        MemDurableLsn::default()
    }

    /// Advances the scope's durable LSN to at least `lsn` (monotone — a
    /// stale advance never rewinds the view).
    pub fn advance(&self, scope: ScopeId, lsn: Lsn) {
        let mut scopes = self.lock();
        let entry = scopes.entry(scope).or_insert(0);
        if lsn.0 > *entry {
            *entry = lsn.0;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ScopeId, u64>> {
        self.scopes.lock().expect("durable-lsn fake lock")
    }
}

impl DurableLsn for MemDurableLsn {
    fn durable_lsn(&self, scope: ScopeId) -> Lsn {
        Lsn(self.lock().get(&scope).copied().unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use crate::conformance::durable_lsn_view_suite;

    #[test]
    fn the_fake_passes_the_durable_lsn_view_suite() {
        let source = super::MemDurableLsn::new();
        let advance = |scope, lsn| source.advance(scope, lsn);
        durable_lsn_view_suite(&source, advance);
    }
}
