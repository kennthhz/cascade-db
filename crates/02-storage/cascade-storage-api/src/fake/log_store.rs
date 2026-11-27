//! [`MemLogStore`] (§4.10): one `LogSink` + `LogSource` + `LogScopes` in
//! memory. Appends are validated (epoch, contiguity, framing, idempotence)
//! and staged; durability advances inline when someone waits — "waiting is
//! what triggers a flush" (§4.3) — so the promises hold without a
//! scheduler. `crash()` discards everything not yet durable and
//! `restart()` hands back an instance carrying exactly the durable log;
//! the conformance suites drive create/crash/restart through these.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::Waker;

use cascade_types::{Epoch, Hlc, Lsn, LocalBoxFuture, ScopeId};
use cascade_wal_format::{self, walk};

use crate::error::{FrameError, StoreError};
use crate::key_provider::KeyRef;
use crate::types::HolderReport;
use crate::log_sink::LogSink;
use crate::log_source::{LogSource, LogSubscription};
use crate::scope_admin::LogScopes;
use crate::types::{AppendMeta, LogChunk, RetentionHolder};
use crate::OpCtx;

/// The largest chunk a subscription serves per `next` (§4.4 "about
/// `max_bytes`" in read_log; the fake uses 1 MiB, the frame cap).
const SUB_CHUNK_MAX: usize = 1024 * 1024;

#[derive(Clone)]
struct AppendSpan {
    start: Lsn,
    max_commit_ts: Option<Hlc>,
}

#[derive(Clone)]
struct ScopeState {
    epoch: Epoch,
    /// End of the contiguous durable prefix — always a group end (S16).
    durable: Lsn,
    /// The whole log from LSN 0; accepted-but-unflushed bytes live past
    /// `durable` and die at `crash`/fence.
    bytes: Vec<u8>,
    /// End of accepted bytes (contiguity frontier).
    appended: Lsn,
    waiters: Vec<(Lsn, Waker)>,
    holders: HashMap<RetentionHolder, Lsn>,
    appends: Vec<AppendSpan>,
    /// A flush failed in this epoch (§4.3 promise 7): refuse appends and
    /// fail waiters until `open_writer` reopens.
    failed: bool,
    /// `drop_scope` fenced the scope and holders still pin the log.
    dropped: bool,
}

impl Default for ScopeState {
    fn default() -> Self {
        ScopeState {
            epoch: Epoch::FIRST,
            durable: Lsn(0),
            bytes: Vec::new(),
            appended: Lsn(0),
            waiters: Vec::new(),
            holders: HashMap::new(),
            appends: Vec::new(),
            failed: false,
            dropped: false,
        }
    }
}

#[derive(Default, Clone)]
struct StoreInner {
    scopes: HashMap<ScopeId, ScopeState>,
    keys: HashMap<ScopeId, KeyRef>,
    /// One-shot: the next flush (inline or subscription-driven) fails and
    /// fails the epoch (§4.3 promise 7).
    inject_flush_failure: bool,
}

/// In-memory log store fake.
#[derive(Clone, Default)]
pub struct MemLogStore {
    inner: Arc<Mutex<StoreInner>>,
}

impl MemLogStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms a one-shot flush failure (§4.3 promise 7): the next waiter
    /// sees `Failed`, the epoch is refused appends until reopened.
    pub fn inject_flush_failure(&self) {
        self.inner.lock().expect("store lock").inject_flush_failure = true;
    }

    /// Crash: discard everything not yet durable, in place. Acknowledged
    /// bytes survive (§4.3 promise 6).
    pub fn crash(&self) {
        let mut inner = self.inner.lock().expect("store lock");
        for st in inner.scopes.values_mut() {
            st.bytes.truncate(st.durable.0 as usize);
            st.appended = st.durable;
            st.appends.retain(|s| s.start < st.durable);
            let waiters = std::mem::take(&mut st.waiters);
            for (_, w) in waiters {
                w.wake();
            }
        }
    }

    /// Restart: a fresh instance carrying exactly the durable log — the
    /// harness's create/crash/restart cycle (§4.10).
    pub fn restart(&self) -> MemLogStore {
        let inner = self.inner.lock().expect("store lock").clone();
        MemLogStore {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// The retention floor: min over holders, or 0 — with no holder
    /// registered nothing is discardable (§4.4 promise 4).
    fn floor_of(st: &ScopeState) -> Lsn {
        st.holders
            .values()
            .copied()
            .min()
            .unwrap_or(Lsn(0))
    }

    /// Serves whole records `[from, end)` — the shared read logic of
    /// `read_log` and subscription `next`. Caller holds the lock.
    fn serve(scope: ScopeId, st: &ScopeState, from: Lsn, max_bytes: usize) -> Result<LogChunk, StoreError> {
        let floor = Self::floor_of(st);
        if from < floor {
            return Err(StoreError::Truncated { floor });
        }
        if from >= st.durable {
            return Ok(LogChunk {
                start: from,
                bytes: Vec::new(),
                end: from,
                durable: st.durable,
            });
        }
        // `from` must be a record boundary the reader learned from the log
        // itself (§4.4 promise 3). The fake walks the log to check —
        // acceptable at fake scale; the real store consults its extent
        // index (§5.5).
        let mut at = 0usize;
        let mut boundary = false;
        while at < st.bytes.len() {
            if at as u64 == from.0 {
                boundary = true;
                break;
            }
            let f = walk(&st.bytes[at..]).map_err(|_| StoreError::Corruption {
                what: crate::CorruptionSite::LogFrame { scope, at: Lsn(at as u64) },
            })?;
            at += f.total;
        }
        if !boundary {
            return Err(StoreError::NotRecordBoundary { at: from });
        }
        // Whole records, at least one, never past durability (§4.4
        // promises 1–2). Durability is a group end, so the cut is clean.
        let mut end = at;
        while end < st.bytes.len() && end - at < max_bytes.max(1) {
            let f = walk(&st.bytes[end..]).map_err(|_| StoreError::Corruption {
                what: crate::CorruptionSite::LogFrame { scope, at: Lsn(end as u64) },
            })?;
            end += f.total;
            if end >= st.durable.0 as usize {
                end = st.durable.0 as usize;
                break;
            }
        }
        Ok(LogChunk {
            start: from,
            bytes: st.bytes[at..end].to_vec(),
            end: Lsn(end as u64),
            durable: st.durable,
        })
    }

    /// The inline flush: durability advances over every accepted byte —
    /// whose end is a group end by `append`'s framing rule (S16).
    fn flush(inner: &mut StoreInner, scope: ScopeId) -> Result<Lsn, StoreError> {
        let st = inner
            .scopes
            .get_mut(&scope)
            .ok_or(StoreError::UnknownScope(scope))?;
        if std::mem::take(&mut inner.inject_flush_failure) {
            st.failed = true;
            let waiters = std::mem::take(&mut st.waiters);
            for (_, w) in waiters {
                w.wake();
            }
            return Err(StoreError::Failed { scope });
        }
        st.durable = st.appended;
        let waiters = std::mem::take(&mut st.waiters);
        for (_, w) in waiters {
            w.wake();
        }
        Ok(st.durable)
    }

    /// Validates framing of `bytes` (§4.3 promise 4): whole v2/v3 records,
    /// CRCs, header LSN == position, header epoch == append epoch, and a
    /// group end on the last record (a v2 record is a group of one).
    fn check_framing(bytes: &[u8], start: Lsn, epoch: Epoch) -> Result<(), StoreError> {
        if bytes.is_empty() {
            return Err(StoreError::BadFrame { at: start, reason: FrameError::BadLength });
        }
        let mut at = 0usize;
        let mut last = None;
        while at < bytes.len() {
            let f = walk(&bytes[at..]).map_err(|e| StoreError::BadFrame {
                at: Lsn(start.0 + at as u64),
                reason: match e {
                    cascade_wal_format::WalkError::BadCrc => FrameError::BadCrc,
                    cascade_wal_format::WalkError::Truncated
                    | cascade_wal_format::WalkError::UnknownVersion(_) => FrameError::BadLength,
                    cascade_wal_format::WalkError::UnknownFlags(_) => FrameError::UnknownFrame,
                },
            })?;
            if f.lsn != start.0 + at as u64 {
                return Err(StoreError::BadFrame {
                    at: Lsn(start.0 + at as u64),
                    reason: FrameError::BadLsn,
                });
            }
            if f.epoch != epoch.0 {
                return Err(StoreError::BadFrame {
                    at: Lsn(start.0 + at as u64),
                    reason: FrameError::BadEpoch,
                });
            }
            at += f.total;
            last = Some(f);
        }
        if at != bytes.len() {
            return Err(StoreError::BadFrame {
                at: Lsn(start.0 + at as u64),
                reason: FrameError::BadLength,
            });
        }
        let last = last.expect("non-empty checked above");
        let ends_group = last.format_version == 3 && last.group_end
            || last.format_version == 2; // v2 records are groups of one
        if !ends_group {
            return Err(StoreError::BadFrame {
                at: Lsn(start.0 + at as u64),
                reason: FrameError::NotGroupEnd,
            });
        }
        Ok(())
    }
}

impl LogSink for MemLogStore {
    fn open_writer<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        epoch: Epoch,
    ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let st = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            if st.dropped || epoch < st.epoch {
                return Err(StoreError::Fenced { current: st.epoch });
            }
            if epoch == st.epoch {
                // A retry of the same open (§4.3 promise 1): the durable
                // LSN again, unchanged state.
                return Ok(st.durable);
            }
            // A new epoch discards accepted-but-unflushed bytes from older
            // epochs — they were never acknowledged — and reopens a failed
            // stream (§8.1 step 4).
            st.bytes.truncate(st.durable.0 as usize);
            st.appended = st.durable;
            st.appends.retain(|s| s.start < st.durable);
            st.epoch = epoch;
            st.failed = false;
            Ok(st.durable)
        })
    }

    fn append<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        epoch: Epoch,
        start: Lsn,
        bytes: &'a [u8],
        meta: AppendMeta,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let st = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            if st.dropped {
                return Err(StoreError::Fenced { current: st.epoch });
            }
            if st.failed {
                return Err(StoreError::Failed { scope });
            }
            if epoch < st.epoch {
                return Err(StoreError::Fenced { current: st.epoch });
            }
            // The epoch check happened before anything else mutable — a
            // re-send from a fenced epoch fails Fenced, not Conflict
            // (§4.3 promise 1's ordering rule).
            Self::check_framing(bytes, start, epoch)?;
            let len = bytes.len();
            if start > st.appended {
                return Err(StoreError::Gap { expected: st.appended });
            }
            if start < st.appended {
                // Overlap: identical bytes only (§4.3 promise 3).
                let overlap_end = (start.0 as usize + len).min(st.appended.0 as usize);
                let existing = &st.bytes[start.0 as usize..overlap_end];
                let given = &bytes[..overlap_end - start.0 as usize];
                if existing != given {
                    let at = existing
                        .iter()
                        .zip(given)
                        .position(|(a, b)| a != b)
                        .map(|i| Lsn(start.0 + i as u64))
                        .unwrap_or(start);
                    return Err(StoreError::Conflict { at });
                }
                if start.0 as usize + len <= st.appended.0 as usize {
                    return Ok(()); // pure re-send: no further effect
                }
            }
            // Accept the new suffix (contiguity holds: start == appended
            // here, or the overlap was identical and the suffix extends).
            st.bytes.extend_from_slice(bytes);
            st.appended = Lsn(start.0 + len as u64);
            st.appends.push(AppendSpan {
                start,
                max_commit_ts: meta.max_commit_ts,
            });
            Ok(())
        })
    }

    fn durable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        inner
            .scopes
            .get(&scope)
            .map(|st| st.durable)
            .ok_or(StoreError::UnknownScope(scope))
    }

    fn wait_durable<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>> {
        Box::pin(async move {
            loop {
                // Scoped so the guard never spans the await below.
                let flushed: Option<Lsn> = {
                    let mut inner = self.inner.lock().expect("store lock");
                    {
                        let st = inner
                            .scopes
                            .get_mut(&scope)
                            .ok_or(StoreError::UnknownScope(scope))?;
                        if st.failed {
                            return Err(StoreError::Failed { scope });
                        }
                        if st.durable >= lsn {
                            return Ok(st.durable);
                        }
                    }
                    // Waiting is what triggers a flush (§4.3); the fake's
                    // flush is inline, resolving on the calling task.
                    Some(Self::flush(&mut inner, scope)?)
                };
                if let Some(durable) = flushed {
                    if durable >= lsn {
                        return Ok(durable);
                    }
                }
                // Durable advanced but the caller waits for bytes not yet
                // appended — another task appends and flushes; park until a
                // wake (a single-task suite never reaches here).
                futures_lite::future::poll_fn(|cx| {
                    let mut inner = self.inner.lock().expect("store lock");
                    if let Some(st) = inner.scopes.get_mut(&scope) {
                        if st.durable >= lsn {
                            return std::task::Poll::Ready(());
                        }
                        st.waiters.push((lsn, cx.waker().clone()));
                    }
                    std::task::Poll::Pending
                })
                .await;
            }
        })
    }
}

impl LogScopes for MemLogStore {
    fn create_scope<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        initial: Epoch,
        key: KeyRef,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let initial = Epoch::issued(initial.0);
            let mut inner = self.inner.lock().expect("store lock");
            if inner.scopes.contains_key(&scope) {
                return Err(StoreError::ScopeExists(scope));
            }
            inner.scopes.insert(
                scope,
                ScopeState {
                    epoch: initial,
                    ..ScopeState::default()
                },
            );
            inner.keys.insert(scope, key);
            Ok(())
        })
    }

    fn drop_scope<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let st = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            // Fence every epoch at once: no open_writer and no append can
            // touch a dropped scope again.
            st.epoch = Epoch(u64::MAX);
            let waiters = std::mem::take(&mut st.waiters);
            for (_, w) in waiters {
                w.wake();
            }
            if st.holders.is_empty() {
                inner.scopes.remove(&scope);
                inner.keys.remove(&scope);
            } else {
                // Freed once no holder remains; `release_holder` completes
                // the removal.
                st.dropped = true;
            }
            Ok(())
        })
    }
}

impl LogSource for MemLogStore {
    fn read_log<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        from: Lsn,
        max_bytes: usize,
    ) -> LocalBoxFuture<'a, Result<LogChunk, StoreError>> {
        Box::pin(async move {
            let inner = self.inner.lock().expect("store lock");
            let st = inner.scopes.get(&scope).ok_or(StoreError::UnknownScope(scope))?;
            Self::serve(scope, st, from, max_bytes)
        })
    }

    fn subscribe(
        &self,
        _ctx: &OpCtx<'_>,
        scope: ScopeId,
        from: Lsn,
    ) -> Box<dyn LogSubscription> {
        Box::new(MemSubscription {
            store: self.clone(),
            scope,
            pos: from,
        })
    }

    fn set_retained_from<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let st = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            // A holder's value only moves forward, and a new holder
            // registers at or above the current floor (§4.4 promise 4) —
            // the fake clamps to keep the floor monotone. It also never
            // passes the durable LSN: a holder above it would make every
            // read at the durable point fail Truncated.
            let clamped = lsn.min(st.durable).max(Self::floor_of(st));
            st.holders.insert(holder, clamped);
            Ok(())
        })
    }

    fn release_holder<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let remove_now = {
                let st = inner
                    .scopes
                    .get_mut(&scope)
                    .ok_or(StoreError::UnknownScope(scope))?;
                st.holders.remove(&holder);
                st.dropped && st.holders.is_empty()
            };
            if remove_now {
                inner.scopes.remove(&scope);
                inner.keys.remove(&scope);
            }
            Ok(())
        })
    }

    fn retained_floor(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        inner
            .scopes
            .get(&scope)
            .map(Self::floor_of)
            .ok_or(StoreError::UnknownScope(scope))
    }

    fn retention_holders(&self, scope: ScopeId) -> Result<Vec<HolderReport>, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        let st = inner
            .scopes
            .get(&scope)
            .ok_or(StoreError::UnknownScope(scope))?;
        Ok(st.holders
            .iter()
            .map(|(holder, lsn)| HolderReport {
                holder: *holder,
                lsn: *lsn,
                lag: st.durable.0.saturating_sub(lsn.0),
            })
            .collect())
    }

    fn append_before_time<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        ts: Hlc,
    ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>> {
        Box::pin(async move {
            let inner = self.inner.lock().expect("store lock");
            let st = inner.scopes.get(&scope).ok_or(StoreError::UnknownScope(scope))?;
            // The earliest append whose highest commit timestamp is above
            // `ts`; commits at or below `ts` end before that append does
            // (§4.4). With none above, the log end is the boundary.
            let answer = st
                .appends
                .iter()
                .find(|s| s.max_commit_ts.is_some_and(|t| t > ts))
                .map(|s| s.start)
                .unwrap_or(st.durable);
            let floor = Self::floor_of(st);
            if answer < floor {
                return Err(StoreError::Truncated { floor });
            }
            Ok(answer)
        })
    }
}

/// A subscription cursor (§5.5): reads what is durable; when it reaches
/// the durable LSN it flushes inline (the fake's shape of "waits for the
/// next advance" — waiting triggers a flush) and only then parks.
struct MemSubscription {
    store: MemLogStore,
    scope: ScopeId,
    pos: Lsn,
}

impl LogSubscription for MemSubscription {
    fn next(&mut self) -> LocalBoxFuture<'_, Result<LogChunk, StoreError>> {
        Box::pin(async move {
            loop {
                // Scoped so the guard never spans the await below.
                let park: Option<()> = {
                    let mut inner = self.store.inner.lock().expect("store lock");
                    {
                        let st = inner
                            .scopes
                            .get_mut(&self.scope)
                            .ok_or(StoreError::UnknownScope(self.scope))?;
                        if st.failed {
                            return Err(StoreError::Failed { scope: self.scope });
                        }
                        if st.dropped {
                            // Dropped while parked: end, never park again.
                            return Err(StoreError::UnknownScope(self.scope));
                        }
                        if self.pos < st.durable {
                            let chunk =
                                MemLogStore::serve(self.scope, st, self.pos, SUB_CHUNK_MAX)?;
                            self.pos = chunk.end;
                            return Ok(chunk);
                        }
                    }
                    // At the durable LSN: waiting advances it (§5.5's
                    // "reads what is durable and, at the end, waits for the
                    // next advance").
                    MemLogStore::flush(&mut inner, self.scope)?;
                    if self.pos
                        < inner
                            .scopes
                            .get(&self.scope)
                            .map(|s| s.durable)
                            .unwrap_or(self.pos)
                    {
                        None // fresh bytes — serve them from the top
                    } else {
                        Some(()) // park until a background producer wakes us
                    }
                };
                match park {
                    None => continue,
                    Some(()) => futures_lite::future::poll_fn(|cx| {
                        let mut inner = self.store.inner.lock().expect("store lock");
                        if let Some(st) = inner.scopes.get_mut(&self.scope) {
                            // Dropped or failed: wake the loop head, which
                            // reports it. Re-queueing would park forever on
                            // a scope that will never progress.
                            if st.dropped || st.failed {
                                return std::task::Poll::Ready(());
                            }
                            if self.pos < st.durable {
                                return std::task::Poll::Ready(());
                            }
                            st.waiters.push((self.pos, cx.waker().clone()));
                        } else {
                            // The scope is gone: report it through the loop.
                            return std::task::Poll::Ready(());
                        }
                        std::task::Poll::Pending
                    })
                    .await,
                }
            }
        })
    }
}
