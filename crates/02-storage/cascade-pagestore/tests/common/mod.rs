//! The harness the page store's integration tests share: a real log store
//! and a real page store on one in-memory device, a workload driver that
//! keeps its own reference replay of everything it appends, and a device
//! that crashes at a chosen I/O.
//!
//! The reference replay is the point of every test here (§10): a page the
//! store serves must equal the page built from the same records, byte for
//! byte. It is built with the page format alone — `PageMut::append_cell`,
//! and a new page whenever the page says it is full — so that neither the
//! store nor `cascade-redo` is ever asked to vouch for itself.

#![allow(dead_code)] // each test binary uses the part of the harness it needs

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cascade_logstore::LogStore;
use cascade_page::PageMut;
use cascade_pagestore::{Config, HeapInsertRedo, PageStore};
use cascade_redo::MAX_CELL_LEN;
use cascade_storage_api::fake::{test_ctx, MemBlockDevice};
use cascade_storage_api::key_provider::KeyRef;
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::log_source::LogSource;
use cascade_storage_api::page_source::PageSource;
use cascade_storage_api::scope_admin::{LogScopes, PageScopes};
use cascade_storage_api::types::AppendMeta;
use cascade_storage_api::{
    BlockDevice, BlockHandle, FileName, FileId, IoError, OpCtx, PageKey, StoreError, TenantContext,
};
use cascade_types::{
    DatabaseId, Epoch, Hlc, LocalBoxFuture, Lsn, PAGE_SIZE, PageNo, ScopeId, SegmentId, TxnId,
    VersionWord,
};
use cascade_wal_format::{encode, encode_with_flags, encoded_len, WalRecord, FLAG_GROUP_END};
use futures_lite::future::block_on;

pub const SCOPE: ScopeId = ScopeId(7);
pub const KEY: KeyRef = KeyRef { key_id: 1 };

/// How many segments the workload writes into.
pub const SEGMENTS: u32 = 4;
/// The largest payload any page of the format can hold.
pub const MAX_PAYLOAD: usize = MAX_CELL_LEN - cascade_page::CELL_FIXED;

pub fn with_ctx<R>(f: impl FnOnce(&OpCtx<'_>) -> R) -> R {
    let span = cascade_telemetry::Span::noop();
    let ctx = test_ctx(&span, 1);
    f(&ctx)
}

// ---------------------------------------------------------------------------
// A node: one device, one log store, one page store
// ---------------------------------------------------------------------------

pub struct Node<D: BlockDevice + 'static> {
    pub dev: Arc<D>,
    pub log: Arc<LogStore<D>>,
    pub pages: Arc<PageStore<D, HeapInsertRedo>>,
}

/// Opens the log store on `dev`.
pub fn open_log<D: BlockDevice + 'static>(dev: Arc<D>) -> Arc<LogStore<D>> {
    block_on(LogStore::open(dev)).expect("the log store opens")
}

/// Opens the page store on `dev`, reading the log through `source`.
pub fn open_pages<D: BlockDevice + 'static>(
    dev: Arc<D>,
    source: Arc<dyn LogSource>,
    config: Config,
) -> Arc<PageStore<D, HeapInsertRedo>> {
    block_on(PageStore::open(
        dev,
        source,
        HeapInsertRedo::new(),
        config,
    ))
    .expect("the page store opens")
}

/// Opens both stores on `dev`: one log store, which the page store reads.
pub fn boot<D: BlockDevice + 'static>(dev: Arc<D>, config: Config) -> Node<D> {
    let log = open_log(dev.clone());
    let source: Arc<dyn LogSource> = log.clone();
    let pages = open_pages(dev.clone(), source, config);
    Node { dev, log, pages }
}

/// Bundles already-open stores as a node.
pub fn node_of<D: BlockDevice + 'static>(
    dev: Arc<D>,
    log: Arc<LogStore<D>>,
    pages: Arc<PageStore<D, HeapInsertRedo>>,
) -> Node<D> {
    Node { dev, log, pages }
}

/// Attaches the scope to both stores, ready to write.
pub fn create_scope<D: BlockDevice + 'static>(node: &Node<D>, scope: ScopeId) {
    with_ctx(|ctx| {
        block_on(async {
            node.pages
                .attach(ctx, scope, None, KEY)
                .await
                .expect("the page store attaches the scope");
            node.log
                .create_scope(ctx, scope, Epoch(1), KEY)
                .await
                .expect("the log store creates the scope");
            node.log
                .open_writer(ctx, scope, Epoch(1))
                .await
                .expect("the writer opens");
        })
    })
}

pub fn flush<D: BlockDevice + 'static>(node: &Node<D>, scope: ScopeId) -> Result<cascade_pagestore::FlushOutcome, StoreError> {
    with_ctx(|ctx| block_on(node.pages.flush(ctx, scope)))
}

pub fn catch_up<D: BlockDevice + 'static>(node: &Node<D>, scope: ScopeId, to: Lsn) -> Result<(), StoreError> {
    with_ctx(|ctx| block_on(node.pages.catch_up(ctx, scope, to)))
}

/// A store with a small cache and a small dirty bound: pages cycle through
/// disk and the doublewrite area often, which is what these tests want.
pub fn squeezed(cache_pages: usize, max_dirty_pages: usize) -> Config {
    Config {
        cache_pages,
        max_dirty_pages,
        ..Config::default()
    }
}

/// A store that never flushes on its own, for tests that arm exactly one.
pub fn no_backpressure(cache_pages: usize) -> Config {
    let defaults = Config::default();
    squeezed(cache_pages, defaults.dw_capacity())
}

// ---------------------------------------------------------------------------
// Randomness: deterministic, replayable from the seed
// ---------------------------------------------------------------------------

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---------------------------------------------------------------------------
// The workload and its reference replay
// ---------------------------------------------------------------------------

pub struct Row {
    pub segment: SegmentId,
    pub payload: Vec<u8>,
}

/// The log replayed through the page format alone, kept as the pages it
/// produces.
pub struct Reference {
    pages: HashMap<(SegmentId, PageNo), Box<[u8; PAGE_SIZE]>>,
    /// Each segment's open page: the last one the replay appended to.
    open: HashMap<SegmentId, PageNo>,
    /// Every page's page LSN, after each checkpoint.
    history: Vec<HashMap<(SegmentId, PageNo), Lsn>>,
    /// The log end at each checkpoint.
    pub ends: Vec<Lsn>,
    pub end: Lsn,
}

impl Reference {
    pub fn new() -> Self {
        Reference {
            pages: HashMap::new(),
            open: HashMap::new(),
            history: Vec::new(),
            ends: Vec::new(),
            end: Lsn(0),
        }
    }

    pub fn page_lsn_of(image: &[u8; PAGE_SIZE]) -> Lsn {
        Lsn(u64::from_le_bytes(image[8..16].try_into().expect("8 bytes")))
    }

    fn slots_of(image: &[u8; PAGE_SIZE]) -> u16 {
        u16::from_le_bytes(image[4..6].try_into().expect("2 bytes"))
    }

    /// Replays records, in the order the log holds them, with nothing but
    /// the page format: each cell is appended to its segment's open page
    /// with `PageMut::append_cell`, and when the page itself answers `Full`
    /// the replay opens the next one. That is the sequential-fill rule
    /// stated in the format's own terms. Neither `cascade-redo`'s apply nor
    /// its fill cursor is consulted here, so a defect in either — a page
    /// LSN stamped wrong, free space counted wrong, a cell placed on the
    /// wrong page — shows up as a difference against the store.
    pub fn apply(&mut self, records: &[(WalRecord, u64)]) {
        for (rec, start) in records {
            let WalRecord::HeapInsert { segment, cell, .. } = rec else {
                continue;
            };
            let cell = cascade_page::parse_cell(cell).expect("a well-formed cell");
            let end = Lsn(start + encoded_len(rec) as u64);
            loop {
                let page_no = *self.open.entry(*segment).or_insert(0);
                let page = self
                    .pages
                    .entry((*segment, page_no))
                    .or_insert_with(|| Box::new([0u8; PAGE_SIZE]));
                let fresh = !cascade_page::is_initialized(page.as_mut());
                let mut pm = if fresh {
                    PageMut::init(page.as_mut())
                } else {
                    PageMut::wrap(page.as_mut()).expect("the replay's own pages are valid")
                };
                match pm.append_cell(cell.xmin, cell.payload) {
                    Ok(_) => {
                        pm.set_page_lsn(end.0);
                        break;
                    }
                    // The page's own answer: it has no room for this cell,
                    // so the next one is opened.
                    Err(cascade_page::PageError::Full) if !fresh => {
                        self.open.insert(*segment, page_no + 1);
                    }
                    Err(e) => panic!("the replay cannot place a cell: {e}"),
                }
            }
        }
        if let Some((rec, start)) = records.last() {
            self.end = Lsn(start + encoded_len(rec) as u64);
        }
    }

    /// Records where every page stands.
    pub fn checkpoint(&mut self) {
        self.history.push(
            self.pages
                .iter()
                .map(|(key, image)| (*key, Reference::page_lsn_of(image)))
                .collect(),
        );
        self.ends.push(self.end);
    }

    pub fn keys(&self) -> Vec<(SegmentId, PageNo)> {
        let mut keys: Vec<_> = self.pages.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    pub fn image(&self, key: (SegmentId, PageNo)) -> Option<&[u8; PAGE_SIZE]> {
        self.pages.get(&key).map(|b| &**b)
    }

    pub fn page_lsn(&self, key: (SegmentId, PageNo)) -> Option<Lsn> {
        self.pages.get(&key).map(|b| Reference::page_lsn_of(b))
    }

    /// The highest page the replay has written in `segment`.
    pub fn high(&self, segment: SegmentId) -> Option<PageNo> {
        self.pages
            .keys()
            .filter(|(s, _)| *s == segment)
            .map(|(_, p)| *p)
            .max()
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }
}

/// Builds one mini-transaction at `at` — a HEAP_INSERT per row, closed by
/// a COMMIT carrying the group-end flag (S16) — as the bytes to append and
/// as the records with their start LSNs.
pub fn group(at: u64, xid: u64, rows: &[Row]) -> (Vec<u8>, Vec<(WalRecord, u64)>) {
    let mut records: Vec<WalRecord> = rows
        .iter()
        .map(|row| WalRecord::HeapInsert {
            xid,
            segment: row.segment,
            cell: cascade_page::encode_cell(VersionWord::uncommitted(TxnId(xid)), &row.payload)
                .expect("a test cell fits"),
        })
        .collect();
    records.push(WalRecord::CommitTxn {
        xid,
        commit_ts: Hlc(at),
    });
    let mut bytes = Vec::new();
    let mut placed = Vec::with_capacity(records.len());
    let mut off = at;
    for (i, rec) in records.iter().enumerate() {
        let encoded = if i + 1 == records.len() {
            encode_with_flags(rec, Lsn(off), 1, FLAG_GROUP_END)
        } else {
            encode(rec, Lsn(off), 1)
        };
        placed.push((rec.clone(), off));
        off += encoded.len() as u64;
        bytes.extend(encoded);
    }
    (bytes, placed)
}

/// A deterministic workload with its reference replay.
pub struct Workload {
    pub at: Lsn,
    xid: u64,
    pub reference: Reference,
    rng: Rng,
    /// Groups appended so far.
    pub groups: u64,
}

impl Workload {
    pub fn new(seed: u64) -> Self {
        Workload {
            at: Lsn(0),
            xid: 1,
            reference: Reference::new(),
            rng: Rng::new(seed),
            groups: 0,
        }
    }

    /// Zero to three rows. Payload sizes straddle the spill boundary often
    /// and reach the format's ceiling.
    pub fn random_rows(&mut self) -> Vec<Row> {
        let count = match self.rng.below(6) {
            0 => 0,
            1 | 2 => 1,
            3 | 4 => 2,
            _ => 3,
        };
        (0..count)
            .map(|i| {
                let segment = SegmentId(1 + self.rng.below(SEGMENTS as u64) as u32);
                let len = match self.rng.below(8) {
                    0 => self.rng.below(40),
                    1 => 40 + self.rng.below(560),
                    2 | 3 => 600 + self.rng.below(1400),
                    4..=6 => 2000 + self.rng.below(3000),
                    _ => 5000 + self.rng.below((MAX_PAYLOAD - 5000) as u64 + 1),
                } as usize;
                let mut payload = vec![b'r'; len];
                if len >= 8 {
                    // A tag that tells rows apart, so a mis-routed or
                    // duplicated row shows up as a byte difference.
                    payload[..8].copy_from_slice(&((self.xid << 8) | i).to_le_bytes());
                }
                Row { segment, payload }
            })
            .collect()
    }

    /// Appends one mini-transaction, waits for it to be durable, and
    /// replays it into the reference.
    pub fn push<D: BlockDevice + 'static>(
        &mut self,
        node: &Node<D>,
        scope: ScopeId,
        rows: &[Row],
    ) -> Result<Lsn, StoreError> {
        let (bytes, records) = group(self.at.0, self.xid, rows);
        let start = self.at;
        let end = with_ctx(|ctx| {
            block_on(async {
                let meta = AppendMeta {
                    max_commit_ts: Some(Hlc(start.0)),
                    writer_durable: Lsn(0),
                };
                node.log
                    .append(ctx, scope, Epoch(1), start, &bytes, meta)
                    .await?;
                node.log
                    .wait_durable(ctx, scope, Lsn(start.0 + bytes.len() as u64))
                    .await
            })
        })?;
        self.reference.apply(&records);
        self.at = end;
        self.xid += 1;
        self.groups += 1;
        Ok(end)
    }

    pub fn push_random<D: BlockDevice + 'static>(&mut self, node: &Node<D>, scope: ScopeId) -> Result<Lsn, StoreError> {
        let rows = self.random_rows();
        self.push(node, scope, &rows)
    }

    pub fn checkpoint(&mut self) {
        self.reference.checkpoint();
    }
}

// ---------------------------------------------------------------------------
// Verification, always against the reference
// ---------------------------------------------------------------------------

/// Reads every page the reference holds, as of `at`, and requires each to
/// equal the replay byte for byte — its page LSN included. Returns how
/// many pages were compared.
pub fn verify<D: BlockDevice + 'static>(node: &Node<D>, scope: ScopeId, reference: &Reference, at: Lsn) -> usize {
    with_ctx(|ctx| {
        block_on(async {
            for key in reference.keys() {
                let page = PageKey {
                    segment: key.0,
                    page_no: key.1,
                };
                let want = reference.image(key).expect("a reference page");
                let got = node
                    .pages
                    .get_page(ctx, scope, page, at)
                    .await
                    .unwrap_or_else(|e| panic!("page {page:?} as of {at:?}: {e:?}"));
                assert_eq!(
                    got.page_lsn,
                    Reference::page_lsn_of(want),
                    "page {page:?}: page LSN as of {at:?}"
                );
                let got_bytes = got.buf.as_slice();
                if got_bytes != want {
                    // Where they part, and what each side thinks of the
                    // page — never the 8 KiB themselves.
                    let first = (0..PAGE_SIZE).find(|&i| got_bytes[i] != want[i]);
                    panic!(
                        "page {page:?} differs from the replay as of {at:?}: first differing \
                         byte at {:?}; store slots {}, replay slots {}",
                        first,
                        Reference::slots_of(got_bytes.try_into().expect("page-sized")),
                        Reference::slots_of(want),
                    );
                }
            }
            reference.page_count()
        })
    })
}

/// §4.5 promises 1 and 5, differentially. At every past checkpoint, a page
/// is either exactly what it was then — because nothing changed it since —
/// or `TooOld`, because something did. There is no third answer.
pub fn verify_history<D: BlockDevice + 'static>(node: &Node<D>, scope: ScopeId, reference: &Reference) -> usize {
    let mut checked = 0usize;
    with_ctx(|ctx| {
        block_on(async {
            for (i, then) in reference.history.iter().enumerate() {
                let at = reference.ends[i];
                for key in reference.keys() {
                    let page = PageKey {
                        segment: key.0,
                        page_no: key.1,
                    };
                    let now = reference.page_lsn(key).expect("a reference page");
                    let result = node.pages.get_page(ctx, scope, page, at).await;
                    if then.get(&key) == Some(&now) {
                        // Unchanged since: exactly the page it is now.
                        let got = result.unwrap_or_else(|e| {
                            panic!("unchanged page {page:?} as of {at:?} must read, got {e:?}")
                        });
                        assert_eq!(got.page_lsn, now, "page {page:?} as of {at:?}");
                        assert!(
                            got.buf.as_slice() == reference.image(key).expect("a reference page"),
                            "unchanged page {page:?} differs as of {at:?}"
                        );
                    } else {
                        assert!(
                            matches!(result, Err(StoreError::TooOld { .. })),
                            "page {page:?} changed since {at:?}: TooOld, never a newer image, got {:?}",
                            result.map(|img| img.page_lsn)
                        );
                    }
                    checked += 1;
                }
            }
        })
    });
    checked
}

/// `verify`, and then the pages past every segment's highest must be
/// unwritten. The second half is what catches a mis-route that creates
/// pages beyond the replay's: comparing only the replay's own pages would
/// miss garbage on a page the replay never had.
pub fn verify_whole<D: BlockDevice + 'static>(
    node: &Node<D>,
    scope: ScopeId,
    reference: &Reference,
    at: Lsn,
) -> usize {
    let compared = verify(node, scope, reference, at);
    verify_unwritten(node, scope, reference);
    compared
}

/// §4.5 promise 4: past a segment's highest page nothing was ever written.
pub fn verify_unwritten<D: BlockDevice + 'static>(node: &Node<D>, scope: ScopeId, reference: &Reference) {
    with_ctx(|ctx| {
        block_on(async {
            let at = reference.end;
            for segment in 1..=SEGMENTS {
                let segment = SegmentId(segment);
                let beyond = reference.high(segment).map_or(0, |high| high + 1);
                for page_no in beyond..beyond + 3 {
                    let page = PageKey { segment, page_no };
                    let img = node
                        .pages
                        .get_page(ctx, scope, page, at)
                        .await
                        .unwrap_or_else(|e| panic!("unwritten page {page:?}: {e:?}"));
                    assert!(
                        img.buf.as_slice().iter().all(|&b| b == 0),
                        "unwritten page {page:?} is not zero"
                    );
                    assert_eq!(img.page_lsn, Lsn(0), "unwritten page {page:?}");
                }
            }
        })
    });
}

// ---------------------------------------------------------------------------
// A device that crashes at a chosen page-store operation
// ---------------------------------------------------------------------------

/// A device that crashes at a chosen page-store write, sync or rename —
/// the cut points the flush protocol actually has (§6.3).
///
/// `MemBlockDevice::crash` keeps each unsynced 4 KiB block with probability
/// one half, so a cut inside a flush produces exactly the states recovery
/// must tell apart: a batch half written to the doublewrite area, home
/// pages torn (an 8 KiB page is two blocks, and either may be the one
/// that survives), a record block persisted without the image it describes.
///
/// Only the page store's files (`pg…`) are cut. The log store's writes are
/// left alone: every test makes its appends durable *before* arming a cut,
/// so the acknowledged log is exactly what the reference replayed.
#[derive(Clone)]
pub struct CutDevice {
    inner: MemBlockDevice,
    state: Arc<CutState>,
}

struct CutState {
    /// Page-store operations left before the cut; zero disarms it.
    armed: AtomicU64,
    /// Whether the cut lands before its operation (which is then lost) or
    /// after it (so it is written, and may or may not survive the crash).
    cut_after: bool,
    seed: u64,
    dead: AtomicBool,
    /// Which open file is which, so a `FileId` can be recognised as the
    /// page store's.
    names: Mutex<HashMap<FileId, FileName>>,
    /// Page-store operations seen, cut or not.
    ops: AtomicU64,
}

fn dead() -> IoError {
    IoError(std::io::Error::other("the device crashed"))
}

impl CutDevice {
    pub fn new(seed: u64, cut_after: bool) -> Self {
        CutDevice {
            inner: MemBlockDevice::new(),
            state: Arc::new(CutState {
                armed: AtomicU64::new(0),
                cut_after,
                seed,
                dead: AtomicBool::new(false),
                names: Mutex::new(HashMap::new()),
                ops: AtomicU64::new(0),
            }),
        }
    }

    /// Crashes at the `n`-th page-store write, sync or rename from now on.
    pub fn arm(&self, n: u64) {
        self.state.dead.store(false, Ordering::SeqCst);
        self.state.armed.store(n, Ordering::SeqCst);
    }

    /// Page-store operations seen so far.
    pub fn ops(&self) -> u64 {
        self.state.ops.load(Ordering::SeqCst)
    }

    pub fn crashed(&self) -> bool {
        self.state.dead.load(Ordering::SeqCst)
    }

    /// A crash with nothing in flight: the power simply goes out.
    pub fn plain_crash(&self, seed: u64) {
        self.inner.crash(seed);
    }

    /// Brings the machine back: the device answers again, with no cut armed.
    pub fn revive(&self) {
        self.state.dead.store(false, Ordering::SeqCst);
        self.state.armed.store(0, Ordering::SeqCst);
        self.state.names.lock().unwrap().clear();
    }

    fn fire(&self) -> IoError {
        self.state.dead.store(true, Ordering::SeqCst);
        self.inner.crash(self.state.seed);
        dead()
    }

    /// Counts a page-store operation and says whether the cut lands on it.
    /// `Err` once the device is dead — and, for a cut that lands *before*
    /// its operation, right there: the operation is never performed.
    fn arrive(&self, ours: bool) -> Result<bool, IoError> {
        if self.state.dead.load(Ordering::SeqCst) {
            return Err(dead());
        }
        if !ours {
            return Ok(false);
        }
        self.state.ops.fetch_add(1, Ordering::SeqCst);
        let left = self.state.armed.load(Ordering::SeqCst);
        if left == 0 || left > 1 {
            if left > 0 {
                self.state.armed.store(left - 1, Ordering::SeqCst);
            }
            return Ok(false);
        }
        // This operation is the cut.
        if self.state.cut_after {
            Ok(true)
        } else {
            Err(self.fire())
        }
    }

    /// The after-half of a cut that let its operation through.
    fn land<T>(&self, cut: bool, result: Result<T, IoError>) -> Result<T, IoError> {
        match result {
            Err(e) => Err(e),
            Ok(_) if cut => Err(self.fire()),
            Ok(value) => Ok(value),
        }
    }
}

impl BlockDevice for CutDevice {
    fn local(&self) -> Box<dyn BlockHandle> {
        Box::new(CutHandle { dev: self.clone() })
    }
}

struct CutHandle {
    dev: CutDevice,
}

impl CutHandle {
    /// Whether `f` is one of the page store's files.
    fn ours(&self, f: FileId) -> bool {
        self.dev
            .state
            .names
            .lock()
            .unwrap()
            .get(&f)
            .is_some_and(|name| name.0.starts_with("pg"))
    }

    fn dead(&self) -> Result<(), IoError> {
        if self.dev.crashed() {
            Err(dead())
        } else {
            Ok(())
        }
    }
}

/// A no-op span every forwarded operation can borrow.
static NOOP_SPAN: cascade_telemetry::Span = cascade_telemetry::Span::noop();

/// An operation context for a forwarded operation. The trait's returned
/// future may borrow only `self` — the lifetime in `dyn Future + 'a` is
/// invariant, so a wrapper cannot also capture the caller's context — and
/// the fake device ignores the context anyway.
fn own_ctx() -> OpCtx<'static> {
    OpCtx::new(TenantContext::for_database(DatabaseId(0)), &NOOP_SPAN)
}

impl BlockHandle for CutHandle {
    fn open(
        &self,
        _ctx: &OpCtx<'_>,
        name: &FileName,
        create: bool,
    ) -> LocalBoxFuture<'_, Result<FileId, IoError>> {
        let name = name.clone();
        Box::pin(async move {
            self.dead()?;
            let inner = self.dev.inner.local();
            let f = inner.open(&own_ctx(), &name, create).await?;
            self.dev
                .state
                .names
                .lock()
                .unwrap()
                .insert(f, name);
            Ok(f)
        })
    }

    fn read_at(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        buf: cascade_types::AlignedBuf,
    ) -> LocalBoxFuture<'_, Result<cascade_types::AlignedBuf, IoError>> {
        Box::pin(async move {
            self.dev.inner.local().read_at(&own_ctx(), f, off, buf).await
        })
    }

    fn write_at(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        buf: cascade_types::AlignedBuf,
    ) -> LocalBoxFuture<'_, Result<cascade_types::AlignedBuf, IoError>> {
        Box::pin(async move {
            let cut = self.dev.arrive(self.ours(f))?;
            let inner = self.dev.inner.local();
            let written = inner.write_at(&own_ctx(), f, off, buf).await;
            self.dev.land(cut, written)
        })
    }

    fn sync(&self, _ctx: &OpCtx<'_>, f: FileId) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move {
            let cut = self.dev.arrive(self.ours(f))?;
            let inner = self.dev.inner.local();
            let synced = inner.sync(&own_ctx(), f).await;
            self.dev.land(cut, synced)
        })
    }

    fn len(&self, f: FileId) -> LocalBoxFuture<'_, Result<u64, IoError>> {
        Box::pin(async move { self.dev.inner.local().len(f).await })
    }

    fn allocate(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        len: u64,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move {
            self.dev
                .inner
                .local()
                .allocate(&own_ctx(), f, off, len)
                .await
        })
    }

    fn punch_hole(
        &self,
        _ctx: &OpCtx<'_>,
        f: FileId,
        off: u64,
        len: u64,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move {
            self.dev
                .inner
                .local()
                .punch_hole(&own_ctx(), f, off, len)
                .await
        })
    }

    fn remove(&self, _ctx: &OpCtx<'_>, name: &FileName) -> LocalBoxFuture<'_, Result<(), IoError>> {
        let name = name.clone();
        Box::pin(async move {
            self.dev.inner.local().remove(&own_ctx(), &name).await
        })
    }

    fn rename(
        &self,
        _ctx: &OpCtx<'_>,
        from: &FileName,
        to: &FileName,
    ) -> LocalBoxFuture<'_, Result<(), IoError>> {
        let (from, to) = (from.clone(), to.clone());
        Box::pin(async move {
            let cut = self.dev.arrive(from.0.starts_with("pg"))?;
            let inner = self.dev.inner.local();
            let renamed = inner.rename(&own_ctx(), &from, &to).await;
            self.dev.land(cut, renamed)
        })
    }

    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<FileName>, IoError>> {
        Box::pin(async move { self.dev.inner.local().list().await })
    }

    fn sync_namespace(&self) -> LocalBoxFuture<'_, Result<(), IoError>> {
        Box::pin(async move { self.dev.inner.local().sync_namespace().await })
    }
}
