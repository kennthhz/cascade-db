//! The page store (storage-layer-design.md §6): it ingests each attached
//! scope's log, applies the records to pages through `cascade-redo`,
//! writes pages to disk in the background through a doublewrite area, and
//! serves pages exactly as of an LSN. Commits never wait for it — the page
//! store sees only a [`LogSource`], never a sink.
//!
//! # M7 slice 1: what is pull-shaped
//!
//! The log store's first slice flushed inline on the calling task and left
//! the per-stream flusher task for a later slice. This slice makes the same
//! trade, for the same reason: the contract laws are identical either way,
//! and they are what the suites pin.
//!
//! | §6 says | This slice does |
//! | :--- | :--- |
//! | a subscription per scope, driven from a shared scheduler (§6.2) | **catch-up on demand**: `get_page` pulls the log with `read_log` until it reaches `at_lsn` (§4.5 promise 3). [`catch_up`] ingests without a read. |
//! | per-page redo chains, applied at read time (§6.2, §6.4) | pages are applied eagerly and kept in a bounded per-scope cache; clean pages are evicted and re-read from disk |
//! | a background materializer (§6.3) | [`flush`] — one doublewrite batch, called by the housekeeper, or by ingest itself when the dirty bound is reached |
//!
//! # The invariant everything rests on
//!
//! **On-disk pages are exactly the replay of the log below the manifest's
//! materialized LSN.** Not "at least": a page ahead of that point would
//! break routing on resume. A heap insert names a segment, not a page, so
//! where it goes is decided by the segment's open page — and an open page
//! that already contains the record would send it to the next page, again.
//!
//! Three things keep the invariant:
//!
//! 1. A batch is one snapshot — the dirty pages and the ingested LSN, taken
//!    under one lock hold, at a record boundary — and its header records
//!    that LSN.
//! 2. The manifest advances to the batch's LSN only after the batch's pages
//!    are synced in their home files.
//! 3. Recovery *finishes* an interrupted batch before serving anything:
//!    with a valid batch in the doublewrite area, it restores every page
//!    whose home copy is missing or damaged, syncs, and records the LSN.
//!
//! # The flush protocol (§6.3)
//!
//! ```text
//! write batch → pg-dblwrite          sync
//! write images + records → home      sync          (pages durable)
//! write manifest (materialized = batch LSN)        (restart point durable)
//! report the retention holder, if configured        (§4.4 promise 5)
//! clear pg-dblwrite                  sync
//! ```
//!
//! A crash anywhere leaves the previous consistent state, the batch
//! half-installed with its doublewrite copy intact, or the batch complete —
//! and recovery tells these apart by checksum, never by position.
//!
//! # When the disk fails
//!
//! A failed sync is permanent (S19). Any I/O error during a flush marks the
//! scope failed for the life of this store instance: later flushes return
//! that error instead of building on unknown state. Reads keep working
//! from what is in memory and from checksum-validated disk. A scope whose
//! ingest meets a record it cannot apply — a log already truncated below
//! what it needs, a malformed record — fails the same way, loudly.

mod gate;
mod io;
mod layout;
mod redo;

use std::collections::{HashMap, HashSet, VecDeque};
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, MutexGuard};

use cascade_storage_api::log_source::LogSource;
use cascade_storage_api::page_source::PageSource;
use cascade_storage_api::scope_admin::PageScopes;
use cascade_storage_api::types::{Lineage, PageImage, PageKey, ReaderId};
use cascade_storage_api::{
    BlockDevice, BlockHandle, CorruptionSite, FileName, FileId, KeyRef, OpCtx, RetentionHolder,
    StoreError, TenantContext,
};
use cascade_types::{
    AlignedBuf, DatabaseId, LocalBoxFuture, Lsn, PAGE_SIZE, PageNo, ScopeId, SegmentId,
};
use cascade_wal_format::{decode, DecodeError};

use crate::gate::Gate;
use crate::io::{
    home_is_current, io_err, open_cached, open_file, page_files, read_dw_batch, read_manifest,
    read_page, scan_high, write_bytes,
};
use crate::layout::{
    encode_dw_header, encode_manifest, encode_page_record, meta_file, page_file, parse_meta_file,
    parse_page_file, DwBatch, DwEntry, LayoutError, PageRecord, BLOCK, DBLWRITE_NAME,
    DW_MAX_HEADER_BLOCKS, MANIFEST_NAME, MANIFEST_TMP_NAME, PAGE_REC_LEN, PAGE_RECS_PER_BLOCK,
};

pub use redo::{HeapInsertRedo, PageData, PageSet, RedoPlan};

// ---------------------------------------------------------------------------
// Configuration and reports
// ---------------------------------------------------------------------------

/// Tunables. The defaults are starting values, to be tuned by benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Pages kept in memory per scope. Clean pages beyond it are evicted
    /// and re-read from disk; dirty pages are never evicted. Zero keeps
    /// no clean page — every read past a flush goes to disk.
    pub cache_pages: usize,
    /// Ingest flushes the scope when this many pages are dirty. It bounds
    /// the store's memory for unflushed pages, and must fit one batch.
    pub max_dirty_pages: usize,
    /// The doublewrite header region's size in 4 KiB blocks. Eight blocks
    /// describe 1363 pages; the batch is one snapshot, so this is also the
    /// largest flush.
    pub dw_header_blocks: usize,
    /// How much log one catch-up pull asks the log source for.
    pub read_chunk_bytes: usize,
    /// Whether to report the materialized LSN to the log store as this
    /// instance's retention value (§6.3). **Off by default**: once the
    /// page store is a holder, the log below its value reads as
    /// `Truncated`, and runtime recovery still reads the log from 0 to
    /// rebuild transaction state. Turn it on with that consumer in mind.
    pub report_retention: bool,
    /// The holder id written to the manifest when the store is first
    /// created; afterwards the manifest's own value is kept.
    pub instance: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cache_pages: 4096,
            max_dirty_pages: 1024,
            dw_header_blocks: 8,
            read_chunk_bytes: 1024 * 1024,
            report_retention: false,
            instance: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    DoublewriteHeaderOutOfRange,
    /// A dirty bound of zero would flush before every record.
    DirtyBoundBelowOne,
    DirtyPagesExceedDoublewrite,
    ReadChunkTooSmall,
}

impl ConfigError {
    fn message(self) -> &'static str {
        match self {
            ConfigError::DoublewriteHeaderOutOfRange => {
                "page store config: dw_header_blocks must be in 1..=64"
            }
            ConfigError::DirtyBoundBelowOne => {
                "page store config: max_dirty_pages must be at least 1"
            }
            ConfigError::DirtyPagesExceedDoublewrite => {
                "page store config: max_dirty_pages exceeds the doublewrite capacity"
            }
            ConfigError::ReadChunkTooSmall => {
                "page store config: read_chunk_bytes must be at least one 4 KiB block"
            }
        }
    }
}

impl Config {
    /// The doublewrite header region's size in bytes.
    pub fn dw_header_len(&self) -> usize {
        self.dw_header_blocks * BLOCK
    }

    /// How many pages one flush can write.
    pub fn dw_capacity(&self) -> usize {
        layout::dw_capacity(self.dw_header_len())
    }

    /// Checks the values `open` depends on.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // The upper bound is the most recovery will read from a batch
        // header, whatever a later configuration says.
        if self.dw_header_blocks == 0 || self.dw_header_blocks > DW_MAX_HEADER_BLOCKS {
            return Err(ConfigError::DoublewriteHeaderOutOfRange);
        }
        if self.max_dirty_pages == 0 {
            return Err(ConfigError::DirtyBoundBelowOne);
        }
        if self.max_dirty_pages > self.dw_capacity() {
            return Err(ConfigError::DirtyPagesExceedDoublewrite);
        }
        if self.read_chunk_bytes < BLOCK {
            return Err(ConfigError::ReadChunkTooSmall);
        }
        Ok(())
    }
}

/// What one [`PageStore::flush`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Nothing was dirty and the manifest already named the ingested LSN.
    Clean,
    /// Nothing was dirty, but the records since the last flush touched no
    /// page: the manifest now names the ingested LSN.
    Advanced {
        /// The LSN the manifest now records.
        materialized: Lsn,
    },
    /// One doublewrite batch landed.
    Flushed {
        /// Pages the batch wrote.
        pages: usize,
        /// The LSN the manifest now records.
        materialized: Lsn,
    },
}

/// What `open` found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenReport {
    /// A doublewrite batch had been interrupted by a crash and was
    /// completed.
    pub completed_batch: bool,
    /// Pages in that batch whose home copy was missing or damaged and was
    /// restored from the doublewrite copy.
    pub repaired_pages: usize,
    /// Page files of scopes the manifest no longer lists, removed.
    pub removed_orphan_files: usize,
}

/// A scope's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeStats {
    /// On-disk pages are the replay of the log below this LSN.
    pub materialized: Lsn,
    /// In-memory pages are the replay of the log below this LSN.
    pub ingested: Lsn,
    /// The scope's durable LSN, as last seen in a log read.
    pub durable: Option<Lsn>,
    pub cached_pages: usize,
    pub dirty_pages: usize,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// An error that does not go away inside this store instance. `StoreError`
/// owns an `io::Error` and cannot be cloned, so the cause is kept in a
/// form that can be returned again on every later call.
#[derive(Debug, Clone)]
enum Sticky {
    Truncated {
        floor: Lsn,
    },
    Corruption {
        what: CorruptionSite,
    },
    Unsupported(&'static str),
    Io(String),
}

impl Sticky {
    /// The errors that make a scope's *ingest* permanently unable to
    /// proceed: the data it is asked to apply is gone or wrong.
    fn data(e: &StoreError) -> Option<Sticky> {
        match e {
            StoreError::Truncated { floor } => Some(Sticky::Truncated { floor: *floor }),
            StoreError::Corruption { what } => Some(Sticky::Corruption { what: *what }),
            StoreError::Unsupported(what) => Some(Sticky::Unsupported(what)),
            _ => None,
        }
    }

    /// The errors that make a scope's *flush* unable to proceed: its disk
    /// refused, or the manifest cannot be written.
    fn flush(e: &StoreError) -> Option<Sticky> {
        match e {
            StoreError::Io(err) => Some(Sticky::Io(err.to_string())),
            other => Sticky::data(other),
        }
    }

    fn store_error(&self) -> StoreError {
        match self {
            Sticky::Truncated { floor } => StoreError::Truncated { floor: *floor },
            Sticky::Corruption { what } => StoreError::Corruption { what: *what },
            Sticky::Unsupported(what) => StoreError::Unsupported(what),
            Sticky::Io(msg) => StoreError::Io(std::io::Error::other(msg.clone())),
        }
    }
}

struct PageEntry {
    image: Vec<u8>,
    page_lsn: Lsn,
    dirty: bool,
}

struct ScopeState {
    key: KeyRef,
    /// On-disk pages are exactly the replay of the log below this.
    materialized: Lsn,
    /// In-memory pages are exactly the replay of the log below this.
    ingested: Lsn,
    durable: Option<Lsn>,
    pages: HashMap<PageKey, PageEntry>,
    /// Insertion order of `pages`' keys — the eviction queue. Dirty keys
    /// rotate to the back when they reach the front.
    order: VecDeque<PageKey>,
    dirty_count: usize,
    /// Each segment's highest page, in memory or on disk.
    high: HashMap<SegmentId, PageNo>,
    /// Each segment's number of page records on disk. A page at or past
    /// this count was never written.
    slots: HashMap<SegmentId, u32>,
    read_floors: HashMap<ReaderId, Lsn>,
    retention_reported: Option<Lsn>,
    ingest_failed: Option<Sticky>,
    flush_failed: Option<Sticky>,
}

impl ScopeState {
    fn fresh(key: KeyRef, materialized: Lsn) -> Self {
        ScopeState {
            key,
            materialized,
            ingested: materialized,
            durable: None,
            pages: HashMap::new(),
            order: VecDeque::new(),
            dirty_count: 0,
            high: HashMap::new(),
            slots: HashMap::new(),
            read_floors: HashMap::new(),
            retention_reported: None,
            ingest_failed: None,
            flush_failed: None,
        }
    }
}

#[derive(Default)]
struct StoreState {
    scopes: HashMap<ScopeId, ScopeState>,
}

/// One page of a doublewrite batch.
struct BatchEntry {
    key: PageKey,
    image: Vec<u8>,
    page_lsn: Lsn,
}

/// A flush's snapshot: everything is captured under one lock hold.
struct Snapshot {
    ingested: Lsn,
    materialized: Lsn,
    batch: Vec<BatchEntry>,
}

/// What listing and measuring the page files found.
struct Measured {
    /// Every page file, as `(scope, segment, is a record file)`.
    data_files: Vec<(ScopeId, SegmentId, bool)>,
    /// For each segment of an attached scope that has a page file: whether
    /// its image file and its record file are present.
    present: HashMap<(ScopeId, SegmentId), (bool, bool)>,
}

/// A change to make to the manifest, applied in memory only once it is
/// durable.
#[derive(Debug, Clone, Copy, Default)]
struct Edit {
    materialized: Option<(ScopeId, Lsn)>,
    exclude: Option<ScopeId>,
}

/// A batch's page records to write, grouped by the 4 KiB record block they
/// share: `(segment, block index)` → `(slot in the block, record)`.
type RecordWrites = HashMap<(SegmentId, usize), Vec<(usize, [u8; PAGE_REC_LEN])>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn crc(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

fn manifest_damage() -> StoreError {
    StoreError::Corruption {
        what: CorruptionSite::Manifest,
    }
}

fn page_damage(scope: ScopeId, page: PageKey) -> StoreError {
    StoreError::Corruption {
        what: CorruptionSite::Page { scope, page },
    }
}

fn log_damage(scope: ScopeId, at: Lsn) -> StoreError {
    StoreError::Corruption {
        what: CorruptionSite::LogFrame { scope, at },
    }
}

fn decode_error(e: DecodeError, scope: ScopeId, at: Lsn) -> StoreError {
    match e {
        // The record's CRC passed but its payload is not what its type
        // promises: only a buggy encoder produces that.
        DecodeError::Corruption => log_damage(scope, at),
        // A newer build wrote this. Named, not called damage — and not
        // skipped: the store cannot know whether the record touches a page.
        DecodeError::UnknownRecordType(_) => {
            StoreError::Unsupported("a WAL record type this page store does not know")
        }
        DecodeError::UnknownRecordVersion(_) => {
            StoreError::Unsupported("a WAL record version this page store does not read")
        }
    }
}

/// Drops clean pages past `cap`, oldest first. `keep` is never dropped —
/// the page a caller has just loaded. A queue of only dirty pages ends the
/// sweep: dirty pages are the unflushed log, and memory gives before they
/// do.
fn evict(s: &mut ScopeState, cap: usize, keep: Option<PageKey>) {
    let mut scanned = 0usize;
    while s.pages.len() > cap && scanned < s.order.len() {
        let Some(key) = s.order.pop_front() else {
            break;
        };
        match s.pages.get(&key) {
            Some(e) if e.dirty || Some(key) == keep => {
                s.order.push_back(key);
                scanned += 1;
            }
            Some(_) => {
                s.pages.remove(&key);
                scanned = 0;
            }
            None => scanned = 0,
        }
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// The page store over a [`BlockDevice`] and a [`LogSource`]. One per
/// process, shared by every core; created with [`PageStore::open`].
///
/// Three async gates, always taken in this order or not at all — which is
/// what rules out a cycle:
///
/// ```text
/// ingest gate (per scope) → flush gate → home gate
/// ```
///
/// Ingest takes the flush gate when backpressure makes it flush; scope
/// administration takes all three. A plain `flush` takes only the flush
/// gate and the home gate. The state lock is a `std::sync::Mutex` that is
/// never held across an `await`, so it is never held while a gate is.
pub struct PageStore<D: BlockDevice, P: RedoPlan> {
    dev: Arc<D>,
    log: Arc<dyn LogSource>,
    plan: P,
    config: Config,
    state: Mutex<StoreState>,
    /// Serializes flushes: the doublewrite area holds one batch, and a
    /// scope's batches must land in order (an older snapshot overwriting a
    /// newer one would regress its pages behind the manifest's back).
    flush_gate: Gate,
    /// Serializes the home-file phase of a flush against lazy page reads:
    /// a page record is 64 bytes inside a shared 4 KiB block, so a read of
    /// one page's record can meet another page's record being rewritten.
    home_gate: Gate,
    ingest_gates: Mutex<HashMap<ScopeId, Arc<Gate>>>,
    /// The holder id from the manifest.
    instance: u64,
    batch_seq: Mutex<u64>,
    open_report: OpenReport,
}

impl<D: BlockDevice, P: RedoPlan> PageStore<D, P> {
    /// Opens the store on `dev`: reads the manifest, finishes an
    /// interrupted doublewrite batch, checks every attached scope's page
    /// files against the manifest, and sweeps orphans.
    ///
    /// Refuses, with `Corruption`, rather than open onto state it cannot
    /// vouch for: page files without a manifest (the LSN they are a replay
    /// through is unknown), or a listed segment whose files have gone
    /// missing (the log that would rebuild them may be gone too).
    pub async fn open(
        dev: Arc<D>,
        log: Arc<dyn LogSource>,
        plan: P,
        config: Config,
    ) -> Result<Arc<Self>, StoreError> {
        config
            .validate()
            .map_err(|e| StoreError::Unsupported(e.message()))?;
        let mut store = Self {
            dev,
            log,
            plan,
            config,
            state: Mutex::new(StoreState::default()),
            flush_gate: Gate::default(),
            home_gate: Gate::default(),
            ingest_gates: Mutex::new(HashMap::new()),
            instance: 0,
            batch_seq: Mutex::new(0),
            open_report: OpenReport::default(),
        };
        let span = cascade_telemetry::Span::noop();
        let ctx = OpCtx::new(TenantContext::for_database(DatabaseId(0)), &span);
        store.recover(&ctx).await?;
        Ok(Arc::new(store))
    }

    /// What `open` found and did.
    pub fn open_report(&self) -> OpenReport {
        self.open_report
    }

    /// The configuration the store was opened with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// A scope's progress.
    pub fn stats(&self, scope: ScopeId) -> Result<ScopeStats, StoreError> {
        let st = lock(&self.state);
        let s = Self::scope(&st, scope)?;
        Ok(ScopeStats {
            materialized: s.materialized,
            ingested: s.ingested,
            durable: s.durable,
            cached_pages: s.pages.len(),
            dirty_pages: s.dirty_count,
        })
    }

    fn st(&self) -> MutexGuard<'_, StoreState> {
        lock(&self.state)
    }

    fn scope(st: &StoreState, scope: ScopeId) -> Result<&ScopeState, StoreError> {
        st.scopes
            .get(&scope)
            .ok_or(StoreError::UnknownScope(scope))
    }

    fn scope_mut(st: &mut StoreState, scope: ScopeId) -> Result<&mut ScopeState, StoreError> {
        st.scopes
            .get_mut(&scope)
            .ok_or(StoreError::UnknownScope(scope))
    }

    fn ingest_gate(&self, scope: ScopeId) -> Arc<Gate> {
        lock(&self.ingest_gates)
            .entry(scope)
            .or_insert_with(|| Arc::new(Gate::default()))
            .clone()
    }

    // -- ingest -------------------------------------------------------------

    /// Ingests the scope's durable log at least through `up_to`, without
    /// reading a page. For a housekeeper that wants the page store caught
    /// up before a read misses on it.
    pub async fn catch_up(&self, ctx: &OpCtx<'_>, scope: ScopeId, up_to: Lsn) -> Result<(), StoreError> {
        Self::scope(&self.st(), scope)?;
        self.ingest_to(ctx, scope, up_to).await
    }

    /// Ingests through `up_to` under the scope's ingest gate.
    ///
    /// The gate is what keeps a record from being applied twice: two cores
    /// catching the same scope up would otherwise both route the same
    /// record, and the second would find a fill cursor already past it.
    ///
    /// A scope whose ingest has failed refuses every call, even one below
    /// the LSN it reached: its pages are still consistent up to there, but
    /// a scope that half-works hides the failure it needs to report.
    async fn ingest_to(&self, ctx: &OpCtx<'_>, scope: ScopeId, up_to: Lsn) -> Result<(), StoreError> {
        {
            let st = self.st();
            let s = Self::scope(&st, scope)?;
            if let Some(failed) = &s.ingest_failed {
                return Err(failed.store_error());
            }
            if s.ingested >= up_to {
                // `ingested` only ever advances over records the log source
                // called durable, so `up_to` is at or below the durable LSN.
                return Ok(());
            }
        }
        let gate = self.ingest_gate(scope);
        let _ingesting = gate.lock().await;
        {
            let st = self.st();
            let s = Self::scope(&st, scope)?;
            if let Some(failed) = &s.ingest_failed {
                return Err(failed.store_error());
            }
            if s.ingested >= up_to {
                return Ok(());
            }
        }
        let outcome = self.ingest(ctx, scope, up_to).await;
        if let Err(e) = &outcome {
            if let Some(sticky) = Sticky::data(e) {
                let mut st = self.st();
                if let Some(s) = st.scopes.get_mut(&scope) {
                    s.ingest_failed.get_or_insert(sticky);
                }
            }
        }
        outcome
    }

    async fn ingest(&self, ctx: &OpCtx<'_>, scope: ScopeId, up_to: Lsn) -> Result<(), StoreError> {
        let read_max = self.config.read_chunk_bytes;
        loop {
            let from = Self::scope(&self.st(), scope)?.ingested;
            let chunk = self.log.read_log(ctx, scope, from, read_max).await?;
            // §4.4 promise 2: chunks are contiguous and gapless. A source
            // that breaks it has handed over bytes of unknown provenance.
            if chunk.start != from || chunk.end.0 != chunk.start.0 + chunk.bytes.len() as u64 {
                return Err(log_damage(scope, from));
            }
            if chunk.end > chunk.durable {
                // §4.4 promise 1: only durable records are ever served.
                // Applying a byte past the durable LSN would put a record
                // on a page that a crash may not have kept.
                return Err(log_damage(scope, chunk.durable));
            }
            Self::scope_mut(&mut self.st(), scope)?.durable = Some(chunk.durable);
            if chunk.bytes.is_empty() {
                if up_to > chunk.durable {
                    // Nothing above the durable LSN is ever applied
                    // (§4.5 promise 2).
                    return Err(StoreError::BeyondDurable {
                        durable: chunk.durable,
                    });
                }
                // `from` is below the durable LSN, yet nothing is there.
                return Err(log_damage(scope, from));
            }

            let mut at = 0usize;
            while at < chunk.bytes.len() {
                let start = Lsn(from.0 + at as u64);
                // The chunk can run past `up_to`: the log source serves
                // whole records up to *its* durable LSN, which may be ahead
                // of what was asked for. A record that begins at or past
                // `up_to` is not this request's — everything below `up_to`
                // is applied, and the scope stops exactly there.
                if start >= up_to {
                    return Ok(());
                }
                let decoded = match decode(&chunk.bytes[at..]) {
                    Ok(d) => d,
                    Err(e) => return Err(decode_error(e, scope, start)),
                };
                let end = Lsn(start.0 + decoded.total as u64);
                if end > up_to {
                    // `up_to` falls inside this record. Nothing of it is
                    // applied, and `ingested` stays at its start: the
                    // scope is left on a record boundary.
                    return Err(StoreError::NotRecordBoundary { at: up_to });
                }
                // Backpressure, at a record boundary — so the flush takes
                // a snapshot of a consistent state.
                if Self::scope(&self.st(), scope)?.dirty_count >= self.config.max_dirty_pages {
                    self.flush(ctx, scope).await?;
                }
                let mut pages = Pages {
                    store: self,
                    ctx,
                    scope,
                };
                self.plan
                    .apply(&mut pages, &decoded.record, start)
                    .await?;
                Self::scope_mut(&mut self.st(), scope)?.ingested = end;
                at += decoded.total;
            }
            if Self::scope(&self.st(), scope)?.ingested >= up_to {
                return Ok(());
            }
        }
    }

    /// The scope's page, in memory if it is cached, otherwise from disk.
    async fn page_data(
        &self,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
        key: PageKey,
    ) -> Result<Option<PageData>, StoreError> {
        if let Some(cached) = self.cached(scope, key)? {
            return Ok(Some(cached));
        }
        self.load_from_disk(ctx, scope, key).await
    }

    fn cached(&self, scope: ScopeId, key: PageKey) -> Result<Option<PageData>, StoreError> {
        let st = self.st();
        Ok(Self::scope(&st, scope)?
            .pages
            .get(&key)
            .map(|e| PageData {
                image: e.image.clone(),
                page_lsn: e.page_lsn,
            }))
    }

    /// Reads a page from disk and caches it, if it was ever written.
    async fn load_from_disk(
        &self,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
        key: PageKey,
    ) -> Result<Option<PageData>, StoreError> {
        {
            let st = self.st();
            let s = Self::scope(&st, scope)?;
            if s.pages.contains_key(&key) || s.slots.get(&key.segment).copied().unwrap_or(0) <= key.page_no
            {
                // Cached, or past every record the sidecar holds: never
                // written.
                return Ok(s.pages.get(&key).map(|e| PageData {
                    image: e.image.clone(),
                    page_lsn: e.page_lsn,
                }));
            }
        }
        let _home = self.home_gate.lock().await;
        if let Some(cached) = self.cached(scope, key)? {
            return Ok(Some(cached));
        }
        let handle = self.dev.local();
        let Some(loaded) = read_page(&*handle, ctx, scope, key).await? else {
            return Ok(None);
        };
        let mut st = self.st();
        let s = Self::scope_mut(&mut st, scope)?;
        // A reader that raced another core's load must not overwrite what
        // that core cached: the disk copy is the older one.
        if let Entry::Vacant(entry) = s.pages.entry(key) {
            entry.insert(PageEntry {
                image: loaded.image.clone(),
                page_lsn: loaded.page_lsn,
                dirty: false,
            });
            s.order.push_back(key);
            evict(s, self.config.cache_pages, Some(key));
        }
        Ok(Some(loaded))
    }

    /// Stores a page the plan changed, dirty.
    fn insert_dirty(&self, scope: ScopeId, key: PageKey, page: PageData) -> Result<(), StoreError> {
        if page.image.len() != PAGE_SIZE {
            return Err(StoreError::Unsupported(
                "a page image must be PAGE_SIZE bytes",
            ));
        }
        let mut st = self.st();
        let s = Self::scope_mut(&mut st, scope)?;
        match s.pages.get_mut(&key) {
            Some(e) => {
                e.image = page.image;
                e.page_lsn = page.page_lsn;
                if !e.dirty {
                    e.dirty = true;
                    s.dirty_count += 1;
                }
            }
            None => {
                s.pages.insert(
                    key,
                    PageEntry {
                        image: page.image,
                        page_lsn: page.page_lsn,
                        dirty: true,
                    },
                );
                s.dirty_count += 1;
                s.order.push_back(key);
            }
        }
        if page.page_lsn > Lsn(0) {
            s.high
                .entry(key.segment)
                .and_modify(|h| *h = (*h).max(key.page_no))
                .or_insert(key.page_no);
        }
        Ok(())
    }

    // -- reads --------------------------------------------------------------

    async fn get_page_inner(
        &self,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
        key: PageKey,
        at_lsn: Lsn,
    ) -> Result<PageImage, StoreError> {
        Self::scope(&self.st(), scope)?;
        self.ingest_to(ctx, scope, at_lsn).await?;
        let data = self.page_data(ctx, scope, key).await?;
        let st = self.st();
        let s = Self::scope(&st, scope)?;
        let Some(data) = data else {
            // §4.5 promise 4: a page no record has touched is zeros.
            let mut buf = AlignedBuf::zeroed(PAGE_SIZE);
            buf.as_mut_slice().fill(0);
            return Ok(PageImage {
                buf,
                page_lsn: Lsn(0),
            });
        };
        if data.page_lsn > at_lsn {
            // The only version held is newer than the one asked for.
            // Without page versions that is `TooOld` — never a silently
            // newer answer (§4.5 promise 5).
            return Err(StoreError::TooOld { oldest: s.ingested });
        }
        let mut buf = AlignedBuf::zeroed(PAGE_SIZE);
        buf.as_mut_slice().copy_from_slice(&data.image);
        Ok(PageImage {
            buf,
            page_lsn: data.page_lsn,
        })
    }

    // -- flush --------------------------------------------------------------

    /// Writes the scope's dirty pages to disk through one doublewrite
    /// batch, records the LSN they reflect, and — if configured — reports
    /// it to the log store. Ingest calls this itself when the dirty bound
    /// is reached, so a store nobody flushes still bounds its memory.
    pub async fn flush(&self, ctx: &OpCtx<'_>, scope: ScopeId) -> Result<FlushOutcome, StoreError> {
        {
            let st = self.st();
            let s = Self::scope(&st, scope)?;
            if let Some(failed) = &s.flush_failed {
                return Err(failed.store_error());
            }
        }
        let _flushing = self.flush_gate.lock().await;

        let snapshot = self.snapshot(scope)?;
        if let Some(failed) = &Self::scope(&self.st(), scope)?.flush_failed {
            // A flush that held the gate before this one failed.
            return Err(failed.store_error());
        }
        if snapshot.batch.is_empty() {
            if snapshot.materialized == snapshot.ingested {
                self.report_retention(ctx, scope).await?;
                return Ok(FlushOutcome::Clean);
            }
            // Nothing is dirty, so every page's last change is on disk and
            // no page holds a record at or above the ingested LSN: the
            // on-disk pages are the replay of the log below it.
            let handle = self.dev.local();
            self.persist_manifest(
                ctx,
                &*handle,
                Edit {
                    materialized: Some((scope, snapshot.ingested)),
                    exclude: None,
                },
            )
            .await?;
            self.report_retention(ctx, scope).await?;
            return Ok(FlushOutcome::Advanced {
                materialized: snapshot.ingested,
            });
        }
        if snapshot.batch.len() > self.config.dw_capacity() {
            // Unreachable while ingest keeps its dirty bound (it flushes at
            // `max_dirty_pages`, which `Config::validate` keeps within one
            // batch). Splitting a snapshot would put pages on disk that are
            // ahead of any LSN the manifest could honestly record, so this
            // refuses instead.
            let sticky = Sticky::Unsupported(
                "a flush snapshot exceeds the doublewrite capacity: the dirty bound is broken",
            );
            let e = sticky.store_error();
            self.fail_flush(scope, sticky);
            return Err(e);
        }
        let handle = self.dev.local();
        if let Err(e) = self
            .write_batch(ctx, &*handle, scope, snapshot.ingested, &snapshot.batch)
            .await
        {
            if let Some(sticky) = Sticky::flush(&e) {
                self.fail_flush(scope, sticky);
            }
            return Err(e);
        }
        self.report_retention(ctx, scope).await?;
        Ok(FlushOutcome::Flushed {
            pages: snapshot.batch.len(),
            materialized: snapshot.ingested,
        })
    }

    fn fail_flush(&self, scope: ScopeId, sticky: Sticky) {
        let mut st = self.st();
        if let Some(s) = st.scopes.get_mut(&scope) {
            s.flush_failed.get_or_insert(sticky);
        }
    }

    /// The scope's dirty pages and its ingested LSN, under one lock hold.
    fn snapshot(&self, scope: ScopeId) -> Result<Snapshot, StoreError> {
        let st = self.st();
        let s = Self::scope(&st, scope)?;
        let mut keys: Vec<PageKey> = s
            .pages
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(k, _)| *k)
            .collect();
        keys.sort_unstable_by_key(|k| (k.segment.0, k.page_no));
        let batch = keys
            .into_iter()
            .filter_map(|k| {
                s.pages.get(&k).map(|e| BatchEntry {
                    key: k,
                    image: e.image.clone(),
                    page_lsn: e.page_lsn,
                })
            })
            .collect();
        Ok(Snapshot {
            ingested: s.ingested,
            materialized: s.materialized,
            batch,
        })
    }

    /// One batch: doublewrite, sync, home files, sync, manifest, clear.
    async fn write_batch(
        &self,
        ctx: &OpCtx<'_>,
        handle: &dyn BlockHandle,
        scope: ScopeId,
        ingested: Lsn,
        batch: &[BatchEntry],
    ) -> Result<(), StoreError> {
        let header_len = self.config.dw_header_len();
        let seq = {
            let mut n = lock(&self.batch_seq);
            *n += 1;
            *n
        };
        let plan = DwBatch {
            scope,
            seq,
            ingested,
            entries: batch
                .iter()
                .map(|e| DwEntry {
                    segment: e.key.segment,
                    page_no: e.key.page_no,
                    page_lsn: e.page_lsn,
                    image_crc: crc(&e.image),
                })
                .collect(),
        };
        let mut bytes = Vec::with_capacity(header_len + batch.len() * PAGE_SIZE);
        bytes.extend_from_slice(&encode_dw_header(&plan, header_len));
        for e in batch {
            bytes.extend_from_slice(&e.image);
        }
        let dw_name = FileName(DBLWRITE_NAME.into());
        // A doublewrite file created by this flush needs its directory
        // entry durable before the batch's sync can vouch for it: the
        // device's rule is that `sync_namespace` makes a create durable
        // (§4.8). The in-memory fake keeps unsynced creations, so only
        // this reasoning stands behind it.
        let created = open_file(handle, ctx, &dw_name, false).await?.is_none();
        let dw = handle.open(ctx, &dw_name, true).await.map_err(io_err)?;
        write_bytes(handle, ctx, dw, 0, &bytes).await?;
        handle.sync(ctx, dw).await.map_err(io_err)?;
        if created {
            handle.sync_namespace().await.map_err(io_err)?;
        }

        self.install(ctx, handle, scope, batch, false).await?;
        {
            let mut st = self.st();
            let s = Self::scope_mut(&mut st, scope)?;
            for e in batch {
                if let Some(entry) = s.pages.get_mut(&e.key) {
                    // A page changed since the snapshot has a new page LSN
                    // and stays dirty; the next batch carries it.
                    if entry.dirty && entry.page_lsn == e.page_lsn {
                        entry.dirty = false;
                        s.dirty_count -= 1;
                    }
                }
            }
            // Pages became clean here; the cache bound applies to them at
            // once, not at the next insert.
            evict(s, self.config.cache_pages, None);
        }
        self.persist_manifest(
            ctx,
            handle,
            Edit {
                materialized: Some((scope, ingested)),
                exclude: None,
            },
        )
        .await?;
        self.clear_doublewrite(ctx, handle).await
    }

    /// Writes a batch's pages to their home files: each image, and its
    /// record. Returns the number of pages written. The caller must not
    /// hold the home gate; this takes it.
    ///
    /// With `repair`, a page whose home copy already holds exactly this
    /// image is skipped — recovery's path, where most of a batch has
    /// usually landed. Any read error counts as "not current": the page is
    /// rewritten, and if that fails the failure is the real one.
    async fn install(
        &self,
        ctx: &OpCtx<'_>,
        handle: &dyn BlockHandle,
        scope: ScopeId,
        batch: &[BatchEntry],
        repair: bool,
    ) -> Result<usize, StoreError> {
        let _home = self.home_gate.lock().await;
        let mut files: HashMap<FileName, FileId> = HashMap::new();
        let mut records: RecordWrites = HashMap::new();
        let mut present: HashMap<SegmentId, u32> = HashMap::new();
        let mut written = 0usize;
        for e in batch {
            if repair && home_is_current(handle, ctx, scope, e).await {
                continue;
            }
            let f = open_cached(
                handle,
                ctx,
                &mut files,
                page_file(scope, e.key.segment),
                true,
            )
            .await?;
            write_bytes(
                handle,
                ctx,
                f,
                e.key.page_no as u64 * PAGE_SIZE as u64,
                &e.image,
            )
            .await?;
            let record = encode_page_record(&PageRecord::seal(&e.image, e.page_lsn));
            let block = e.key.page_no as usize / PAGE_RECS_PER_BLOCK;
            records
                .entry((e.key.segment, block))
                .or_default()
                .push((e.key.page_no as usize % PAGE_RECS_PER_BLOCK, record));
            let count = present.entry(e.key.segment).or_insert(0);
            *count = (*count).max(e.key.page_no.saturating_add(1));
            written += 1;
        }
        for ((segment, block), list) in records {
            let f = open_cached(handle, ctx, &mut files, meta_file(scope, segment), true).await?;
            // A record is 64 bytes inside a shared block: read the block,
            // patch this batch's records into it, write it back. Flushes
            // are serialized and lazy reads take the home gate, so no other
            // writer or reader meets this read-modify-write.
            let off = block as u64 * BLOCK as u64;
            let mut blk = AlignedBuf::zeroed(BLOCK);
            if handle.len(f).await.map_err(io_err)? > off {
                blk = handle.read_at(ctx, f, off, blk).await.map_err(io_err)?;
            }
            for (slot, record) in list {
                blk.as_mut_slice()[slot * PAGE_REC_LEN..(slot + 1) * PAGE_REC_LEN]
                    .copy_from_slice(&record);
            }
            handle.write_at(ctx, f, off, blk).await.map_err(io_err)?;
        }
        for f in files.values() {
            handle.sync(ctx, *f).await.map_err(io_err)?;
        }
        // A file created by this batch needs its directory entry durable
        // too (the device's crash rule, §4.8).
        handle.sync_namespace().await.map_err(io_err)?;
        {
            let mut st = self.st();
            let s = Self::scope_mut(&mut st, scope)?;
            for (segment, count) in present {
                let slots = s.slots.entry(segment).or_insert(0);
                *slots = (*slots).max(count);
            }
        }
        Ok(written)
    }

    async fn persist_manifest(
        &self,
        ctx: &OpCtx<'_>,
        handle: &dyn BlockHandle,
        edit: Edit,
    ) -> Result<(), StoreError> {
        let block = {
            let st = self.st();
            let mut scopes: Vec<layout::ManifestScope> = st
                .scopes
                .iter()
                .filter(|(scope, _)| Some(**scope) != edit.exclude)
                .map(|(scope, s)| layout::ManifestScope {
                    scope: *scope,
                    key_id: s.key.key_id,
                    materialized: match edit.materialized {
                        Some((target, lsn)) if target == *scope => lsn,
                        _ => s.materialized,
                    },
                    segments: {
                        let mut v: Vec<SegmentId> = s.slots.keys().copied().collect();
                        v.sort_unstable();
                        v
                    },
                })
                .collect();
            scopes.sort_unstable_by_key(|s| s.scope);
            let manifest = layout::Manifest {
                instance: self.instance,
                scopes,
            };
            encode_manifest(&manifest).map_err(|e| match e {
                LayoutError::TooLarge => StoreError::Unsupported(
                    "the page store manifest is full: one 4 KiB block of scopes and segments",
                ),
                LayoutError::Corrupt | LayoutError::UnknownVersion(_) => manifest_damage(),
            })?
        };
        // Write, sync, rename, sync namespace: the manifest is either the
        // old one or the new one, never a mix (§4.8).
        let tmp = FileName(MANIFEST_TMP_NAME.into());
        let f = handle.open(ctx, &tmp, true).await.map_err(io_err)?;
        write_bytes(handle, ctx, f, 0, &block).await?;
        handle.sync(ctx, f).await.map_err(io_err)?;
        handle
            .rename(ctx, &tmp, &FileName(MANIFEST_NAME.into()))
            .await
            .map_err(io_err)?;
        handle.sync_namespace().await.map_err(io_err)?;
        // Durable: now it is true in memory too.
        let mut st = self.st();
        if let Some(scope) = edit.exclude {
            st.scopes.remove(&scope);
        }
        if let Some((scope, lsn)) = edit.materialized {
            if let Some(s) = st.scopes.get_mut(&scope) {
                s.materialized = lsn;
            }
        }
        Ok(())
    }

    async fn clear_doublewrite(&self, ctx: &OpCtx<'_>, handle: &dyn BlockHandle) -> Result<(), StoreError> {
        let header_len = self.config.dw_header_len();
        let dw = handle
            .open(ctx, &FileName(DBLWRITE_NAME.into()), true)
            .await
            .map_err(io_err)?;
        write_bytes(handle, ctx, dw, 0, &vec![0u8; header_len]).await?;
        handle.sync(ctx, dw).await.map_err(io_err)
    }

    /// Reports the materialized LSN as this instance's retention value —
    /// only after the manifest that records it is durable (§4.4 promise 5).
    async fn report_retention(&self, ctx: &OpCtx<'_>, scope: ScopeId) -> Result<(), StoreError> {
        if !self.config.report_retention {
            return Ok(());
        }
        let reported = {
            let st = self.st();
            let s = Self::scope(&st, scope)?;
            (s.materialized, s.retention_reported)
        };
        if reported.1 == Some(reported.0) {
            return Ok(());
        }
        self.log
            .set_retained_from(
                ctx,
                scope,
                RetentionHolder::PageStore {
                    instance: self.instance,
                },
                reported.0,
            )
            .await?;
        let mut st = self.st();
        if let Some(s) = st.scopes.get_mut(&scope) {
            s.retention_reported = Some(reported.0);
        }
        Ok(())
    }

    // -- scope administration ----------------------------------------------

    async fn attach_scope(
        &self,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
        lineage: Option<Lineage>,
        key: KeyRef,
    ) -> Result<(), StoreError> {
        if lineage.is_some() {
            // §4.6: the branch hook exists; v1 does not implement it.
            return Err(StoreError::Unsupported("branch lineage attach"));
        }
        let gate = self.ingest_gate(scope);
        let _ingesting = gate.lock().await;
        let _flushing = self.flush_gate.lock().await;
        if self.st().scopes.contains_key(&scope) {
            return Err(StoreError::ScopeExists(scope));
        }
        let handle = self.dev.local();
        // S18: a scope id is never reused, so any page file still carrying
        // it is a leftover (a detach interrupted before its manifest
        // write) and can never be anyone's pages.
        self.remove_scope_files(&*handle, ctx, scope).await?;
        {
            let mut st = self.st();
            st.scopes.insert(scope, ScopeState::fresh(key, Lsn(0)));
        }
        if let Err(e) = self.persist_manifest(ctx, &*handle, Edit::default()).await {
            // Not attached after all: the scope and its gate are both
            // forgotten, so a retry starts clean.
            self.st().scopes.remove(&scope);
            lock(&self.ingest_gates).remove(&scope);
            return Err(e);
        }
        Ok(())
    }

    /// Stops materializing the scope, releases its log retention if it
    /// reported one, and deletes its page files. Scope ids are never
    /// reused (S18), so nothing can read those pages again.
    ///
    /// The order is what a crash between any two steps needs: the holder
    /// goes first (a holder left behind would pin the log forever), the
    /// manifest next (a scope the manifest no longer lists is an orphan
    /// whose files `open` sweeps), the files last.
    async fn detach_scope(&self, ctx: &OpCtx<'_>, scope: ScopeId) -> Result<(), StoreError> {
        let gate = self.ingest_gate(scope);
        let _ingesting = gate.lock().await;
        let _flushing = self.flush_gate.lock().await;
        let _home = self.home_gate.lock().await;
        let reported = Self::scope(&self.st(), scope)?.retention_reported;
        if self.config.report_retention && reported.is_some() {
            let holder = RetentionHolder::PageStore {
                instance: self.instance,
            };
            match self.log.release_holder(ctx, scope, holder).await {
                Ok(()) => {}
                // The log scope is already gone: there is no holder left
                // to release. Anything else is a real refusal.
                Err(StoreError::UnknownScope(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let handle = self.dev.local();
        self.persist_manifest(
            ctx,
            &*handle,
            Edit {
                materialized: None,
                exclude: Some(scope),
            },
        )
        .await?;
        // The scope is gone from the manifest whatever the device does
        // next, so its gate goes with it now: a failed file removal must
        // not leave the entry behind. The files are removed last — `open`
        // sweeps them as orphans if that fails.
        lock(&self.ingest_gates).remove(&scope);
        self.remove_scope_files(&*handle, ctx, scope).await?;
        Ok(())
    }

    async fn remove_scope_files(
        &self,
        handle: &dyn BlockHandle,
        ctx: &OpCtx<'_>,
        scope: ScopeId,
    ) -> Result<usize, StoreError> {
        let names = handle.list().await.map_err(io_err)?;
        let mut removed = 0usize;
        for name in names {
            let mine = parse_meta_file(&name.0)
                .or_else(|| parse_page_file(&name.0))
                .is_some_and(|(owner, _)| owner == scope);
            if mine {
                handle.remove(ctx, &name).await.map_err(io_err)?;
                removed += 1;
            }
        }
        if removed > 0 {
            handle.sync_namespace().await.map_err(io_err)?;
        }
        Ok(removed)
    }

    // -- recovery -----------------------------------------------------------

    /// Lists the page files and measures them: for each segment of an
    /// attached scope, how many page records its record file holds. The
    /// count is in whole 4 KiB blocks — a page past the last record in the
    /// final block reads as never written, which is the right answer — and
    /// it is what later lets a read decide whether a page can be on disk.
    async fn measure(
        &self,
        handle: &dyn BlockHandle,
        ctx: &OpCtx<'_>,
    ) -> Result<Measured, StoreError> {
        let data_files = page_files(&handle.list().await.map_err(io_err)?);
        let mut present: HashMap<(ScopeId, SegmentId), (bool, bool)> = HashMap::new();
        for (scope, segment, is_meta) in &data_files {
            if self.st().scopes.contains_key(scope) {
                let both = present.entry((*scope, *segment)).or_insert((false, false));
                if *is_meta {
                    both.1 = true;
                } else {
                    both.0 = true;
                }
            }
        }
        let mut lengths = Vec::new();
        for (scope, segment) in present.keys() {
            let Some(f) = open_file(handle, ctx, &meta_file(*scope, *segment), false).await?
            else {
                continue;
            };
            let len = handle.len(f).await.map_err(io_err)?;
            if len % PAGE_REC_LEN as u64 != 0 {
                return Err(page_damage(
                    *scope,
                    PageKey {
                        segment: *segment,
                        page_no: 0,
                    },
                ));
            }
            let count = u32::try_from(len / PAGE_REC_LEN as u64).unwrap_or(u32::MAX);
            lengths.push((*scope, *segment, count));
        }
        let mut st = self.st();
        for (scope, segment, count) in lengths {
            let slots = Self::scope_mut(&mut st, scope)?
                .slots
                .entry(segment)
                .or_insert(0);
            *slots = (*slots).max(count);
        }
        Ok(Measured {
            data_files,
            present,
        })
    }

    async fn recover(&mut self, ctx: &OpCtx<'_>) -> Result<(), StoreError> {
        let handle = self.dev.local();
        let data_files = page_files(&handle.list().await.map_err(io_err)?);

        // 1. The restart point.
        let manifest = match read_manifest(&*handle, ctx).await? {
            Some(m) => m,
            // Pages with no manifest: the LSN they are a replay through is
            // unknown, and ingesting from 0 onto them would route every
            // insert by an open page that already holds it.
            None if !data_files.is_empty() => return Err(manifest_damage()),
            None => layout::Manifest {
                instance: self.config.instance.unwrap_or(1),
                scopes: Vec::new(),
            },
        };
        self.instance = manifest.instance;
        {
            let mut st = self.st();
            for s in &manifest.scopes {
                let mut state = ScopeState::fresh(KeyRef { key_id: s.key_id }, s.materialized);
                for segment in &s.segments {
                    // Known to exist; its record count is measured below.
                    state.slots.insert(*segment, 0);
                }
                st.scopes.insert(s.scope, state);
            }
        }

        // What the durable manifest lists. A segment with pages on disk
        // that it does not name is not covered by any materialized LSN.
        let listed: HashSet<(ScopeId, SegmentId)> = {
            let st = self.st();
            st.scopes
                .iter()
                .flat_map(|(scope, s)| s.slots.keys().map(move |segment| (*scope, *segment)))
                .collect()
        };
        let mut rewrote_manifest = false;

        // 2. Finish an interrupted batch, before anything is served.
        let mut repaired = 0usize;
        let mut completed = false;
        if let Some((scope, through, batch)) = read_dw_batch(&*handle, ctx).await? {
            let attached = self.st().scopes.contains_key(&scope);
            let materialized = Self::scope(&self.st(), scope)
                .map(|s| s.materialized)
                .unwrap_or(Lsn(0));
            // A batch older than the manifest is not one this restart can
            // learn anything from; it is simply overwritten by the next.
            if attached && through >= materialized {
                repaired = self.install(ctx, &*handle, scope, &batch, true).await?;
                // The manifest names segments from the store's bookkeeping,
                // and `install` recorded only the segments of the pages it
                // rewrote. The files are measured first: a segment whose
                // batch pages all survived the crash would otherwise be
                // left out of the manifest it has to appear in.
                self.measure(&*handle, ctx).await?;
                if through > materialized {
                    self.persist_manifest(
                        ctx,
                        &*handle,
                        Edit {
                            materialized: Some((scope, through)),
                            exclude: None,
                        },
                    )
                    .await?;
                    rewrote_manifest = true;
                }
                // The scope's pages are now, in their home files, the
                // replay of the log below `through` — so ingest resumes
                // exactly there. Both counters move: the manifest only
                // names a point, and a scope whose `ingested` stayed at 0
                // would replay the whole log onto pages that already hold
                // it, routing each record by an open page that has moved
                // past it.
                {
                    let mut st = self.st();
                    let s = Self::scope_mut(&mut st, scope)?;
                    s.materialized = s.materialized.max(through);
                    s.ingested = s.ingested.max(through);
                }
                self.clear_doublewrite(ctx, &*handle).await?;
                completed = true;
            }
        }

        // 3. Every attached scope's files, measured and checked against the
        // manifest. Listed again: completing a batch may have created
        // segment files.
        let Measured {
            data_files,
            present: files_of,
        } = self.measure(&*handle, ctx).await?;
        {
            // A listed segment without both of its files has lost pages the
            // log may no longer rebuild — serving zeros for them would be
            // silent data loss.
            for (scope, segment) in &listed {
                if files_of.get(&(*scope, *segment)) != Some(&(true, true)) {
                    return Err(page_damage(
                        *scope,
                        PageKey {
                            segment: *segment,
                            page_no: 0,
                        },
                    ));
                }
            }
            // A file the manifest does not list is not covered by any
            // materialized LSN. Once step 2 has rewritten the manifest
            // from the files on disk, everything on disk is listed.
            if !rewrote_manifest {
                for (scope, segment) in files_of.keys() {
                    if !listed.contains(&(*scope, *segment)) {
                        return Err(manifest_damage());
                    }
                }
            }
        }

        // 5. Each segment's open page must read back. It is the page every
        // later insert is routed by; one read per segment is cheap insurance
        // that its image and record agree.
        let mut open_pages: Vec<(ScopeId, SegmentId, u32)> = Vec::new();
        {
            let st = self.st();
            for (scope, state) in st.scopes.iter() {
                for (segment, slots) in state.slots.iter() {
                    if *slots > 0 {
                        open_pages.push((*scope, *segment, *slots));
                    }
                }
            }
        }
        for (scope, segment, slots) in open_pages {
            let high = scan_high(&*handle, ctx, scope, segment, slots)
                .await?
                .ok_or_else(|| page_damage(scope, PageKey { segment, page_no: 0 }))?;
            let key = PageKey {
                segment,
                page_no: high,
            };
            if read_page(&*handle, ctx, scope, key).await?.is_none() {
                return Err(page_damage(scope, key));
            }
            Self::scope_mut(&mut self.st(), scope)?
                .high
                .insert(segment, high);
        }

        // 6. Orphans: page files of scopes the manifest no longer lists —
        // a detach interrupted between its manifest write and its file
        // removal. Scope ids are never reused (S18), so they are nobody's.
        let mut removed = 0usize;
        // The guard is dropped before any device call: the state lock is
        // never held across an `await`, even here where nothing else runs.
        let orphans: HashSet<ScopeId> = {
            let st = self.st();
            data_files
                .iter()
                .filter(|(scope, _, _)| !st.scopes.contains_key(scope))
                .map(|(scope, _, _)| *scope)
                .collect()
        };
        for scope in orphans {
            removed += self.remove_scope_files(&*handle, ctx, scope).await?;
        }

        self.open_report = OpenReport {
            completed_batch: completed,
            repaired_pages: repaired,
            removed_orphan_files: removed,
        };
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The store as a plan's page set
// ---------------------------------------------------------------------------

/// The store's page set for one scope, for one record's apply. It exists
/// only inside [`PageStore::ingest`], under the scope's ingest gate.
struct Pages<'a, D: BlockDevice, P: RedoPlan> {
    store: &'a PageStore<D, P>,
    ctx: &'a OpCtx<'a>,
    scope: ScopeId,
}

impl<D: BlockDevice, P: RedoPlan> PageSet for Pages<'_, D, P> {
    fn scope(&self) -> ScopeId {
        self.scope
    }

    fn high_page(&self, segment: SegmentId) -> Option<PageNo> {
        let st = self.store.st();
        st.scopes
            .get(&self.scope)
            .and_then(|s| s.high.get(&segment))
            .copied()
    }

    fn get<'b>(&'b self, key: PageKey) -> LocalBoxFuture<'b, Result<Option<PageData>, StoreError>> {
        Box::pin(self.store.page_data(self.ctx, self.scope, key))
    }

    fn put<'b>(
        &'b self,
        key: PageKey,
        page: PageData,
    ) -> LocalBoxFuture<'b, Result<(), StoreError>> {
        Box::pin(async move { self.store.insert_dirty(self.scope, key, page) })
    }
}

// ---------------------------------------------------------------------------
// The contracts
// ---------------------------------------------------------------------------

impl<D: BlockDevice, P: RedoPlan> PageSource for PageStore<D, P> {
    fn get_page<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        page: PageKey,
        at_lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<PageImage, StoreError>> {
        Box::pin(self.get_page_inner(ctx, scope, page, at_lsn))
    }

    fn materialized_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        Self::scope(&self.st(), scope).map(|s| s.materialized)
    }

    /// The **ingested** LSN, which is at or ahead of the materialized one:
    /// the newest version of every page is in memory up to it, so any
    /// request at or above it succeeds (§4.5 promise 5). Naming the
    /// materialized LSN instead would promise too little between flushes —
    /// a page changed since the last flush is held only in memory.
    fn oldest_readable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        Self::scope(&self.st(), scope).map(|s| s.ingested)
    }

    fn set_read_floor<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        reader: ReaderId,
        lsn: Lsn,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            // Recorded. Page versions arrive with the managed service
            // (§6.5); until then no version is ever discarded, so no floor
            // has anything to enforce.
            let mut st = self.st();
            Self::scope_mut(&mut st, scope)?.read_floors.insert(reader, lsn);
            Ok(())
        })
    }
}

impl<D: BlockDevice, P: RedoPlan> PageScopes for PageStore<D, P> {
    fn attach<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        lineage: Option<Lineage>,
        key: KeyRef,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(self.attach_scope(ctx, scope, lineage, key))
    }

    fn detach<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
    ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(self.detach_scope(ctx, scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_page::{encode_cell, PageView};
    use cascade_storage_api::fake::{MemBlockDevice, MemLogStore};
    use cascade_storage_api::log_sink::LogSink;
    use cascade_storage_api::log_source::LogSubscription;
    use cascade_storage_api::scope_admin::LogScopes;
    use cascade_storage_api::types::AppendMeta;
    use cascade_types::{Epoch, Hlc, TxnId, VersionWord};
    use cascade_wal_format::{
        encode, encode_with_flags, walk, WalRecord, V2_FORMAT_VERSION, FLAG_GROUP_END,
    };
    use futures_lite::future::block_on;
    use static_assertions::assert_impl_all;

    type Store = PageStore<MemBlockDevice, HeapInsertRedo>;

    const SCOPE: ScopeId = ScopeId(7);
    const OTHER: ScopeId = ScopeId(8);
    const KEY: KeyRef = KeyRef { key_id: 1 };
    const SEGMENT: SegmentId = SegmentId(1);

    /// Encoded lengths of the two records every group below is made of.
    const INS: u64 = 36 + 4 + (26 + 3000) + 4;
    const COMMIT: u64 = 36 + 8 + 4;

    fn page(page_no: u32) -> PageKey {
        PageKey {
            segment: SEGMENT,
            page_no,
        }
    }

    fn with_ctx<R>(f: impl FnOnce(&OpCtx<'_>) -> R) -> R {
        let span = cascade_telemetry::Span::noop();
        let ctx = OpCtx::new(TenantContext::for_database(DatabaseId(0)), &span);
        f(&ctx)
    }

    fn boot(dev: Arc<MemBlockDevice>, log: Arc<MemLogStore>, config: Config) -> Arc<Store> {
        let source: Arc<dyn LogSource> = log.clone();
        with_ctx(|ctx| {
            block_on(async {
                let store = PageStore::open(dev, source, HeapInsertRedo::new(), config)
                    .await
                    .expect("opens");
                store
                    .attach(ctx, SCOPE, None, KEY)
                    .await
                    .expect("attaches");
                store
            })
        })
    }

    struct Rig {
        dev: Arc<MemBlockDevice>,
        log: Arc<MemLogStore>,
        store: Arc<Store>,
    }

    fn rig(config: Config) -> Rig {
        let dev = Arc::new(MemBlockDevice::new());
        let log = Arc::new(MemLogStore::new());
        let store = boot(dev.clone(), log.clone(), config);
        with_ctx(|ctx| {
            block_on(async {
                log.create_scope(ctx, SCOPE, Epoch(1), KEY)
                    .await
                    .expect("log scope");
                log.open_writer(ctx, SCOPE, Epoch(1)).await.expect("opens writer");
            })
        });
        Rig { dev, log, store }
    }

    fn cell_for(xid: u64, payload: &[u8]) -> Vec<u8> {
        encode_cell(VersionWord::uncommitted(TxnId(xid)), payload).expect("a test cell fits")
    }

    /// Appends one mini-transaction at `at` — a heap insert per cell, then
    /// a commit carrying the group-end flag — and returns its end LSN.
    fn group_of(log: &MemLogStore, at: u64, xid: u64, cells: &[Vec<u8>], wait: bool) -> Lsn {
        let mut records: Vec<WalRecord> = cells
            .iter()
            .map(|cell| WalRecord::HeapInsert {
                xid,
                segment: SEGMENT,
                cell: cell.clone(),
            })
            .collect();
        records.push(WalRecord::CommitTxn {
            xid,
            commit_ts: Hlc(at),
        });
        let mut bytes = Vec::new();
        let mut off = at;
        for (i, rec) in records.iter().enumerate() {
            let encoded = if i + 1 == records.len() {
                encode_with_flags(rec, Lsn(off), 1, FLAG_GROUP_END)
            } else {
                encode(rec, Lsn(off), 1)
            };
            off += encoded.len() as u64;
            bytes.extend(encoded);
        }
        let meta = AppendMeta {
            max_commit_ts: None,
            writer_durable: Lsn(0),
        };
        with_ctx(|ctx| {
            block_on(async {
                log.append(ctx, SCOPE, Epoch(1), Lsn(at), &bytes, meta)
                    .await
                    .expect("appends");
                if wait {
                    log.wait_durable(ctx, SCOPE, Lsn(off)).await.expect("flushes")
                } else {
                    Lsn(off)
                }
            })
        })
    }

    fn group(log: &MemLogStore, at: u64, xid: u64, payloads: &[&[u8]]) -> Lsn {
        let cells: Vec<Vec<u8>> = payloads.iter().map(|p| cell_for(xid, p)).collect();
        group_of(log, at, xid, &cells, true)
    }

    fn get(store: &Store, scope: ScopeId, key: PageKey, at: Lsn) -> Result<PageImage, StoreError> {
        with_ctx(|ctx| block_on(store.get_page(ctx, scope, key, at)))
    }

    fn flush(store: &Store, scope: ScopeId) -> Result<FlushOutcome, StoreError> {
        with_ctx(|ctx| block_on(store.flush(ctx, scope)))
    }

    fn reopen(dev: Arc<MemBlockDevice>, log: Arc<MemLogStore>, config: Config) -> Result<Arc<Store>, StoreError> {
        let source: Arc<dyn LogSource> = log.clone();
        with_ctx(|_ctx| {
            block_on(PageStore::open(
                dev,
                source,
                HeapInsertRedo::new(),
                config,
            ))
        })
    }

    fn names(dev: &MemBlockDevice) -> Vec<String> {
        let listed = block_on(dev.local().list()).expect("lists");
        let mut names: Vec<String> = listed.into_iter().map(|n| n.0).collect();
        names.sort();
        names
    }

    /// §4.5 promise 1 at its edge: a request inside a record is refused
    /// before any of that record is applied, and the scope is left on the
    /// record's start.
    #[test]
    fn an_lsn_inside_a_record_is_refused_as_not_a_record_boundary() {
        let r = rig(Config::default());
        let end = group(&r.log, 0, 1, &[b"row"]);
        assert!(matches!(
            get(&r.store, SCOPE, page(0), Lsn(5)),
            Err(StoreError::NotRecordBoundary { at: Lsn(5) })
        ));
        assert_eq!(
            r.store.stats(SCOPE).unwrap().ingested,
            Lsn(0),
            "nothing of the record was applied"
        );
        let img = get(&r.store, SCOPE, page(0), end).expect("a boundary reads");
        assert_eq!(
            img.page_lsn.0,
            36 + 4 + (26 + 3) + 4,
            "the insert's end: the commit that closes the group touches no page"
        );
    }

    /// A read may ask for an LSN *below* the durable one, and the log
    /// source then serves whole records past it. The record that begins
    /// exactly at the requested LSN ends past it too, and must not be
    /// mistaken for one the request falls inside: everything below the
    /// requested LSN is applied, and the scope stops exactly there.
    /// (Found by the threaded test; this is its deterministic form.)
    #[test]
    fn a_read_below_the_durable_lsn_stops_exactly_where_asked() {
        let r = rig(Config::default());
        let end1 = group(&r.log, 0, 1, &[b"first"]);
        let end2 = group(&r.log, end1.0, 2, &[b"second"]);
        let end3 = group(&r.log, end2.0, 3, &[b"third"]);
        assert!(
            matches!(r.log.durable_lsn(SCOPE), Ok(d) if d == end3),
            "all three are durable"
        );

        // Asked for the first boundary while two more groups are durable:
        // the chunk spans all three.
        let as_of_1 = get(&r.store, SCOPE, page(0), end1).expect("a boundary is a boundary");
        assert_eq!(
            as_of_1.page_lsn.0,
            36 + 4 + (26 + 5) + 4,
            "the first row only"
        );
        assert_eq!(
            r.store.stats(SCOPE).unwrap().ingested,
            end1,
            "ingest stopped where asked"
        );

        // Then the next one, from a scope already partly ingested.
        let as_of_2 = get(&r.store, SCOPE, page(0), end2).expect("the second boundary");
        assert_eq!(as_of_2.page_lsn.0, end1.0 + 36 + 4 + (26 + 6) + 4, "two rows");
        assert_eq!(r.store.stats(SCOPE).unwrap().ingested, end2);

        // An LSN that really is inside a record, below the durable LSN, is
        // still refused — and leaves the scope on the boundary before it.
        let inside = Lsn(end2.0 + 10);
        assert!(matches!(
            get(&r.store, SCOPE, page(0), inside),
            Err(StoreError::NotRecordBoundary { at }) if at == inside
        ));
        assert_eq!(r.store.stats(SCOPE).unwrap().ingested, end2, "unchanged");

        // Asking for the latest end still works.
        get(&r.store, SCOPE, page(0), end3).expect("the latest");
        assert_eq!(r.store.stats(SCOPE).unwrap().ingested, end3);
    }

    /// A log source that serves real chunks but reports a durable LSN
    /// below what it served — a broken §4.4 promise 1.
    struct LyingSource(Arc<MemLogStore>);

    impl LogSource for LyingSource {
        fn read_log<'a>(
            &'a self,
            ctx: &'a OpCtx<'_>,
            scope: ScopeId,
            from: Lsn,
            max_bytes: usize,
        ) -> LocalBoxFuture<'a, Result<cascade_storage_api::types::LogChunk, StoreError>> {
            Box::pin(async move {
                let mut chunk = self.0.read_log(ctx, scope, from, max_bytes).await?;
                chunk.durable = Lsn(0);
                Ok(chunk)
            })
        }

        fn subscribe(
            &self,
            ctx: &OpCtx<'_>,
            scope: ScopeId,
            from: Lsn,
        ) -> Box<dyn LogSubscription> {
            self.0.subscribe(ctx, scope, from)
        }

        fn set_retained_from<'a>(
            &'a self,
            ctx: &'a OpCtx<'_>,
            scope: ScopeId,
            holder: RetentionHolder,
            lsn: Lsn,
        ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
            self.0.set_retained_from(ctx, scope, holder, lsn)
        }

        fn release_holder<'a>(
            &'a self,
            ctx: &'a OpCtx<'_>,
            scope: ScopeId,
            holder: RetentionHolder,
        ) -> LocalBoxFuture<'a, Result<(), StoreError>> {
            self.0.release_holder(ctx, scope, holder)
        }

        fn retained_floor(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
            self.0.retained_floor(scope)
        }

        fn retention_holders(
            &self,
            scope: ScopeId,
        ) -> Result<Vec<cascade_storage_api::HolderReport>, StoreError> {
            self.0.retention_holders(scope)
        }

        fn append_before_time<'a>(
            &'a self,
            ctx: &'a OpCtx<'_>,
            scope: ScopeId,
            ts: Hlc,
        ) -> LocalBoxFuture<'a, Result<Lsn, StoreError>> {
            self.0.append_before_time(ctx, scope, ts)
        }
    }

    /// "No non-durable record is ever applied" is the store's own promise,
    /// so it does not take the log source's word for it: a chunk that
    /// reaches past the durable LSN it reports is damage, nothing of it is
    /// applied, and the scope stays refused.
    #[test]
    fn a_chunk_past_the_durable_lsn_it_reports_is_refused() {
        let dev = Arc::new(MemBlockDevice::new());
        let log = Arc::new(MemLogStore::new());
        let source: Arc<dyn LogSource> = Arc::new(LyingSource(log.clone()));
        let store = with_ctx(|ctx| {
            block_on(async {
                let store = PageStore::open(dev, source, HeapInsertRedo::new(), Config::default())
                    .await
                    .expect("opens");
                store
                    .attach(ctx, SCOPE, None, KEY)
                    .await
                    .expect("attaches");
                log.create_scope(ctx, SCOPE, Epoch(1), KEY)
                    .await
                    .expect("log scope");
                log.open_writer(ctx, SCOPE, Epoch(1)).await.expect("opens writer");
                store
            })
        });
        let end = group(&log, 0, 1, &[b"row"]);

        for _ in 0..2 {
            assert!(
                matches!(
                    get(&store, SCOPE, page(0), end),
                    Err(StoreError::Corruption { .. })
                ),
                "served past the durable LSN it reports"
            );
        }
        assert_eq!(
            store.stats(SCOPE).unwrap().ingested,
            Lsn(0),
            "nothing of the chunk was applied"
        );
        assert!(
            matches!(
                get(&store, SCOPE, page(0), Lsn(0)),
                Err(StoreError::Corruption { .. })
            ),
            "and the scope stays refused"
        );
    }

    /// §4.5 promise 2: no non-durable record is ever applied.
    #[test]
    fn nothing_above_the_durable_lsn_is_applied() {
        let r = rig(Config::default());
        let end = group_of(&r.log, 0, 1, &[cell_for(1, b"row")], false); // accepted, not flushed
        assert!(matches!(
            get(&r.store, SCOPE, page(0), end),
            Err(StoreError::BeyondDurable { durable: Lsn(0) })
        ));
        assert_eq!(r.store.stats(SCOPE).unwrap().ingested, Lsn(0));
        with_ctx(|ctx| block_on(r.log.wait_durable(ctx, SCOPE, end))).expect("flushes");
        get(&r.store, SCOPE, page(0), end).expect("durable now");
    }

    /// §4.5 promise 4, and the scope checks every entry point shares.
    #[test]
    fn an_unwritten_page_is_zero_and_an_unknown_scope_is_refused() {
        let r = rig(Config::default());
        let img = get(&r.store, SCOPE, page(40), Lsn(0)).expect("unwritten");
        assert!(img.buf.as_slice().iter().all(|&b| b == 0));
        assert_eq!(img.page_lsn, Lsn(0));
        assert!(matches!(r.store.materialized_lsn(SCOPE), Ok(m) if m == Lsn(0)));
        assert!(matches!(r.store.oldest_readable_lsn(SCOPE), Ok(o) if o == Lsn(0)));
        for scope in [OTHER, ScopeId(u64::MAX)] {
            assert!(
                matches!(get(&r.store, scope, page(0), Lsn(0)), Err(StoreError::UnknownScope(s)) if s == scope)
            );
            assert!(matches!(
                r.store.materialized_lsn(scope),
                Err(StoreError::UnknownScope(s)) if s == scope
            ));
            assert!(r.store.stats(scope).is_err());
            assert!(flush(&r.store, scope).is_err());
        }
    }

    /// §4.5 promise 5: only the newest version of a page is held, so an
    /// older one is `TooOld` — never a silently newer answer.
    #[test]
    fn a_page_newer_than_the_requested_lsn_is_too_old() {
        let r = rig(Config::default());
        let end1 = group(&r.log, 0, 1, &[b"one"]);
        let end2 = group(&r.log, end1.0, 2, &[b"two"]);
        let img = get(&r.store, SCOPE, page(0), end2).expect("current");
        assert_eq!(img.page_lsn.0, end1.0 + 36 + 4 + (26 + 3) + 4);
        assert!(
            matches!(r.store.oldest_readable_lsn(SCOPE), Ok(oldest) if oldest == end2),
            "anything at or above the ingested LSN is servable"
        );
        assert!(
            matches!(get(&r.store, SCOPE, page(0), end1), Err(StoreError::TooOld { oldest }) if oldest == end2)
        );
        // A page the later records did not touch is still exactly itself.
        get(&r.store, SCOPE, page(9), end1).expect("untouched page");
    }

    /// The dirty bound is ingest's own backpressure. Nobody calls `flush`,
    /// yet the manifest advances — at a record boundary — and pages come
    /// back from disk.
    #[test]
    fn ingest_flushes_itself_at_the_dirty_bound() {
        let config = Config {
            cache_pages: 0,
            max_dirty_pages: 2,
            ..Config::default()
        };
        let r = rig(config);
        let big = [b'b'; 3000];
        let mut at = 0u64;
        let mut ends = Vec::new();
        for i in 0..5u64 {
            at = group(&r.log, at, i + 1, &[&big]).0;
            ends.push(at);
        }
        // Ingest without reading a page: a read would cache the page it
        // reads, and this is about what ingest alone leaves behind.
        with_ctx(|ctx| block_on(r.store.catch_up(ctx, SCOPE, Lsn(at)))).expect("ingests");

        // Two 3026-byte cells fill a page. The third insert opens page 1 and
        // makes two dirty pages, so the commit after it finds the bound
        // reached and flushes. The fourth insert joins page 1; the fifth
        // opens page 2, and again the commit after it flushes. The last
        // flush therefore covers the fifth insert, and nothing is dirty.
        let stats = r.store.stats(SCOPE).unwrap();
        assert_eq!(stats.materialized, Lsn(ends[3] + INS));
        assert_eq!(stats.dirty_pages, 0);
        assert_eq!(stats.cached_pages, 0, "nothing clean is cached");

        for (page_no, xids) in [(0u32, [1u64, 2]), (1, [3, 4])] {
            let img = get(&r.store, SCOPE, page(page_no), Lsn(at)).expect("read back from disk");
            let view = PageView::parse(img.buf.as_slice().try_into().expect("page-sized"))
                .expect("a valid heap page");
            assert_eq!(view.slot_count(), 2);
            for (slot, xid) in xids.iter().enumerate() {
                assert_eq!(
                    view.cell(slot).unwrap().xmin,
                    VersionWord::uncommitted(TxnId(*xid)),
                    "page {page_no}, slot {slot}"
                );
            }
        }
        let last = get(&r.store, SCOPE, page(2), Lsn(at)).expect("read back from disk");
        let view = PageView::parse(last.buf.as_slice().try_into().expect("page-sized")).unwrap();
        assert_eq!(view.slot_count(), 1);
        assert_eq!(COMMIT, 48, "test arithmetic: the lengths above are the format's");
    }

    /// A log already truncated below what the page store needs cannot be
    /// ingested. The scope fails, says so, and keeps saying so.
    #[test]
    fn a_truncated_log_fails_ingest_loudly_and_stays_failed() {
        let r = rig(Config::default());
        let end1 = group(&r.log, 0, 1, &[b"first"]);
        get(&r.store, SCOPE, page(0), end1).expect("ingests");
        flush(&r.store, SCOPE).expect("flushes");
        let end2 = group(&r.log, end1.0, 2, &[b"second"]);
        // Someone else's holder moves the floor past what this store has
        // materialized.
        with_ctx(|ctx| {
            block_on(
                r.log
                    .set_retained_from(ctx, SCOPE, RetentionHolder::Archive, end2),
            )
        })
        .expect("registers");
        for _ in 0..2 {
            assert!(
                matches!(get(&r.store, SCOPE, page(0), end2), Err(StoreError::Truncated { floor }) if floor == end2)
            );
        }
        let stats = r.store.stats(SCOPE).unwrap();
        assert_eq!(
            (stats.ingested, stats.materialized),
            (end1, end1),
            "the scope stopped where it was consistent"
        );
    }

    /// A malformed record — its CRC passed, its cell is not a cell — stops
    /// ingest at the record's start and is reported as damage there, on
    /// every later call.
    #[test]
    fn a_malformed_record_fails_ingest_naming_the_lsn() {
        let r = rig(Config::default());
        let end1 = group(&r.log, 0, 1, &[b"fine"]);
        get(&r.store, SCOPE, page(0), end1).expect("ingests");

        // A HEAP_INSERT whose cell's length prefix lies. The encoder stamps
        // the record's CRC over the bytes it is given, so the log's framing
        // accepts it; only redo can tell.
        let mut broken = cell_for(2, b"broken");
        broken[..2].copy_from_slice(&3u16.to_le_bytes());
        let end2 = group_of(&r.log, end1.0, 2, &[broken], true);

        for _ in 0..2 {
            assert!(
                matches!(get(&r.store, SCOPE, page(0), end2), Err(StoreError::Corruption { what: CorruptionSite::LogFrame { scope, at } }) if scope == SCOPE && at == end1),
                "damage is named at the record's start, and keeps being"
            );
        }
        assert_eq!(
            r.store.stats(SCOPE).unwrap().ingested,
            end1,
            "ingest stopped at the record's start"
        );
        // A failed scope refuses every read, even one below the LSN it
        // reached: the failure is the answer, not a half-current page. So
        // the page is inspected through the store's state instead.
        assert!(matches!(
            get(&r.store, SCOPE, page(0), end1),
            Err(StoreError::Corruption { .. })
        ));
        let slots = {
            let st = r.store.st();
            let image = st.scopes[&SCOPE].pages[&page(0)].image.clone();
            let bytes: &[u8; PAGE_SIZE] = image.as_slice().try_into().expect("page-sized");
            PageView::parse(bytes).expect("a valid heap page").slot_count()
        };
        assert_eq!(slots, 1, "the broken record touched nothing");
    }

    /// A record type this build does not know is refused too. The store
    /// cannot know whether such a record touches a page, so skipping it
    /// could silently drop a page change.
    #[test]
    fn an_unknown_record_type_is_refused_not_skipped() {
        let r = rig(Config::default());
        let end1 = group(&r.log, 0, 1, &[b"fine"]);
        get(&r.store, SCOPE, page(0), end1).expect("ingests");

        // A COMMIT whose type byte is rewritten to one nothing defines,
        // CRC-restamped.
        let mut stranger = encode_with_flags(
            &WalRecord::CommitTxn {
                xid: 2,
                commit_ts: Hlc(1),
            },
            Lsn(end1.0),
            1,
            FLAG_GROUP_END,
        );
        let n = stranger.len();
        stranger[8] = 200;
        let sum = crc32fast::hash(&stranger[..n - 4]);
        stranger[n - 4..].copy_from_slice(&sum.to_le_bytes());
        with_ctx(|ctx| {
            block_on(async {
                let meta = AppendMeta {
                    max_commit_ts: None,
                    writer_durable: Lsn(0),
                };
                r.log
                    .append(ctx, SCOPE, Epoch(1), end1, &stranger, meta)
                    .await
                    .expect("the log store is payload-blind and accepts it");
                r.log
                    .wait_durable(ctx, SCOPE, Lsn(end1.0 + n as u64))
                    .await
                    .expect("flushes");
            })
        });
        let end2 = Lsn(end1.0 + n as u64);
        assert!(matches!(
            get(&r.store, SCOPE, page(0), end2),
            Err(StoreError::Unsupported(_))
        ));
        assert_eq!(r.store.stats(SCOPE).unwrap().ingested, end1);
    }

    /// S19 at page-store scale: a failed sync fails the scope's flushes for
    /// the life of the instance, and `open` will not build on the file.
    #[test]
    fn a_failed_sync_fails_the_scope_until_reopen() {
        let r = rig(Config::default());
        let end = group(&r.log, 0, 1, &[b"row"]);
        get(&r.store, SCOPE, page(0), end).expect("ingests");
        r.dev.inject_sync_failure();
        let first = flush(&r.store, SCOPE);
        assert!(matches!(&first, Err(StoreError::Io(_))), "got {first:?}");
        let again = flush(&r.store, SCOPE);
        assert!(
            matches!(&again, Err(StoreError::Io(_))),
            "refused again, without going near the device"
        );
        // Reads still serve what is in memory.
        get(&r.store, SCOPE, page(0), end).expect("reads from memory");
        let reopened = reopen(r.dev.clone(), r.log.clone(), Config::default());
        assert!(
            matches!(&reopened, Err(StoreError::Io(_))),
            "open refuses the poisoned doublewrite file"
        );
    }

    #[test]
    fn detach_deletes_the_scopes_pages_and_open_sweeps_orphans() {
        let r = rig(Config::default());
        let end = group(&r.log, 0, 1, &[b"row"]);
        get(&r.store, SCOPE, page(0), end).unwrap();
        flush(&r.store, SCOPE).unwrap();
        let present = names(&r.dev);
        assert!(present.contains(&"pg-7-1".to_string()));
        assert!(present.contains(&"pgm-7-1".to_string()));
        assert!(present.contains(&"pg-MANIFEST".to_string()));

        with_ctx(|ctx| block_on(r.store.detach(ctx, SCOPE))).expect("detaches");
        assert!(names(&r.dev).iter().all(|n| !n.starts_with("pg-7-") && !n.starts_with("pgm-7-")));
        assert!(matches!(
            get(&r.store, SCOPE, page(0), end),
            Err(StoreError::UnknownScope(s)) if s == SCOPE
        ));
        // A scope the manifest no longer lists stays detached.
        let again = reopen(r.dev.clone(), r.log.clone(), Config::default()).expect("reopens");
        assert!(matches!(
            again.materialized_lsn(SCOPE),
            Err(StoreError::UnknownScope(s)) if s == SCOPE
        ));

        // A detach interrupted before its file removal leaves orphans.
        with_ctx(|ctx| {
            block_on(again.dev.local().open(ctx, &FileName("pg-7-1".into()), true))
        })
        .unwrap();
        with_ctx(|ctx| {
            block_on(again.dev.local().open(ctx, &FileName("pgm-7-1".into()), true))
        })
        .unwrap();
        let swept = reopen(r.dev.clone(), r.log.clone(), Config::default()).expect("reopens");
        assert_eq!(swept.open_report().removed_orphan_files, 2);
        assert!(!names(&r.dev).iter().any(|n| n.ends_with("-7-1")));
    }

    #[test]
    fn open_refuses_pages_it_cannot_vouch_for() {
        // Pages with no manifest: the LSN they are a replay through is lost.
        let dev = Arc::new(MemBlockDevice::new());
        let log = Arc::new(MemLogStore::new());
        with_ctx(|ctx| block_on(dev.local().open(ctx, &FileName("pg-7-1".into()), true)))
            .unwrap();
        let err = reopen(dev, log, Config::default()).err().expect("refuses");
        assert!(matches!(
            err,
            StoreError::Corruption {
                what: CorruptionSite::Manifest
            }
        ));

        // A listed segment whose image file has gone.
        let r = rig(Config::default());
        let end = group(&r.log, 0, 1, &[b"row"]);
        get(&r.store, SCOPE, page(0), end).unwrap();
        flush(&r.store, SCOPE).unwrap();
        with_ctx(|ctx| block_on(r.dev.local().remove(ctx, &FileName("pg-7-1".into())))).unwrap();
        let err = reopen(r.dev.clone(), r.log.clone(), Config::default())
            .err()
            .expect("refuses");
        assert!(matches!(
            err,
            StoreError::Corruption {
                what: CorruptionSite::Page { scope, page }
            } if scope == SCOPE && page.segment == SEGMENT
        ));
    }

    #[test]
    fn config_bounds_are_checked_before_the_store_opens() {
        assert_eq!(Config::default().validate(), Ok(()));
        let defaults = Config::default();
        let bad = Config {
            max_dirty_pages: defaults.dw_capacity() + 1,
            ..defaults.clone()
        };
        assert_eq!(bad.validate(), Err(ConfigError::DirtyPagesExceedDoublewrite));
        for blocks in [0usize, 65] {
            let bad = Config {
                dw_header_blocks: blocks,
                ..defaults.clone()
            };
            assert_eq!(bad.validate(), Err(ConfigError::DoublewriteHeaderOutOfRange));
        }
        let bad = Config {
            max_dirty_pages: 0,
            ..defaults.clone()
        };
        assert_eq!(bad.validate(), Err(ConfigError::DirtyBoundBelowOne));
        let bad = Config {
            read_chunk_bytes: 100,
            ..defaults
        };
        assert_eq!(bad.validate(), Err(ConfigError::ReadChunkTooSmall));

        let dev = Arc::new(MemBlockDevice::new());
        let err = reopen(dev, Arc::new(MemLogStore::new()), bad).err().unwrap();
        assert!(matches!(err, StoreError::Unsupported(_)));
    }

    #[test]
    fn read_floors_register_per_reader() {
        let r = rig(Config::default());
        with_ctx(|ctx| {
            block_on(async {
                r.store
                    .set_read_floor(ctx, SCOPE, ReaderId(1), Lsn(10))
                    .await
                    .expect("registers");
                r.store
                    .set_read_floor(ctx, SCOPE, ReaderId(2), Lsn(20))
                    .await
                    .expect("registers");
                assert!(matches!(
                    r.store.set_read_floor(ctx, OTHER, ReaderId(1), Lsn(10)).await,
                    Err(StoreError::UnknownScope(_))
                ));
            })
        });
    }

    /// S3: one object shared by every core; its futures stay on their core.
    #[test]
    fn the_store_is_one_object_shared_by_every_core() {
        assert_impl_all!(Store: Send, Sync);
        assert_impl_all!(HeapInsertRedo: Send, Sync);
        assert_impl_all!(PageData: Send, Sync);
    }

    /// Rewrites a v3-encoded byte run into v2 — the version field changes,
    /// the flags byte goes back to being a reserved zero, and each record's
    /// CRC is re-stamped. v2 still decodes, every record its own group of
    /// one (S16's compat half).
    fn rewritten_v2(bytes: Vec<u8>) -> Vec<u8> {
        let mut out = bytes;
        let mut at = 0usize;
        while at < out.len() {
            let f = walk(&out[at..]).expect("v3 frames before the rewrite");
            let end = at + f.total;
            out[at + 4..at + 8].copy_from_slice(&V2_FORMAT_VERSION.to_le_bytes());
            let sum = crc32fast::hash(&out[at..end - 4]);
            out[end - 4..end].copy_from_slice(&sum.to_le_bytes());
            at = end;
        }
        out
    }

    /// A log written in format version 2 — pre-flags bytes — replays into
    /// pages exactly like v3: decode accepts it, every record is its own
    /// group of one, and the framing check accepts the group for the same
    /// reason. The next v3 group continues the same scope without a seam.
    #[test]
    fn v2_records_ingest_and_materialize_like_v3() {
        let r = rig(Config::default());
        // A v2 mini-transaction: the commit carries no flags byte, because
        // in v2 that byte was still reserved zero.
        let v2 = rewritten_v2({
            let ins = encode(
                &WalRecord::HeapInsert {
                    xid: 1,
                    segment: SEGMENT,
                    cell: cell_for(1, b"ancient row"),
                },
                Lsn(0),
                1,
            );
            let com = encode(
                &WalRecord::CommitTxn {
                    xid: 1,
                    commit_ts: Hlc(1),
                },
                Lsn(ins.len() as u64),
                1,
            );
            let mut b = ins;
            b.extend(com);
            b
        });
        with_ctx(|ctx| {
            block_on(async {
                r.log
                    .append(ctx, SCOPE, Epoch(1), Lsn(0), &v2, AppendMeta {
                        max_commit_ts: Some(Hlc(1)),
                        writer_durable: Lsn(0),
                    })
                    .await
                    .expect("the framing accepts a v2 group");
                r.log
                    .wait_durable(ctx, SCOPE, Lsn(v2.len() as u64))
                    .await
                    .expect("flushes");
            })
        });
        let end = Lsn(v2.len() as u64);
        let img = get(&r.store, SCOPE, page(0), end).expect("reads");
        let view = PageView::parse(img.buf.as_slice().try_into().expect("page-sized")).unwrap();
        assert_eq!(view.slot_count(), 1);
        assert_eq!(img.page_lsn.0, 36 + 4 + (26 + 11) + 4, "the insert's end");
        assert_eq!(r.store.stats(SCOPE).unwrap().ingested, end);

        // And a v3 group continues the same scope without a seam.
        let end3 = group(&r.log, end.0, 2, &[b"modern row"]);
        let img = get(&r.store, SCOPE, page(0), end3).expect("reads");
        let view = PageView::parse(img.buf.as_slice().try_into().expect("page-sized")).unwrap();
        assert_eq!(view.slot_count(), 2, "both rows, one v2 and one v3");
    }

    /// The no-txn housekeeping records that exist — the closed-timestamp
    /// heartbeat and the HLC high-water mark — are page-neutral: ingest
    /// advances past them, the restart point follows, and the page they
    /// ride along with is untouched.
    #[test]
    fn housekeeping_records_ingest_without_touching_pages() {
        let r = rig(Config::default());
        let end1 = group(&r.log, 0, 1, &[b"row"]);
        get(&r.store, SCOPE, page(0), end1).expect("ingests");
        flush(&r.store, SCOPE).expect("the insert's page lands");

        let mut bytes = encode(
            &WalRecord::ClosedTsHeartbeat { closed_ts: 77 },
            Lsn(end1.0),
            1,
        );
        let hw = encode_with_flags(
            &WalRecord::HlcHighWater { max_issued: 88 },
            Lsn(end1.0 + bytes.len() as u64),
            1,
            FLAG_GROUP_END,
        );
        bytes.extend(hw);
        with_ctx(|ctx| {
            block_on(async {
                r.log
                    .append(ctx, SCOPE, Epoch(1), end1, &bytes, AppendMeta {
                        max_commit_ts: None,
                        writer_durable: Lsn(0),
                    })
                    .await
                    .expect("appends");
                r.log
                    .wait_durable(ctx, SCOPE, Lsn(end1.0 + bytes.len() as u64))
                    .await
                    .expect("flushes");
            })
        });
        let end2 = Lsn(end1.0 + bytes.len() as u64);

        let img = get(&r.store, SCOPE, page(0), end2).expect("reads");
        let view = PageView::parse(img.buf.as_slice().try_into().expect("page-sized")).unwrap();
        assert_eq!(view.slot_count(), 1, "nothing was added to the page");
        assert_eq!(img.page_lsn, Lsn(36 + 4 + (26 + 3) + 4), "the page is exactly as it was");
        assert_eq!(r.store.stats(SCOPE).unwrap().ingested, end2, "ingest advanced");
        let outcome = flush(&r.store, SCOPE);
        assert!(
            matches!(&outcome, Ok(FlushOutcome::Advanced { materialized }) if *materialized == end2),
            "got {outcome:?}"
        );
    }
}
