//! Tier A session (FR-14, FR-16): one connection owned by exactly one
//! worker reactor for its whole life, running the simple-query loop
//! (startup happened on the housekeeper).
//!
//! `Session` holds an `Rc` so the type is structurally `!Send` — a future
//! refactor that makes connection state cross a thread boundary fails to
//! compile instead of silently working. The debug engine is the plan-A1
//! slice: one fixed-row table per worker, subset SQL parsed by
//! [`crate::sql`]. Results are written through [`RowBatcher`], which
//! frames DataRows in vectorized batches of `batch_rows` (FR-16).

use std::{
    cell::RefCell,
    io,
    os::unix::io::FromRawFd,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use cascade_telemetry::Span;
use cascade_tenant::TenantContext;
use cascade_types::{Lsn, SegmentId};
use cascade_wal::{WalError, WalHandle, WalRecord, WalWriter};
use futures_lite::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use static_assertions::assert_not_impl_any;

use crate::{
    handoff::Handoff,
    sql::{Col, SqlError, Stmt, TableKind},
    tenant::{dev_context, TenantCounters},
    tierc::TierCAck,
};

// FR-14, compile-enforced: if this ever starts to fail, connection state
// grew a cross-thread path — the design, not the assertion, is wrong.
assert_not_impl_any!(Session: Send, Sync);

/// Column type OIDs (text format on the wire).
const INT4_OID: i32 = 23;
const TEXT_OID: i32 = 25;

type Row = (i32, String);

/// The debug table's storage segment in WAL records (one logical table in
/// the debug slice; the catalog assigns real segments in later milestones).
const DEV_TABLE_SEG: SegmentId = SegmentId(1);

/// Process-wide transaction-id source for WAL records. The debug slice runs
/// one process; per-DB xid generators come with the catalog.
static NEXT_XID: AtomicU64 = AtomicU64::new(1);

fn next_xid() -> u64 {
    NEXT_XID.fetch_add(1, Ordering::Relaxed)
}

/// Per-connection WAL wiring (T1.11, FR-27): the DB's shared writer plus
/// *this reactor's* handle. The writer is `Arc`-shared across workers; the
/// handle is opened on — and stays on — this worker's ring (I2), shared by
/// all its sessions via `Rc`.
pub struct Wal {
    pub writer: Arc<WalWriter>,
    pub handle: Rc<WalHandle>,
}

/// The worker-shared debug table: `debug_table(id INT, val TEXT)`, one
/// instance per worker reactor. Sessions on the same worker see each
/// other's committed rows; cross-worker visibility is out of slice scope
/// (the catalog, not the session, owns placement in later milestones).
#[derive(Default)]
pub struct Table {
    rows: RefCell<Vec<Row>>,
}

impl Table {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds replayed rows (T3.3 recovery): the table's initial contents,
    /// applied before any session runs on this reactor. Rows arrive in
    /// commit order — the order the previous boot acked them in.
    pub fn seed(&self, rows: &[(i32, String)]) {
        self.rows.borrow_mut().extend_from_slice(rows);
    }

    fn insert(&self, row: Row) {
        self.rows.borrow_mut().push(row);
    }

    /// Snapshot: values are copied out so no borrow is held across an
    /// await (another session on this reactor may run during the flush).
    fn snapshot(&self) -> Vec<Row> {
        self.rows.borrow().clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxnStatus {
    Idle,
    InTxn,
    /// A statement failed inside the transaction; only ROLLBACK (and
    /// COMMIT, which acts as rollback) is accepted until then.
    Failed,
}

/// One connection's Tier A state. `Rc` field ⇒ `!Send` by construction.
pub struct Session {
    table: Rc<Table>,
    pending: Vec<Row>,
    txn: TxnStatus,
}

impl Session {
    fn new(table: Rc<Table>) -> Self {
        Session {
            table,
            pending: Vec::new(),
            txn: TxnStatus::Idle,
        }
    }

    fn rfb_byte(&self) -> u8 {
        match self.txn {
            TxnStatus::Idle => b'I',
            TxnStatus::InTxn => b'T',
            TxnStatus::Failed => b'E',
        }
    }
}

/// A wire-level error: SQLSTATE code + message, ready for ErrorResponse.
#[derive(Debug, Clone)]
pub struct PgError {
    code: &'static str,
    message: String,
}

impl PgError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        PgError {
            code,
            message: message.into(),
        }
    }
}

impl From<SqlError> for PgError {
    fn from(e: SqlError) -> Self {
        PgError::new(e.code(), e.to_string())
    }
}

impl From<WalError> for PgError {
    fn from(e: WalError) -> Self {
        match e {
            WalError::Io(_) => PgError::new("58000", format!("wal flush failed: {e}")),
            // A torn tail at *flush* time means the watermarks let a flush
            // cover a torn record — impossible by protocol, so it lands in
            // the same integrity bucket as corruption (not a client's
            // fault, never expected).
            WalError::Corruption(_) | WalError::TornTail(_) | WalError::UnknownRecordVersion(_) => {
                PgError::new("XX000", format!("wal integrity failure: {e}"))
            }
        }
    }
}

/// Vectorized row writer (FR-16): DataRows accumulate in one buffer and
/// hit the socket in batches of `batch_rows`; every other message is a
/// direct write. Generic so the batching/framing is unit-testable without
/// a reactor.
struct RowBatcher<W> {
    sink: W,
    buf: Vec<u8>,
    buffered_rows: u32,
    batch_rows: u32,
}

impl<W: AsyncWrite + Unpin> RowBatcher<W> {
    fn new(sink: W, batch_rows: u32) -> Self {
        RowBatcher {
            sink,
            buf: Vec::new(),
            buffered_rows: 0,
            batch_rows,
        }
    }

    /// Buffer one DataRow with text-format fields.
    fn row(&mut self, fields: &[&[u8]]) {
        let body: usize = 2 + fields.iter().map(|f| 4 + f.len()).sum::<usize>();
        self.buf.push(b'D');
        self.buf
            .extend_from_slice(&((body + 4) as i32).to_be_bytes());
        self.buf
            .extend_from_slice(&(fields.len() as i16).to_be_bytes());
        for f in fields {
            self.buf.extend_from_slice(&(f.len() as i32).to_be_bytes());
            self.buf.extend_from_slice(f);
        }
        self.buffered_rows += 1;
    }

    fn should_flush(&self) -> bool {
        self.buffered_rows >= self.batch_rows
    }

    async fn flush(&mut self) -> io::Result<()> {
        if self.buffered_rows > 0 {
            self.sink.write_all(&self.buf).await?;
            self.buf.clear();
            self.buffered_rows = 0;
        }
        Ok(())
    }

    /// Direct framed write of one message (RowDescription, CommandComplete,
    /// ErrorResponse, ReadyForQuery — the control messages a client synchronizes
    /// on; only DataRows batch).
    async fn send(&mut self, msg: u8, payload: &[u8]) -> io::Result<()> {
        self.sink.write_all(&[msg]).await?;
        self.sink
            .write_all(&((payload.len() as i32 + 4).to_be_bytes()))
            .await?;
        self.sink.write_all(payload).await
    }

    async fn send_error(&mut self, err: &PgError) -> io::Result<()> {
        let mut payload = Vec::new();
        for (tag, value) in [
            (b'S', "ERROR"),
            (b'V', "ERROR"),
            (b'C', err.code),
            (b'M', err.message.as_str()),
        ] {
            payload.push(tag);
            payload.extend_from_slice(value.as_bytes());
            payload.push(0);
        }
        payload.push(0);
        self.send(b'E', &payload).await
    }
}

/// Largest accepted message body: the debug subset needs tiny messages, so
/// anything past 1 MiB is a protocol violation, not a `vec![]` this size.
/// Bound checked *before* allocating — a hostile length header must never
/// become a worker-memory bomb.
const MAX_MESSAGE_LEN: usize = 1 << 20;

/// Read one framed message. `Ok(None)` on EOF/close; `Err` on a length
/// header that violates the framing contract.
async fn read_message<W: AsyncRead + Unpin>(sink: &mut W) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 5];
    match sink.read_exact(&mut hdr).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if !(4..=MAX_MESSAGE_LEN + 4).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad message length {len}"),
        ));
    }
    let mut payload = vec![0u8; len - 4];
    sink.read_exact(&mut payload).await?;
    Ok(Some((hdr[0], payload)))
}

/// One framed message, interruptible by the idle reaper (T4.5). While
/// parked here the session marks itself `waiting` in its registry entry
/// — the ONLY state from which the reaper may condemn it (a session
/// mid-statement is not idle, whatever the clock says). If the reaper
/// condemned the connection during this wait, returns `Ok(None)` —
/// indistinguishable from EOF at this layer — so the session exits
/// through the ordinary FR-15 cleanup path. The waker is re-stored on
/// every poll: the reaper's wake must land on the waker of the poll
/// that is actually parked. `reap == None` (reaping disabled) is the
/// plain read, zero overhead.
async fn reap_message<W: AsyncRead + Unpin>(
    reap: Option<&Rc<crate::reaper::ReapRegistry>>,
    id: Option<crate::reaper::ConnId>,
    sink: &mut W,
) -> io::Result<Option<(u8, Vec<u8>)>> {
    let (reg, id) = match (reap, id) {
        (Some(r), Some(i)) => (r, i),
        _ => return read_message(sink).await,
    };
    reg.set_waiting(id, true);
    let mut inner = std::pin::pin!(read_message(sink));
    let res = std::future::poll_fn(|cx| {
        if reg.is_dropped(id) {
            return std::task::Poll::Ready(Ok(None));
        }
        reg.store_waker(id, cx.waker());
        std::future::Future::poll(inner.as_mut(), cx)
    })
    .await;
    reg.set_waiting(id, false);
    res
}

/// One await-able window, interruptible by the reaper — [`reap_message`]
/// generalized from the message read to the STATEMENT window (the stall
/// rule's rescue path). The waker is re-stored on every poll, so the
/// reaper's wake lands on the poll that is actually parked — mid-execute,
/// mid-WAL, or mid-response-write, wherever the statement currently
/// awaits. If the reaper condemned the connection during this window,
/// returns `None`: the caller must close WITHOUT further protocol output
/// (the wedged op may itself be a response write, so the stream's state
/// is unknown) and let the ordinary FR-15 cleanup below run. `reap ==
/// None` (reaping disabled) is the plain body, zero overhead.
async fn rescoped<F: std::future::Future>(
    reap: Option<&Rc<crate::reaper::ReapRegistry>>,
    id: Option<crate::reaper::ConnId>,
    body: F,
) -> Option<F::Output> {
    let (reg, id) = match (reap, id) {
        (Some(r), Some(i)) => (r, i),
        _ => return Some(body.await),
    };
    let mut inner = std::pin::pin!(body);
    std::future::poll_fn(|cx| {
        if reg.is_dropped(id) {
            return std::task::Poll::Ready(None);
        }
        reg.store_waker(id, cx.waker());
        std::future::Future::poll(inner.as_mut(), cx).map(Some)
    })
    .await
}

/// The session's Tier C wiring (T5.4), handed over by its worker: the
/// write-funnel route (the DDL bounce's first hop — FR-29), this
/// reactor's core id (the fan-out's issuer-first ordering key), and the
/// worker's own catalog (the pointer-load read behind `debug_roles()`).
/// All three are ring-local by construction — the handle is `Rc`-based,
/// the catalog is an `Arc` read only from this ring.
pub struct Tierc {
    pub(crate) write: crate::submit::SubmitHandle,
    pub(crate) core: u32,
    pub(crate) catalog: std::sync::Arc<crate::tierc::Catalog>,
}

/// Per-connection environment handed to every session by its worker: the
/// pieces of the shutdown protocol (T4.4) the session honors between
/// statements. `draining` is §10.2 step 2b — once set, the *next*
/// statement is refused with 57P01 and the connection closes (the current
/// one always finishes). `inflight` is the per-worker in-flight-statement
/// gauge the drain waits on: an idle connection holds it at zero, so
/// idle clients never block a drain — only a statement actually being
/// processed does.
#[derive(Clone)]
pub struct SessionEnv {
    pub draining: Arc<AtomicBool>,
    pub inflight: Arc<AtomicU64>,
}

/// Balances one in-flight increment on every exit, unwinding included.
/// A dropped decrement would stick the worker's gauge above zero forever,
/// silently converting every later shutdown of that worker into the
/// deadline path (§10.2 step 2c) — so the decrement is the guard's Drop,
/// not a line at the bottom of the statement arm.
struct GaugeGuard<'a>(&'a AtomicU64);

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Unregisters the connection's reaper entry on every exit, unwinding
/// included. A zombie entry would sit in the registry forever — not
/// re-reapable (it's already dropped or removed from eligibility), but
/// never removed either, a slow leak per panicked session and a growing
/// walk for every reap pass on this ring.
struct ReapGuard<'a>(&'a Rc<crate::reaper::ReapRegistry>, crate::reaper::ConnId);

impl Drop for ReapGuard<'_> {
    fn drop(&mut self) {
        self.0.unregister(self.1);
    }
}

/// The session task a worker spawns per handed-off connection: simple-query
/// loop until Terminate or disconnect. The tenant context and span are
/// constructed once here and *borrowed* through every executed statement
/// (FR-20/FR-21 shape, from birth).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    handoff: Handoff,
    table: Rc<Table>,
    stats: Arc<crate::stats::WorkerCounters>,
    tenants: Arc<TenantCounters>,
    wal: Option<Wal>,
    heap: Option<std::sync::Arc<crate::demoheap::DemoHeap>>,
    tierc: Option<Tierc>,
    batch_rows: u32,
    env: SessionEnv,
    reap: Option<Rc<crate::reaper::ReapRegistry>>,
) {
    stats.connections.fetch_add(1, Ordering::Relaxed);
    // The idle reaper (T4.5, FR-34): one registry entry per connection,
    // armed before the first read and unregistered by an RAII guard on
    // every exit — a panic must not leave a zombie entry for every later
    // reap pass to reconsider (the GaugeGuard discipline). Idle runs
    // from each statement's completion (`touch` below), never from
    // connection age.
    let reap_id = reap.as_ref().map(|r| r.register());
    let _reap_guard = reap
        .as_ref()
        .zip(reap_id)
        .map(|(r, id)| ReapGuard(r, id));
    // SAFETY: the fd crossed exactly once via the shared channel and this
    // task is its sole owner — adopted onto *this* reactor's ring (I2).
    let stream = unsafe { glommio::net::TcpStream::from_raw_fd(handoff.fd) };
    let mut out = RowBatcher::new(stream, batch_rows);
    // Slice scope: the debug engine's fixed dev tenant. Real database-name →
    // context mapping arrives with the catalog (plan A1).
    let ctx = dev_context();
    let span = Span::noop();
    let mut sess = Session::new(table);

    loop {
        let msg = reap_message(reap.as_ref(), reap_id, &mut out.sink).await;
        // Everything after the read — framing refusals, the draining
        // refusal, statement execution, and every response write — runs
        // under ONE rescue wrapper: the reaper's stall rule (`rescoped`
        // above) condemns a statement wedged past `stall_reap_ms` on an
        // io_uring completion that never arrives (the socket-op sibling
        // of cascade-wal's OP_WATCHDOG: the WAL ops are idempotent and
        // re-issue, socket ops are neither, so the backend is killed —
        // PG's own termination of a stuck backend), and this side aborts
        // — `None` — closing through the ordinary FR-15 cleanup below
        // instead of hanging forever. Every close the arms used to spell
        // `break` maps to `true`; the loop breaks on anything but a
        // clean `Some(false)`.
        let close = rescoped(reap.as_ref(), reap_id, async {
            match msg {
                Ok(Some((b'Q', payload))) => {
                    // §10.2 step 2b: draining refuses the *next* statement —
                    // anything already read before the flag was set still
                    // ran. Fatal severity, so the connection closes (PG
                    // semantics for 57P01), and FR-15's cleanup below runs.
                    if env.draining.load(Ordering::Relaxed) {
                        let err = PgError::new(
                            crate::shutdown::ADMIN_SHUTDOWN,
                            "terminating connection: the server is shutting down",
                        );
                        let _ = out.send_error(&err).await;
                        return true;
                    }
                    // The drain's wait counts this statement as in-flight
                    // until its ReadyForQuery went out (§10.2 step 2c). The
                    // guard below balances the increment on EVERY exit —
                    // including a panic unwinding this task, which a manual
                    // decrement at the bottom would miss and which would stick
                    // the gauge above zero forever, silently turning every
                    // later shutdown into the deadline path (T4.4 review).
                    env.inflight.fetch_add(1, Ordering::Relaxed);
                    let _inflight = GaugeGuard(&env.inflight);
                    // The stall rule's clock starts here — the statement's
                    // first await-able moment — and stops at the completion
                    // touch below. A statement wedged past `stall_reap_ms`
                    // (e.g. on a lost completion) is condemned even though
                    // it is not parked in its read; the clock's clearing is
                    // symmetric with the increment, and every other exit
                    // path unregisters the entry outright (ReapGuard).
                    if let (Some(r), Some(id)) = (reap.as_ref(), reap_id) {
                        r.set_in_flight(id, true);
                    }
                    let mut closed = false;
                    // §10.2 step 2b's race window: the flag may have flipped
                    // between the check above and our increment. Re-check with
                    // the statement counted — the drain wait can then never
                    // declare victory over a statement it never saw.
                    if env.draining.load(Ordering::Relaxed) {
                        let err = PgError::new(
                            crate::shutdown::ADMIN_SHUTDOWN,
                            "terminating connection: the server is shutting down",
                        );
                        let _ = out.send_error(&err).await;
                        closed = true;
                    }
                    // Payload is one cstring — except the empty query, which has
                    // no payload at all and answers EmptyQueryResponse. The two
                    // framing failures answer and mark `closed`; `sql` is then
                    // unused.
                    let sql: &str = if closed || payload.is_empty() {
                        ""
                    } else if let Some(nul) = payload.iter().position(|&b| b == 0) {
                        match std::str::from_utf8(&payload[..nul]) {
                            Ok(s) => s,
                            Err(_) => {
                                let e = PgError::new("08P01", "query string is not UTF-8");
                                let _ = out.send_error(&e).await;
                                closed = true;
                                ""
                            }
                        }
                    } else {
                        let e = PgError::new("08P01", "unterminated query string");
                        let _ = out.send_error(&e).await;
                        closed = true;
                        ""
                    };
                    if !closed {
                        // Real PG: a multi-statement query string stops at the first
                        // error; remaining statements are skipped, then RFB closes
                        // the batch. An entirely empty query answers
                        // EmptyQueryResponse.
                        let stmts = crate::sql::split_statements(sql);
                        let mut failed = false;
                        if stmts.is_empty() {
                            let _ = out.send(b'I', &[]).await;
                        }
                        for stmt_sql in stmts {
                            if failed {
                                break;
                            }
                            match crate::sql::parse(stmt_sql.trim()) {
                                Ok(stmt) => {
                                    match execute(
                                        &mut sess,
                                        stmt,
                                        &mut out,
                                        &ctx,
                                        &span,
                                        &tenants,
                                        &stats,
                                        wal.as_ref(),
                                        heap.as_ref(),
                                        tierc.as_ref(),
                                    )
                                    .await
                                    {
                                        Ok(()) => {}
                                        Err(e) => {
                                            let _ = out.send_error(&e).await;
                                            if sess.txn == TxnStatus::InTxn {
                                                sess.txn = TxnStatus::Failed;
                                            }
                                            failed = true;
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = out.send_error(&PgError::from(e)).await;
                                    if sess.txn == TxnStatus::InTxn {
                                        sess.txn = TxnStatus::Failed;
                                    }
                                    failed = true;
                                }
                            }
                        }
                        if out.send(b'Z', &[sess.rfb_byte()]).await.is_err() {
                            closed = true;
                        }
                        // The statement just COMPLETED — the RFB is out. This
                        // is where the reaper's idle clock resets: idle is
                        // measured from last completion, never from connection
                        // age (T4.5) — and where the stall clock stops.
                        if let (Some(r), Some(id)) = (reap.as_ref(), reap_id) {
                            r.set_in_flight(id, false);
                            r.touch(id);
                        }
                    }
                    // `_inflight` (the gauge guard) drops here — or on the
                    // early returns above, or on unwind — balancing the
                    // increment.
                    closed
                }
                // Terminate: clean client goodbye.
                Ok(Some((b'X', _))) => true,
                // Extended protocol or anything else desyncs our framing —
                // respond and close rather than misread the stream (slice runs
                // the simple protocol only, FR-16).
                Ok(Some((t, _))) => {
                    let e = PgError::new(
                        "08P01",
                        format!("protocol violation: unsupported message type {:?}", t),
                    );
                    let _ = out.send_error(&e).await;
                    true
                }
                // EOF — or the idle reaper condemned the connection while it
                // waited for the client; indistinguishable here, both close.
                Ok(None) => true,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    // Framing violation (bad length): answer, then close — the
                    // stream can no longer be trusted.
                    let err = PgError::new("08P01", format!("protocol violation: {e}"));
                    let _ = out.send_error(&err).await;
                    true
                }
                Err(_) => true,
            }
        })
        .await;
        // `None` is the stall abort: close with no further protocol output
        // (the wedged op may itself have been a response write);
        // `Some(true)` is every ordinary close.
        if close != Some(false) {
            break;
        }
    }
    // Disconnect cleanup (FR-15): a mid-txn disconnect aborts. Pending rows
    // were never WAL-appended (the WAL write happens at COMMIT, so nothing
    // of this txn was reserved) — discarding them leaves nothing behind for
    // replay. Page pins, statements, and the per-query arena arrive with the
    // buffer-pool-backed engine; scope exit drops them in order regardless.
    if sess.txn != TxnStatus::Idle {
        sess.pending.clear();
        sess.txn = TxnStatus::Idle;
        // pg_stat_database parity: a transaction abandoned mid-flight —
        // by disconnect, protocol violation, or the reaper (T4.5) — is
        // a rolled-back transaction (T4.6).
        stats.rollbacks.fetch_add(1, Ordering::Relaxed);
    }
    drop(out); // closes the fd
    stats.connections.fetch_sub(1, Ordering::Relaxed);
}

/// Execute one parsed statement, writing its full result set. Errors carry
/// the SQLSTATE the client sees. `wal` is the connection's WAL wiring (or
/// `None` for the in-memory-only debug engine); `heap` the demo relation
/// when booted with one.
#[allow(clippy::too_many_arguments)] // each param is a distinct concern; a bag struct would only hide that
async fn execute<W: AsyncWrite + Unpin>(
    sess: &mut Session,
    stmt: Stmt,
    out: &mut RowBatcher<W>,
    ctx: &TenantContext,
    span: &Span,
    tenants: &TenantCounters,
    stats: &crate::stats::WorkerCounters,
    wal: Option<&Wal>,
    heap: Option<&std::sync::Arc<crate::demoheap::DemoHeap>>,
    tierc: Option<&Tierc>,
) -> Result<(), PgError> {
    // The hot-path shape (FR-20/FR-21): context and span borrowed, per
    // statement, nothing global.
    crate::tenant::debug_hot_path(ctx, span, tenants);
    // FR-33: attempted statements (parse errors never got here).
    // Empty statements (";;") are protocol no-ops — an
    // EmptyQueryResponse and nothing else — and real PostgreSQL counts
    // neither a command nor a transaction for them.
    if !matches!(stmt, Stmt::Empty) {
        stats.statements.fetch_add(1, Ordering::Relaxed);
    }

    // Aborted transaction: every statement but ROLLBACK is refused (PG 25P02);
    // COMMIT acts as ROLLBACK.
    if sess.txn == TxnStatus::Failed && !matches!(stmt, Stmt::Rollback) {
        if stmt == Stmt::Commit {
            sess.pending.clear();
            sess.txn = TxnStatus::Idle;
            stats.rollbacks.fetch_add(1, Ordering::Relaxed);
            return send_tag(out, "ROLLBACK").await;
        }
        return Err(PgError::new(
            "25P02",
            "current transaction is aborted, commands ignored until end of transaction block",
        ));
    }

    match stmt {
        Stmt::Empty => out.send(b'I', &[]).await.map_err(io_err),
        Stmt::Begin => {
            if sess.txn == TxnStatus::InTxn {
                return Err(PgError::new(
                    "25001",
                    "there is already a transaction in progress",
                ));
            }
            sess.txn = TxnStatus::InTxn;
            send_tag(out, "BEGIN").await
        }
        Stmt::Commit => {
            match sess.txn {
                TxnStatus::Idle => {
                    return Err(PgError::new("25P01", "there is no transaction in progress"))
                }
                TxnStatus::InTxn => {
                    // FR-27 order: WAL append → fdatasync → *then* apply and
                    // ack. A flush failure errors before the table is
                    // touched and before the client sees COMMIT — the txn
                    // becomes Failed, and its COMMIT (or ROLLBACK) discards
                    // the pending rows.
                    if let Some(wal) = wal {
                        durable_commit(wal, &sess.pending).await?;
                    }
                    for row in sess.pending.drain(..) {
                        sess.table.insert(row);
                    }
                    stats.commits.fetch_add(1, Ordering::Relaxed);
                }
                TxnStatus::Failed => unreachable!("handled above"),
            }
            sess.txn = TxnStatus::Idle;
            send_tag(out, "COMMIT").await
        }
        Stmt::Rollback => {
            if sess.txn == TxnStatus::Idle {
                return Err(PgError::new("25P01", "there is no transaction in progress"));
            }
            sess.pending.clear();
            sess.txn = TxnStatus::Idle;
            stats.rollbacks.fetch_add(1, Ordering::Relaxed);
            send_tag(out, "ROLLBACK").await
        }
        Stmt::Insert { id, val } => {
            let row = (id, val);
            match sess.txn {
                TxnStatus::InTxn => sess.pending.push(row),
                // An autocommit INSERT is its own transaction: same FR-27
                // order — its WAL record is durable before the ack.
                TxnStatus::Idle => {
                    if let Some(wal) = wal {
                        // A failed autocommit write is an aborted
                        // implicit transaction (PG counts xact_rollback
                        // for it); the txn never left Idle, so no later
                        // rollback path would count it — count it here
                        // or it vanishes from the ledger (unlike a
                        // failed explicit COMMIT, whose txn is still
                        // InTxn and deterministically ends in exactly
                        // one counted rollback downstream).
                        if let Err(e) =
                            durable_commit(wal, std::slice::from_ref(&row)).await
                        {
                            stats.rollbacks.fetch_add(1, Ordering::Relaxed);
                            return Err(e);
                        }
                    }
                    sess.table.insert(row);
                    stats.commits.fetch_add(1, Ordering::Relaxed);
                }
                TxnStatus::Failed => unreachable!("handled above"),
            }
            send_tag(out, "INSERT 0 1").await
        }
        Stmt::Select { table, cols, filter } => match table {
            TableKind::Debug => {
                out.send(b'T', &row_description(&cols))
                    .await
                    .map_err(io_err)?;
                // Own uncommitted inserts are visible inside the
                // transaction; committed rows come from the table.
                // (Insertion order across the two sources is not
                // interleaved — accepted slice cut.)
                let mut rows = sess.pending.clone();
                rows.extend(sess.table.snapshot());
                // The point filter (WHERE id = <i32>) — the demo's lookup
                // shape.
                if let Some(id) = filter {
                    rows.retain(|(rid, _)| *rid == id);
                }
                for (id, val) in &rows {
                    let id = id.to_string();
                    let fields: Vec<&[u8]> = cols
                        .iter()
                        .map(|c| match c {
                            Col::Id => id.as_bytes(),
                            Col::Val => val.as_bytes(),
                            Col::Payload => unreachable!("debug_table has no payload"),
                        })
                        .collect();
                    out.row(&fields);
                    if out.should_flush() {
                        out.flush().await.map_err(io_err)?;
                        // §7.1: every vectorized batch boundary yields.
                        crate::coop::yield_point().await;
                    }
                }
                out.flush().await.map_err(io_err)?;
                send_tag(out, &format!("SELECT {}", rows.len())).await
            }
            TableKind::Demo => {
                // Catalog presence: the relation exists only when the
                // runtime was booted with demo_rows > 0.
                let Some(heap) = heap else {
                    return Err(PgError::new("42P01", "relation \"demo\" does not exist"));
                };
                demo_select(heap, &cols, filter, out).await
            }
        },
        Stmt::DebugWorker => {
            out.send(b'T', &debug_worker_description())
                .await
                .map_err(io_err)?;
            let name = std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_owned();
            out.row(&[name.as_bytes()]);
            out.flush().await.map_err(io_err)?;
            send_tag(out, "SELECT 1").await
        }
        Stmt::DebugRoles => {
            // T5.4's observable: the role names live in THIS reactor's
            // Tier C snapshot. The read is the FR-28 pointer copy —
            // names are extracted synchronously inside the guard's
            // scope (a Guard never spans an .await), then the wire
            // writes happen from the owned Vec. No invalidation exists
            // in the system: a role appears here exactly when this
            // reactor's catalog adopted a newer snapshot, which is the
            // property the DDL test asserts across workers.
            let Some(tierc) = tierc else {
                return Err(PgError::new(
                    "58000",
                    "the Tier C catalog is not wired to this session",
                ));
            };
            let names: Vec<String> = {
                let guard = tierc.catalog.load();
                let Some(snap) = guard.as_ref() else {
                    return Err(PgError::new(
                        crate::tierc::CANNOT_CONNECT_NOW,
                        "the database system is starting up",
                    ));
                };
                snap.roles().map(str::to_owned).collect()
            };
            out.send(b'T', &debug_roles_description())
                .await
                .map_err(io_err)?;
            for name in &names {
                out.row(&[name.as_bytes()]);
            }
            out.flush().await.map_err(io_err)?;
            send_tag(out, &format!("SELECT {}", names.len())).await
        }
        Stmt::CreateRole { name } => {
            // FR-29's DDL path (T5.4): the statement LEAVES the session —
            // it bounces through the write channel to the serialized Tier
            // C writer, and the reply is built from the writer's ack.
            // Nothing role-shaped is decided, cached, or stored here.
            //
            // In a transaction block it is refused: the writer applies at
            // once, so pretending the change would roll back with the
            // block would lie — the same refusal PG gives statements that
            // cannot join a block (25001).
            if sess.txn == TxnStatus::InTxn {
                return Err(PgError::new(
                    "25001",
                    "CREATE ROLE cannot run inside a transaction block",
                ));
            }
            let Some(tierc) = tierc else {
                return Err(PgError::new(
                    "58000",
                    "the Tier C mutation path is not wired to this session",
                ));
            };
            let issuer = tierc.core;
            let change = crate::tierc::TierCChange::CreateRole { name };
            // The T5.1 crossing: the closure (issuer + change) rides the
            // write channel to the serving housekeeper; the future runs
            // THERE (the funnel is a thread_local on that ring) and only
            // the ack comes back. The query path's submit_to arms are
            // exactly the DDL ones (CREATE ROLE and, since T5.5, CREATE
            // DATABASE) — OLTP never takes them (FR-36, pinned by the
            // zero-submit_to probe in tests/ddl.rs).
            match tierc
                .write
                .submit_to(move || async move { crate::tierc::submit_change(issuer, change).await })
                .await
            {
                // Both confirmations answer CREATE ROLE: the change IS in
                // the writer's state and stored on THIS reactor (the
                // issuer's store is awaited first); Pending only names
                // laggards elsewhere, whose stores land lazily — never
                // lost.
                Ok(Ok(TierCAck::Applied)) | Ok(Ok(TierCAck::Pending(_))) => {
                    send_tag(out, "CREATE ROLE").await
                }
                // Some stores can NEVER land (refused or lost — M3): the
                // change DID apply at the writer, but the client must not
                // hear unqualified success — a reactor may never adopt
                // it, and a retry would answer 42710 duplicate. The error
                // says both halves of that truth.
                Ok(Ok(TierCAck::PublicationLost(n))) => Err(PgError::new(
                    "58000",
                    format!(
                        "the change applied at the Tier C writer, but {n} publication(s) \
                         can never reach their reactors (refused or lost); this system \
                         may serve stale roles until restart, and retrying reports a \
                         duplicate"
                    ),
                )),
                // The writer refused the change (e.g. duplicate role,
                // 42710): its snapshot is unchanged, nothing fanned out,
                // and the client gets the writer's own SQLSTATE.
                Ok(Ok(TierCAck::Failed(e))) => {
                    let (code, message) = e.pg();
                    Err(PgError::new(code, message))
                }
                // The transport lost the round trip (writer gone,
                // funnel refused, ring dead): honest failure at the
                // caller — nothing half-applied anywhere.
                Ok(Err(e)) | Err(e) => Err(PgError::new(
                    "58000",
                    format!("the Tier C writer did not confirm the change: {e}"),
                )),
            }
        }
        Stmt::DebugDatabases => {
            // T5.5's observable, on the same terms as debug_roles: the
            // per-database affinity rows live in THIS reactor's snapshot,
            // read by a pointer copy with the extraction synchronous
            // inside the guard's scope. `numa` renders `-` when the row
            // binds nothing (`cross`, or the cluster has no workers) —
            // the row writer carries text fields only.
            let Some(tierc) = tierc else {
                return Err(PgError::new(
                    "58000",
                    "the Tier C catalog is not wired to this session",
                ));
            };
            let rows: Vec<(String, &'static str, Option<u32>)> = {
                let guard = tierc.catalog.load();
                let Some(snap) = guard.as_ref() else {
                    return Err(PgError::new(
                        crate::tierc::CANNOT_CONNECT_NOW,
                        "the database system is starting up",
                    ));
                };
                snap.databases()
                    .map(|(name, affinity, numa)| (name.to_owned(), affinity, numa))
                    .collect()
            };
            out.send(b'T', &debug_databases_description())
                .await
                .map_err(io_err)?;
            for (name, affinity, numa) in &rows {
                let numa = numa
                    .map(|n| n.to_string().into_bytes())
                    .unwrap_or_else(|| b"-".to_vec());
                out.row(&[name.as_bytes(), affinity.as_bytes(), &numa]);
            }
            out.flush().await.map_err(io_err)?;
            send_tag(out, &format!("SELECT {}", rows.len())).await
        }
        Stmt::AlterDatabaseAffinity { name, .. } => {
            // FR-38's honest refusal: the statement parsed, its value was
            // validated at resolution (a bad value never got here), and
            // the answer is unsupported_feature — the migration runner
            // (§2.5.4) is a Phase 7+ deliverable. Nothing bounces, no
            // snapshot changes, no migration is attempted: affinity
            // changes later mean re-homing the DB's whole Tier B set,
            // and pretending that happened would be the lie the review
            // bullet warns about.
            Err(PgError::new(
                "0A000",
                format!(
                    "ALTER DATABASE {name} SET numa_affinity is not supported by this engine \
                     (the migration runner is a later deliverable)"
                ),
            ))
        }
        Stmt::CreateDatabase { name, affinity } => {
            // T5.5's DDL, the CreateRole bounce exactly: leave the
            // session through the write channel, let the ONE writer
            // apply it, answer from the ack. Refused in a transaction
            // block like its sibling (PG refuses CREATE DATABASE in a
            // block too — it cannot roll back).
            if sess.txn == TxnStatus::InTxn {
                return Err(PgError::new(
                    "25001",
                    "CREATE DATABASE cannot run inside a transaction block",
                ));
            }
            let Some(tierc) = tierc else {
                return Err(PgError::new(
                    "58000",
                    "the Tier C mutation path is not wired to this session",
                ));
            };
            let issuer = tierc.core;
            let change = crate::tierc::TierCChange::CreateDatabase { name, affinity };
            // Same crossing as CREATE ROLE (the query path's submit_to
            // arms are the DDL ones — OLTP never takes them, FR-36).
            match tierc
                .write
                .submit_to(move || async move { crate::tierc::submit_change(issuer, change).await })
                .await
            {
                Ok(Ok(TierCAck::Applied)) | Ok(Ok(TierCAck::Pending(_))) => {
                    send_tag(out, "CREATE DATABASE").await
                }
                // Same honesty as CREATE ROLE's arm (M3): the writer
                // applied the change, but a store that can never land
                // must not read as success — routing may never adopt it
                // (stale bound NUMA).
                Ok(Ok(TierCAck::PublicationLost(n))) => Err(PgError::new(
                    "58000",
                    format!(
                        "the change applied at the Tier C writer, but {n} publication(s) \
                         can never reach their reactors (refused or lost); routing may \
                         serve stale placement until restart, and retrying reports a \
                         duplicate"
                    ),
                )),
                Ok(Ok(TierCAck::Failed(e))) => {
                    let (code, message) = e.pg();
                    Err(PgError::new(code, message))
                }
                Ok(Err(e)) | Err(e) => Err(PgError::new(
                    "58000",
                    format!("the Tier C writer did not confirm the change: {e}"),
                )),
            }
        }
    }
}

/// Make one transaction durable, in FR-27 order, and return the commit
/// record's end boundary (⇒ `durable >= boundary` on `Ok`). Every
/// reservation of the transaction is taken *before* the first fill — the
/// COMMIT record's byte range exists before any earlier record becomes
/// flushable — so a crash mid-fill leaves an unfilled hole at the tear
/// point and the filled watermark (therefore every flush) stops there: no
/// partial transaction can ever become durable or be acked. The records
/// need not be contiguous in the log (another reactor's session can
/// reserve in between); the watermark protocol is indifferent to that.
async fn durable_commit(wal: &Wal, rows: &[Row]) -> Result<Lsn, PgError> {
    let xid = next_xid();
    let writer = &wal.writer;
    let begin = writer.append_wal(&WalRecord::BeginTxn { xid });
    let mut inserts = Vec::with_capacity(rows.len());
    for (id, val) in rows {
        inserts.push(writer.append_wal(&WalRecord::Insert {
            xid,
            table: DEV_TABLE_SEG,
            id: *id,
            val: val.clone(),
        }));
    }
    let commit = writer.append_wal(&WalRecord::CommitTxn { xid });
    let end = commit.end();

    // Crash-point barrier (T3.3 harness): every reservation exists, no
    // byte is visible yet. A kill here replays as "nothing happened".
    crate::recovery::crash_barrier("reserved").await;
    let _ = begin.fill();
    for r in inserts {
        let _ = r.fill();
    }
    // Barrier: begin + inserts filled and visible, the commit record
    // reserved but unfilled. A kill here replays as "transaction rolled
    // back" — the filled watermark stops short of the commit record, so
    // no flush ever covered it.
    crate::recovery::crash_barrier("midfill").await;
    let _ = commit.fill();

    writer.flush_wal(&wal.handle, end).await?;
    // Barrier: durable, not yet acked — the window FR-27's ordering
    // exists for. A kill here replays a commit the client never saw
    // (in-doubt); the replay applies it either way.
    crate::recovery::crash_barrier("synced").await;
    Ok(end)
}

async fn send_tag<W: AsyncWrite + Unpin>(
    out: &mut RowBatcher<W>,
    tag: &str,
) -> Result<(), PgError> {
    let mut payload = tag.as_bytes().to_vec();
    payload.push(0);
    out.send(b'C', &payload).await.map_err(io_err)
}

fn io_err(e: io::Error) -> PgError {
    PgError::new("58000", format!("io failure writing results: {e}"))
}

fn field(name: &str, oid: i32, typlen: i16) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(name.as_bytes());
    f.push(0);
    f.extend_from_slice(&0i32.to_be_bytes()); // table oid
    f.extend_from_slice(&0i16.to_be_bytes()); // attnum
    f.extend_from_slice(&oid.to_be_bytes());
    f.extend_from_slice(&typlen.to_be_bytes());
    f.extend_from_slice(&(-1i32).to_be_bytes()); // typmod
    f.extend_from_slice(&0i16.to_be_bytes()); // text format
    f
}

fn row_description(cols: &[Col]) -> Vec<u8> {
    let mut payload = (cols.len() as i16).to_be_bytes().to_vec();
    for c in cols {
        match c {
            Col::Id => payload.extend(field("id", INT4_OID, 4)),
            Col::Val => payload.extend(field("val", TEXT_OID, -2)),
            Col::Payload => payload.extend(field("payload", TEXT_OID, -2)),
        }
    }
    payload
}

/// The perf demo's read path (the measurement instrument, not the
/// engine): parse already paid (ADR 0002); what remains is the point
/// arithmetic — `id → page,slot → pool-resident bytes → decode` — and
/// the wire. Same batching/yield discipline as the debug select.
async fn demo_select<W: AsyncWrite + Unpin>(
    heap: &crate::demoheap::DemoHeap,
    cols: &[Col],
    filter: Option<i32>,
    out: &mut RowBatcher<W>,
) -> Result<(), PgError> {
    out.send(b'T', &row_description(cols))
        .await
        .map_err(io_err)?;
    let (first, last) = match filter {
        Some(id) => (id.max(0) as u32, (id.max(0) as u32).min(heap.rows())),
        None => (1, heap.rows()),
    };
    let mut sent = 0u64;
    if first != 0 {
        for id in first..=last {
            let Some((rid, payload)) = heap.row(id) else { continue };
            let rid = rid.to_string();
            let fields: Vec<&[u8]> = cols
                .iter()
                .map(|c| match c {
                    Col::Id => rid.as_bytes(),
                    Col::Val => unreachable!("demo has no val"),
                    Col::Payload => payload.as_bytes(),
                })
                .collect();
            out.row(&fields);
            sent += 1;
            if out.should_flush() {
                out.flush().await.map_err(io_err)?;
                crate::coop::yield_point().await;
            }
        }
    }
    out.flush().await.map_err(io_err)?;
    send_tag(out, &format!("SELECT {sent}")).await
}

fn debug_worker_description() -> Vec<u8> {
    let mut payload = 1i16.to_be_bytes().to_vec();
    payload.extend(field("debug_worker", TEXT_OID, -2));
    payload
}

fn debug_roles_description() -> Vec<u8> {
    let mut payload = 1i16.to_be_bytes().to_vec();
    payload.extend(field("role", TEXT_OID, -2));
    payload
}

fn debug_databases_description() -> Vec<u8> {
    let mut payload = 3i16.to_be_bytes().to_vec();
    payload.extend(field("database", TEXT_OID, -2));
    payload.extend(field("affinity", TEXT_OID, -2));
    payload.extend(field("numa", TEXT_OID, -2));
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future::block_on;
    use std::{pin::Pin, sync::Mutex, task::Poll};

    /// Records every write as one boundary — the flush counts the tests
    /// assert against.
    struct Sink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl AsyncWrite for Sink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().unwrap().push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn data_row(n: u32) -> [u8; 3] {
        [b'r', n as u8, 0] // payload varies so frames are countable by content
    }

    #[test]
    fn batcher_frames_rows_in_batch_rows_sized_writes() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut b = RowBatcher::new(Sink(Arc::clone(&writes)), 3);
        for i in 0..7 {
            b.row(&[data_row(i).as_slice()]);
            if b.should_flush() {
                block_on(b.flush()).unwrap();
            }
        }
        block_on(b.flush()).unwrap();

        let all = writes.lock().unwrap();
        let sizes: Vec<usize> = all.iter().map(|w| w.len()).collect();
        assert_eq!(
            all.len(),
            3,
            "7 rows / batch 3 → writes at 3, 3, 1 boundaries"
        );
        // Each write holds complete 'D' frames only.
        for (i, w) in all.iter().enumerate() {
            let expect_rows = if i < 2 { 3 } else { 1 };
            let mut rows = 0;
            let mut pos = 0;
            while pos < w.len() {
                assert_eq!(w[pos], b'D', "write {i} must contain only DataRow frames");
                let len = u32::from_be_bytes(w[pos + 1..pos + 5].try_into().unwrap()) as usize;
                pos += 5 + len - 4;
                rows += 1;
            }
            assert_eq!(rows, expect_rows, "write {i} row count");
        }
        // Each frame is exactly one row: 1 + 4 + (2 + 4 + 3) = 14 bytes.
        assert_eq!(sizes.iter().sum::<usize>(), 7 * 14);
    }

    #[test]
    fn batcher_flush_is_noop_when_empty_and_direct_send_skips_buffer() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut b = RowBatcher::new(Sink(Arc::clone(&writes)), 4);
        block_on(b.flush()).unwrap();
        block_on(b.send(b'Z', b"I")).unwrap();
        assert!(b.row_state_is_clean());
        let all = writes.lock().unwrap();
        // Empty flush wrote nothing; the 3 write calls are send()'s type byte,
        // length, payload — the row buffer played no part.
        let combined: Vec<u8> = all.iter().flatten().copied().collect();
        assert_eq!(combined, [b'Z', 0, 0, 0, 5, b'I']);
    }

    impl<W> RowBatcher<W> {
        fn row_state_is_clean(&self) -> bool {
            self.buffered_rows == 0 && self.buf.is_empty()
        }
    }
}
