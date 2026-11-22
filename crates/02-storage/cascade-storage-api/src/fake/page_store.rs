//! [`MemPageStore`] (§4.10): `PageSource` + `PageScopes` over any
//! [`LogSource`], with the page routing and the redo apply both injected —
//! the contract crate never depends on `cascade-redo` (§4.10); tests pass
//! the real `cascade-redo::apply` (M7+) or a toy rule (conformance). The
//! fake materializes inline under its lock — pages change only under the
//! mutex, so a reader never sees a half-applied page (§6.4's rule, at
//! fake scale), and it applies nothing above the durable LSN it has seen
//! (§4.5 promise 2) while catching up on demand (promise 3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cascade_types::{Lsn, LocalBoxFuture, ScopeId};
use cascade_wal_format::decode;

use crate::error::StoreError;
use crate::log_source::LogSource;
use crate::page_source::PageSource;
use crate::scope_admin::PageScopes;
use crate::types::{PageImage, PageKey, ReaderId};
use crate::OpCtx;

/// Routes one record to the pages it changes (the WAL's per-page naming
/// lands with cascade-redo's contract, §12.3–12.4).
pub type PageRouter =
    Arc<dyn Fn(&cascade_wal_format::WalRecord) -> Vec<PageKey> + Send + Sync>;

/// Applies one record to one page image — idempotent by LSN (§12.4: a
/// record applies only if its start is at or above the page LSN).
pub type PageApply = Arc<dyn Fn(&mut [u8], &mut Lsn, &cascade_wal_format::WalRecord, Lsn) + Send + Sync>;

struct ScopePages {
    ingested: Lsn,
    durable_view: Lsn,
    pages: HashMap<PageKey, (Vec<u8>, Lsn)>, // image, page LSN (end, S17)
    read_floors: HashMap<ReaderId, Lsn>,
    dropped: bool,
}

impl Default for ScopePages {
    fn default() -> Self {
        ScopePages {
            ingested: Lsn(0),
            durable_view: Lsn(0),
            pages: HashMap::new(),
            read_floors: HashMap::new(),
            dropped: false,
        }
    }
}

/// The page store fake.
pub struct MemPageStore<S: LogSource + ?Sized> {
    source: Arc<S>,
    route: PageRouter,
    apply: PageApply,
    state: Mutex<HashMap<ScopeId, ScopePages>>,
}

impl<S: LogSource + ?Sized> MemPageStore<S> {
    /// Builds over a log source with injected routing and redo.
    pub fn new(source: Arc<S>, route: PageRouter, apply: PageApply) -> Self {
        MemPageStore {
            source,
            route,
            apply,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Pulls durable log until `ingested >= up_to` (§4.5 promise 3:
    /// catch-up on demand). Catch-up pulls with `read_log`, not a
    /// subscription — the subscription *waits* at the durable LSN (§4.4),
    /// while an on-demand miss must fail fast: an empty chunk means the
    /// request reached past the durable LSN, i.e. `BeyondDurable` (§4.5
    /// promise 2: nothing non-durable is applied). Durability is learned
    /// only from the chunks themselves (`LogChunk::durable`) — the page
    /// store never sees a sink.
    fn catch_up(&self, scope: ScopeId, up_to: Lsn) -> Result<(), StoreError> {
        let span = cascade_telemetry::Span::noop();
        let ctx = crate::fake::test_ctx(&span, 0);
        loop {
            let ingested = {
                let st = self.state.lock().expect("page store lock");
                let scope_state = st.get(&scope).ok_or(StoreError::UnknownScope(scope))?;
                scope_state.ingested
            };
            if ingested >= up_to {
                return Ok(());
            }
            let chunk = futures_lite::future::block_on(
                self.source.read_log(&ctx, scope, ingested, 1024 * 1024),
            )?;
            let mut st = self.state.lock().expect("page store lock");
            let scope_state = st.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            scope_state.durable_view = chunk.durable;
            if chunk.bytes.is_empty() {
                // At the durable LSN below `up_to` — the request reaches
                // past what the log has made durable.
                return Err(StoreError::BeyondDurable { durable: chunk.durable });
            }
            let mut at = 0usize;
            while at < chunk.bytes.len() {
                let d = decode(&chunk.bytes[at..]).map_err(|_| StoreError::Corruption {
                    what: crate::CorruptionSite::LogFrame { scope, at: chunk.start },
                })?;
                let record_lsn = Lsn(chunk.start.0 + at as u64);
                for page in (self.route)(&d.record) {
                    let entry = scope_state.pages.entry(page).or_insert_with(|| (vec![0u8; cascade_types::PAGE_SIZE], Lsn(0)));
                    (self.apply)(&mut entry.0, &mut entry.1, &d.record, record_lsn);
                }
                at += d.total;
                scope_state.ingested = Lsn(record_lsn.0 + d.total as u64);
            }
        }
    }
}

impl<S: LogSource + ?Sized> PageSource for MemPageStore<S> {
    fn get_page<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        page: PageKey,
        at_lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<PageImage, StoreError>> {
        Box::pin(async move {
            {
                let st = self.state.lock().expect("page store lock");
                let scope_state = st.get(&scope).ok_or(StoreError::UnknownScope(scope))?;
                if scope_state.dropped {
                    return Err(StoreError::UnknownScope(scope));
                }
            }
            // Catch-up first: durability is learned from the chunks, and a
            // request the log cannot yet make durable comes back
            // `BeyondDurable` from the catch-up itself (§4.5 promise 2).
            self.catch_up(scope, at_lsn)?;
            let st = self.state.lock().expect("page store lock");
            let scope_state = st.get(&scope).ok_or(StoreError::UnknownScope(scope))?;
            match scope_state.pages.get(&page) {
                None => Ok(PageImage {
                    buf: cascade_types::AlignedBuf::zeroed(cascade_types::PAGE_SIZE),
                    page_lsn: Lsn(0),
                }),
                Some((image, page_lsn)) => {
                    if *page_lsn > at_lsn {
                        // Without page versions the stored image is the only
                        // one; newer-than-requested is TooOld, never a
                        // silently-newer answer (§4.5 promise 5).
                        return Err(StoreError::TooOld { oldest: scope_state.ingested });
                    }
                    let mut buf = cascade_types::AlignedBuf::zeroed(cascade_types::PAGE_SIZE);
                    buf.as_mut_slice().copy_from_slice(image);
                    Ok(PageImage {
                        buf,
                        page_lsn: *page_lsn,
                    })
                }
            }
        })
    }

    fn materialized_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        let st = self.state.lock().expect("page store lock");
        st.get(&scope).map(|s| s.ingested).ok_or(StoreError::UnknownScope(scope))
    }

    fn oldest_readable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        // Without page versions: the newest image of each page is the only
        // one, so the oldest readable point is materialization itself
        // (§4.5, "why a leader never sees TooOld").
        self.materialized_lsn(scope)
    }

    fn set_read_floor<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        reader: ReaderId,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut st = self.state.lock().expect("page store lock");
            let scope_state = st.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            scope_state.read_floors.insert(reader, lsn);
            Ok(())
        })
    }
}

impl<S: LogSource + ?Sized> PageScopes for MemPageStore<S> {
    fn attach<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        lineage: Option<crate::types::Lineage>,
        _key: crate::key_provider::KeyRef,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if lineage.is_some() {
                // §4.6: the branch hook exists; v1 does not implement it.
                return Err(StoreError::Unsupported("branch lineage attach"));
            }
            let mut st = self.state.lock().expect("page store lock");
            if st.contains_key(&scope) {
                return Err(StoreError::ScopeExists(scope));
            }
            st.insert(scope, ScopePages::default());
            Ok(())
        })
    }

    fn detach<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut st = self.state.lock().expect("page store lock");
            let scope_state = st.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            scope_state.dropped = true;
            Ok(())
        })
    }
}
