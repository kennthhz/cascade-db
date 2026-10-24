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
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use cascade_telemetry::Span;
use cascade_tenant::TenantContext;
use futures_lite::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use static_assertions::assert_not_impl_any;

use crate::{
    handoff::Handoff,
    sql::{Col, SqlError, Stmt},
    tenant::{dev_context, TenantCounters},
};

// FR-14, compile-enforced: if this ever starts to fail, connection state
// grew a cross-thread path — the design, not the assertion, is wrong.
assert_not_impl_any!(Session: Send, Sync);

/// Column type OIDs (text format on the wire).
const INT4_OID: i32 = 23;
const TEXT_OID: i32 = 25;

type Row = (i32, String);

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

/// The session task a worker spawns per handed-off connection: simple-query
/// loop until Terminate or disconnect. The tenant context and span are
/// constructed once here and *borrowed* through every executed statement
/// (FR-20/FR-21 shape, from birth).
pub async fn run(
    handoff: Handoff,
    table: Rc<Table>,
    counter: Arc<AtomicU64>,
    tenants: Arc<TenantCounters>,
    batch_rows: u32,
) {
    counter.fetch_add(1, Ordering::Relaxed);
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
        let msg = match read_message(&mut out.sink).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                // Framing violation (bad length): answer, then close — the
                // stream can no longer be trusted.
                let err = PgError::new("08P01", format!("protocol violation: {e}"));
                let _ = out.send_error(&err).await;
                break;
            }
            Err(_) => break,
        };
        match msg.0 {
            b'Q' => {
                // Payload is one cstring — except the empty query, which has
                // no payload at all and answers EmptyQueryResponse.
                let sql = if msg.1.is_empty() {
                    ""
                } else {
                    let Some(nul) = msg.1.iter().position(|&b| b == 0) else {
                        let e = PgError::new("08P01", "unterminated query string");
                        let _ = out.send_error(&e).await;
                        break;
                    };
                    match std::str::from_utf8(&msg.1[..nul]) {
                        Ok(s) => s,
                        Err(_) => {
                            let e = PgError::new("08P01", "query string is not UTF-8");
                            let _ = out.send_error(&e).await;
                            break;
                        }
                    }
                };
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
                            match execute(&mut sess, stmt, &mut out, &ctx, &span, &tenants).await {
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
                    break;
                }
            }
            // Terminate: clean client goodbye.
            b'X' => break,
            // Extended protocol or anything else desyncs our framing —
            // respond and close rather than misread the stream (slice runs
            // the simple protocol only, FR-16).
            t => {
                let e = PgError::new(
                    "08P01",
                    format!("protocol violation: unsupported message type {:?}", t),
                );
                let _ = out.send_error(&e).await;
                break;
            }
        }
    }
    drop(out); // closes the fd
    counter.fetch_sub(1, Ordering::Relaxed);
}

/// Execute one parsed statement, writing its full result set. Errors carry
/// the SQLSTATE the client sees.
async fn execute<W: AsyncWrite + Unpin>(
    sess: &mut Session,
    stmt: Stmt,
    out: &mut RowBatcher<W>,
    ctx: &TenantContext,
    span: &Span,
    tenants: &TenantCounters,
) -> Result<(), PgError> {
    // The hot-path shape (FR-20/FR-21): context and span borrowed, per
    // statement, nothing global.
    crate::tenant::debug_hot_path(ctx, span, tenants);

    // Aborted transaction: every statement but ROLLBACK is refused (PG 25P02);
    // COMMIT acts as ROLLBACK.
    if sess.txn == TxnStatus::Failed && !matches!(stmt, Stmt::Rollback) {
        if stmt == Stmt::Commit {
            sess.pending.clear();
            sess.txn = TxnStatus::Idle;
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
                    for row in sess.pending.drain(..) {
                        sess.table.insert(row);
                    }
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
            send_tag(out, "ROLLBACK").await
        }
        Stmt::Insert { id, val } => {
            let row = (id, val);
            if sess.txn == TxnStatus::InTxn {
                sess.pending.push(row);
            } else {
                sess.table.insert(row);
            }
            send_tag(out, "INSERT 0 1").await
        }
        Stmt::Select { cols } => {
            out.send(b'T', &row_description(&cols))
                .await
                .map_err(io_err)?;
            // Own uncommitted inserts are visible inside the transaction;
            // committed rows come from the table. (Insertion order across
            // the two sources is not interleaved — accepted slice cut.)
            let mut rows = sess.pending.clone();
            rows.extend(sess.table.snapshot());
            for (id, val) in &rows {
                let id = id.to_string();
                let fields: Vec<&[u8]> = cols
                    .iter()
                    .map(|c| match c {
                        Col::Id => id.as_bytes(),
                        Col::Val => val.as_bytes(),
                    })
                    .collect();
                out.row(&fields);
                if out.should_flush() {
                    out.flush().await.map_err(io_err)?;
                }
            }
            out.flush().await.map_err(io_err)?;
            send_tag(out, &format!("SELECT {}", rows.len())).await
        }
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
    }
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
        }
    }
    payload
}

fn debug_worker_description() -> Vec<u8> {
    let mut payload = 1i16.to_be_bytes().to_vec();
    payload.extend(field("debug_worker", TEXT_OID, -2));
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
