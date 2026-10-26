//! The PostgreSQL *runtime architecture*, stripped to its skeleton: a
//! single-threaded postmaster that blocks in accept(2) and forks a backend
//! process per connection; each backend does blocking socket I/O, owns its
//! memory, and shares nothing but the WAL file.
//!
//! This is the comparison twin of the cascade server (see README.md):
//! identical wire protocol, identical debug engine (same parser crate,
//! same `Vec` table semantics), identical WAL record bytes with
//! fdatasync-before-ack. What differs is only the concurrency model:
//!
//! |                | cascade              | pgmodel (this)          |
//! |----------------|----------------------|-------------------------|
//! | dispatch       | postmaster → handoff | postmaster → fork       |
//! | execution unit | pinned reactor task  | OS process              |
//! | socket I/O     | io_uring, async      | blocking read/write     |
//! | WAL commit     | shm watermarks + one | O_APPEND + fdatasync    |
//! |                | CAS-elected leader   | per backend, per commit |
//!
//! The last row is the honest asymmetry: cascade's group commit is cheap
//! *because* it is one process with shared memory. Reproducing it here
//! faithfully would require shm locks (real PG's lwlocks) — out of scope
//! for a runtime baseline; this is PG with commit_delay=0.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::io::AsRawFd,
};

use bench::{parse_cores, pin_to_list};
use cascade_runtime::sql::{self, Col, Stmt};
use cascade_wal::format;

struct Args {
    port: u16,
    cores: Option<Vec<u32>>,
    wal_dir: Option<std::path::PathBuf>,
}

fn parse_args() -> Args {
    let mut args = Args {
        port: 5433,
        cores: None,
        wal_dir: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => args.port = it.next().expect("--port value").parse().expect("port"),
            "--cores" => args.cores = Some(parse_cores(&it.next().expect("--cores value"))),
            "--wal-dir" => {
                args.wal_dir = Some(std::path::PathBuf::from(
                    it.next().expect("--wal-dir value"),
                ))
            }
            other => panic!("unknown arg {other} (want --port/--cores/--wal-dir)"),
        }
    }
    args
}

/// Per-backend engine state — the same shape as cascade's `Session` (the
/// fairness contract: same engine, different runtime).
#[derive(Default)]
struct Engine {
    table: Vec<(i32, String)>,
    pending: Vec<(i32, String)>,
    txn_open: bool,
    txn_failed: bool,
}

fn main() {
    let args = parse_args();
    if let Some(cores) = &args.cores {
        pin_to_list(cores); // backends inherit this affinity across fork
    }

    // Auto-reap backends: no zombies, no SIGCHLD handler needed.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        sa.sa_flags = libc::SA_NOCLDWAIT;
        libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut());
    }

    let listener = TcpListener::bind(("127.0.0.1", args.port)).expect("bind");
    println!(
        "pgmodel: ready on 127.0.0.1:{} (cores {:?}, wal {:?})",
        args.port, args.cores, args.wal_dir
    );
    std::io::stdout().flush().ok();

    let wal = args.wal_dir.map(|dir| {
        std::fs::create_dir_all(&dir).expect("wal dir");
        let path = dir.join("db_0.log");
        std::fs::OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(path)
            .expect("wal file")
    });

    // The postmaster: one thread, blocking accept — fork per connection.
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let wal = wal.as_ref().map(|f| f.as_raw_fd());
                let pid = unsafe { libc::fork() };
                match pid {
                    -1 => eprintln!("pgmodel: fork failed — dropping connection"),
                    0 => {
                        // Backend: own the socket, never return.
                        drop(listener);
                        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            backend(stream, wal)
                        }))
                        .map(|_| 0)
                        .unwrap_or(1);
                        unsafe { libc::_exit(code) };
                    }
                    _ => {
                        // Postmaster: keep accepting. (The fd is the
                        // child's problem now; closing ours is fine.)
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => eprintln!("pgmodel: accept: {e}"),
        }
    }
}

/// One backend: blocking wire session until EOF/Terminate.
fn backend(mut stream: TcpStream, wal_fd: Option<i32>) {
    stream.set_nodelay(true).ok();
    startup(&mut stream);

    let mut eng = Engine::default();
    loop {
        let Some((t, payload)) = read_msg(&mut stream) else {
            return;
        };
        match t {
            b'Q' => {
                let sql = cstring(&payload);
                let stmts = sql::split_statements(&sql);
                let mut failed = false;
                if stmts.is_empty() {
                    write_msg(&mut stream, b'I', &[]).expect("write");
                }
                for s in stmts {
                    if failed {
                        break;
                    }
                    match execute(&mut eng, s.trim(), wal_fd) {
                        Ok(reply) => write_reply(&mut stream, reply),
                        Err((code, message)) => {
                            write_error(&mut stream, &code, &message).expect("write");
                            if eng.txn_open {
                                eng.txn_failed = true;
                            }
                            failed = true;
                        }
                    }
                }
                let rfb = if eng.txn_failed {
                    b'E'
                } else if eng.txn_open {
                    b'T'
                } else {
                    b'I'
                };
                write_msg(&mut stream, b'Z', &[rfb]).expect("write");
            }
            b'X' => return,
            _ => {
                write_error(&mut stream, "08P01", "protocol violation").ok();
                return;
            }
        }
    }
}

/// Trust-mode startup: the exact message sequence cascade's pg.rs sends.
fn startup(stream: &mut TcpStream) {
    // Read + ignore the startup packet (protocol + user/database fields).
    let _ = read_startup(stream);
    write_msg(stream, b'R', &0i32.to_be_bytes()).expect("auth ok");
    for (name, value) in [
        ("server_version", "16.0"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
    ] {
        let mut payload = Vec::new();
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload.extend_from_slice(value.as_bytes());
        payload.push(0);
        write_msg(stream, b'S', &payload).expect("param");
    }
    let pid = std::process::id() as i32;
    let mut key = Vec::with_capacity(8);
    key.extend_from_slice(&pid.to_be_bytes());
    key.extend_from_slice(&0i32.to_be_bytes());
    write_msg(stream, b'K', &key).expect("key");
    write_msg(stream, b'Z', b"I").expect("rfb");
}

fn read_startup(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    let mut rest = vec![0u8; len - 4];
    stream.read_exact(&mut rest)
}

fn read_msg(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    match stream.read_exact(&mut hdr) {
        Ok(_) => {}
        Err(_) => return None,
    }
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).ok()?;
    Some((hdr[0], payload))
}

fn write_msg(stream: &mut TcpStream, t: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(t);
    frame.extend_from_slice(&((payload.len() as i32 + 4).to_be_bytes()));
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

fn cstring(payload: &[u8]) -> String {
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).into_owned()
}

fn write_error(stream: &mut TcpStream, code: &str, message: &str) -> std::io::Result<()> {
    let mut payload = Vec::new();
    for (tag, value) in [
        (b'S', "ERROR"),
        (b'V', "ERROR"),
        (b'C', code),
        (b'M', message),
    ] {
        payload.push(tag);
        payload.extend_from_slice(value.as_bytes());
        payload.push(0);
    }
    payload.push(0);
    write_msg(stream, b'E', &payload)
}

/// RowDescription field block — byte-identical to cascade's `field()`.
fn field(name: &str, oid: i32, typlen: i16) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(name.as_bytes());
    f.push(0);
    f.extend_from_slice(&0i32.to_be_bytes());
    f.extend_from_slice(&0i16.to_be_bytes());
    f.extend_from_slice(&oid.to_be_bytes());
    f.extend_from_slice(&typlen.to_be_bytes());
    f.extend_from_slice(&(-1i32).to_be_bytes());
    f.extend_from_slice(&0i16.to_be_bytes());
    f
}

fn row_description(cols: &[Col]) -> Vec<u8> {
    let mut payload = (cols.len() as i16).to_be_bytes().to_vec();
    for c in cols {
        match c {
            Col::Id => payload.extend(field("id", 23, 4)),
            Col::Val => payload.extend(field("val", 25, -2)),
            Col::Payload => payload.extend(field("payload", 25, -2)),
        }
    }
    payload
}

fn data_row(fields: &[Vec<u8>]) -> Vec<u8> {
    let body: usize = 2 + fields.iter().map(|f| 4 + f.len()).sum::<usize>();
    let mut buf = vec![b'D'];
    buf.extend_from_slice(&((body + 4) as i32).to_be_bytes());
    buf.extend_from_slice(&(fields.len() as i16).to_be_bytes());
    for f in fields {
        buf.extend_from_slice(&(f.len() as i32).to_be_bytes());
        buf.extend_from_slice(f);
    }
    buf
}

/// fdatasync-before-ack: the identical commit discipline to cascade's
/// flush_wal, from a process context (one O_APPEND write per txn).
fn wal_commit(wal_fd: i32, rows: &[(i32, String)]) -> Result<(), (String, String)> {
    use std::os::unix::io::FromRawFd;
    static XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    // Per-process xid base keeps records unique across backends without
    // shm coordination (the stripped-PG baseline; real PG uses shm counters).
    let base = std::process::id() as u64 % 1000 * 1_000_000_000;
    let xid = base + XID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&format::encode(&cascade_wal::WalRecord::BeginTxn { xid }));
    for (id, val) in rows {
        bytes.extend_from_slice(&format::encode(&cascade_wal::WalRecord::Insert {
            xid,
            table: cascade_types::SegmentId(1),
            id: *id,
            val: val.clone(),
        }));
    }
    bytes.extend_from_slice(&format::encode(&cascade_wal::WalRecord::CommitTxn { xid }));

    let mut file = unsafe { std::fs::File::from_raw_fd(wal_fd) };
    let r = file.write_all(&bytes).and_then(|_| file.sync_data());
    std::mem::forget(file); // fd belongs to the backend, not this File
    r.map_err(|e| ("XX000".to_owned(), format!("wal write failed: {e}")))
}

/// The wire bytes one statement produces, decided synchronously by the
/// engine and written by the caller (cascade's session.rs shape).
enum Reply {
    Empty,
    Tag(&'static str),
    Select {
        desc: Vec<u8>,
        rows: Vec<Vec<u8>>,
        tag: String,
    },
}

fn write_reply(stream: &mut TcpStream, reply: Reply) {
    match reply {
        Reply::Empty => {
            write_msg(stream, b'I', &[]).ok();
        }
        Reply::Tag(t) => send_tag(stream, t),
        Reply::Select { desc, rows, tag } => {
            write_msg(stream, b'T', &desc).ok();
            for r in &rows {
                stream.write_all(r).ok();
            }
            send_tag(stream, &tag);
        }
    }
}

/// Execute one statement: mutate the engine, decide the reply.
fn execute(
    eng: &mut Engine,
    stmt_sql: &str,
    wal_fd: Option<i32>,
) -> Result<Reply, (String, String)> {
    if stmt_sql.is_empty() {
        return Ok(Reply::Empty);
    }
    let stmt = sql::parse(stmt_sql).map_err(|e| (e.code().to_owned(), e.to_string()))?;

    if eng.txn_failed && !matches!(stmt, Stmt::Rollback) {
        if stmt == Stmt::Commit {
            eng.pending.clear();
            eng.txn_open = false;
            eng.txn_failed = false;
            return Ok(Reply::Tag("ROLLBACK"));
        }
        return Err((
            "25P02".into(),
            "current transaction is aborted, commands ignored until end of transaction block"
                .into(),
        ));
    }

    match stmt {
        Stmt::Empty => Ok(Reply::Empty),
        Stmt::Begin => {
            if eng.txn_open {
                return Err((
                    "25001".into(),
                    "there is already a transaction in progress".into(),
                ));
            }
            eng.txn_open = true;
            Ok(Reply::Tag("BEGIN"))
        }
        Stmt::Commit => {
            if !eng.txn_open {
                return Err(("25P01".into(), "there is no transaction in progress".into()));
            }
            if let Some(fd) = wal_fd {
                let rows = std::mem::take(&mut eng.pending);
                wal_commit(fd, &rows)?;
                eng.table.extend(rows);
            } else {
                let rows = std::mem::take(&mut eng.pending);
                eng.table.extend(rows);
            }
            eng.txn_open = false;
            Ok(Reply::Tag("COMMIT"))
        }
        Stmt::Rollback => {
            if !eng.txn_open {
                return Err(("25P01".into(), "there is no transaction in progress".into()));
            }
            eng.pending.clear();
            eng.txn_open = false;
            eng.txn_failed = false;
            Ok(Reply::Tag("ROLLBACK"))
        }
        Stmt::Insert { id, val } => {
            match (eng.txn_open, wal_fd) {
                // Autocommit: its own durable transaction, like cascade.
                (false, Some(fd)) => {
                    wal_commit(fd, &[(id, val.clone())])?;
                    eng.table.push((id, val));
                }
                (false, None) => eng.table.push((id, val)),
                (true, _) => eng.pending.push((id, val)),
            }
            Ok(Reply::Tag("INSERT 0 1"))
        }
        Stmt::Select { table: _, cols, filter } => {
            let mut rows: Vec<(i32, String)> = eng
                .pending
                .iter()
                .chain(eng.table.iter())
                .cloned()
                .collect();
            // Same point-filter semantics as cascade's session — the
            // pgmodel must answer queries identically for the comparison.
            if let Some(id) = filter {
                rows.retain(|(rid, _)| *rid == id);
            }
            let desc = row_description(&cols);
            let rendered: Vec<Vec<u8>> = rows
                .iter()
                .map(|(id, val)| {
                    let fields: Vec<Vec<u8>> = cols
                        .iter()
                        .map(|c| match c {
                            Col::Id => id.to_string().into_bytes(),
                            Col::Val => val.as_bytes().to_vec(),
                            Col::Payload => unreachable!("pgmodel's table has no payload"),
                        })
                        .collect();
                    data_row(&fields)
                })
                .collect();
            let tag = format!("SELECT {}", rows.len());
            Ok(Reply::Select {
                desc,
                rows: rendered,
                tag,
            })
        }
        Stmt::DebugWorker => {
            // The architectural tell: cascade answers with the reactor
            // thread's name; the PG model answers with the backend's pid.
            let name = format!("backend-{}", std::process::id());
            let mut desc = (1i16).to_be_bytes().to_vec();
            desc.extend(field("debug_worker", 25, -2));
            let row = data_row(&[name.into_bytes()]);
            Ok(Reply::Select {
                desc,
                rows: vec![row],
                tag: "SELECT 1".to_owned(),
            })
        }
    }
}

fn send_tag(stream: &mut TcpStream, tag: &str) {
    let mut payload = tag.as_bytes().to_vec();
    payload.push(0);
    write_msg(stream, b'C', &payload).ok();
}
