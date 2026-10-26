//! Shared bench machinery: a raw simple-protocol client (identical for both
//! servers, so the client is never the variable), closed-loop workload
//! runners, and latency aggregation.
//!
//! Fairness contract (see README.md): both servers speak this exact
//! protocol, run the same debug engine, and write the same WAL bytes with
//! fdatasync-before-ack. Only the concurrency architecture differs.

use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// A parsed simple-protocol server message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// CommandComplete tag (e.g. "COMMIT", "SELECT 3").
    Tag(String),
    /// One DataRow, text-format fields.
    Row(Vec<String>),
    /// ErrorResponse (SQLSTATE, message).
    Error(String, String),
    /// EmptyQueryResponse / anything else we do not model.
    Other(u8),
}

pub struct Client {
    sock: TcpStream,
}

impl Client {
    /// Connect and run the trust-mode startup handshake (the same bytes
    /// every cascade test uses).
    pub fn connect(addr: &str) -> std::io::Result<Client> {
        let sock = TcpStream::connect(addr)?;
        sock.set_nodelay(true).expect("nodelay");
        let mut body = Vec::new();
        body.extend_from_slice(&196_608i32.to_be_bytes());
        for field in ["user", "bench", "database", "dev", ""] {
            body.extend_from_slice(field.as_bytes());
            body.push(0);
        }
        let mut c = Client { sock };
        c.write_raw(&(body.len() as i32 + 4).to_be_bytes())?;
        c.write_raw(&body)?;
        // Read until ReadyForQuery.
        loop {
            let (t, _) = c.read_msg()?;
            if t == b'Z' {
                break;
            }
        }
        Ok(c)
    }

    fn write_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.sock.write_all(bytes)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        // CASCADE_CLIENT_POLL=1: busy-poll instead of blocking — if the
        // bytes are already in the buffer, the RTT collapses and the
        // blocking read's *sleep wake* is what's broken (WSL2 io_uring
        // writer → blocking reader).
        if std::env::var("CASCADE_CLIENT_POLL").as_deref() == Ok("1") {
            use std::os::fd::AsRawFd;
            use std::time::Duration;
            self.sock.set_nonblocking(true).ok();
            let fd = self.sock.as_raw_fd();
            let total = buf.len();
            let mut done = 0;
            loop {
                if done == total {
                    self.sock.set_nonblocking(false).ok();
                    return Ok(());
                }
                let n = unsafe {
                    libc::recv(
                        fd,
                        buf[done..].as_mut_ptr().cast(),
                        total - done,
                        libc::MSG_DONTWAIT,
                    )
                };
                if n > 0 {
                    done += n as usize;
                } else {
                    std::thread::sleep(Duration::from_micros(50));
                }
            }
        }
        self.sock.read_exact(buf)
    }

    /// One framed message: type byte + payload.
    pub fn read_msg(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut hdr = [0u8; 5];
        self.read_exact(&mut hdr)?;
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
        let mut payload = vec![0u8; len];
        self.read_exact(&mut payload)?;
        Ok((hdr[0], payload))
    }

    /// Send one query and collect every message up to (and including) the
    /// ReadyForQuery. The round trip is the latency unit.
    pub fn query(&mut self, sql: &str) -> std::io::Result<Vec<Msg>> {
        let body = sql.as_bytes();
        let mut frame = Vec::with_capacity(body.len() + 6);
        frame.push(b'Q');
        frame.extend_from_slice(&((body.len() + 5) as i32).to_be_bytes());
        frame.extend_from_slice(body);
        frame.push(0);
        let trace = std::env::var("CASCADE_TRACE_CLIENT").as_deref() == Ok("1");
        let t0 = Instant::now();
        self.write_raw(&frame)?;
        if trace {
            let epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            eprintln!(
                "[cli-t] send-done after {:?} at {}us",
                t0.elapsed(),
                epoch.as_micros()
            );
        }

        let mut msgs = Vec::new();
        loop {
            let (t, payload) = self.read_msg()?;
            match t {
                b'C' => {
                    let tag = payload[..payload.len() - 1].to_vec();
                    msgs.push(Msg::Tag(String::from_utf8_lossy(&tag).into_owned()));
                }
                b'D' => {
                    let n = i16::from_be_bytes([payload[0], payload[1]]) as usize;
                    let mut fields = Vec::with_capacity(n);
                    let mut at = 2;
                    for _ in 0..n {
                        let flen =
                            u32::from_be_bytes(payload[at..at + 4].try_into().unwrap()) as usize;
                        at += 4;
                        if flen == u32::MAX as usize {
                            fields.push(String::new()); // NULL
                            continue;
                        }
                        fields.push(String::from_utf8_lossy(&payload[at..at + flen]).into_owned());
                        at += flen;
                    }
                    msgs.push(Msg::Row(fields));
                }
                b'E' => {
                    let (code, message) = parse_error(&payload);
                    msgs.push(Msg::Error(code, message));
                }
                b'Z' => {
                    msgs.push(Msg::Other(b'Z'));
                    if trace {
                        eprintln!("[cli-t] rfb seen after {:?}", t0.elapsed());
                    }
                    return Ok(msgs);
                }
                other => msgs.push(Msg::Other(other)),
            }
        }
    }

    /// True when every message of the result says the op landed.
    pub fn assert_ok(&self, msgs: &[Msg], what: &str) {
        for m in msgs {
            if let Msg::Error(code, message) = m {
                panic!("{what} failed: {code} {message}");
            }
        }
    }

    /// The CommandComplete tag of the last statement in the batch.
    pub fn last_tag(msgs: &[Msg]) -> &str {
        msgs.iter()
            .filter_map(|m| match m {
                Msg::Tag(t) => Some(t.as_str()),
                _ => None,
            })
            .next_back()
            .unwrap_or("")
    }
}

fn parse_error(payload: &[u8]) -> (String, String) {
    let mut code = String::new();
    let mut message = String::new();
    let mut at = 0;
    while at < payload.len() && payload[at] != 0 {
        let field = payload[at];
        at += 1;
        let end = payload[at..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| at + p)
            .unwrap_or(payload.len());
        let value = String::from_utf8_lossy(&payload[at..end]).into_owned();
        match field {
            b'C' => code = value,
            b'M' => message = value,
            _ => {}
        }
        at = end + 1;
    }
    (code, message)
}

/// The three bench workloads (closed loop: one op in flight per client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// Point reads off a tiny preloaded table (engine ≈ 0, pure platform).
    Read,
    /// Autocommit INSERT: one fsync'd transaction per op.
    Insert,
    /// BEGIN; INSERT; COMMIT in one round trip: the group-commit target.
    Txn,
}

impl Workload {
    pub fn parse(s: &str) -> Option<Workload> {
        match s {
            "read" => Some(Workload::Read),
            "insert" => Some(Workload::Insert),
            "txn" => Some(Workload::Txn),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Workload::Read => "read",
            Workload::Insert => "insert",
            Workload::Txn => "txn",
        }
    }

    /// The statement batch for one op (worker = 0..concurrency, i = op no).
    fn statements(&self, worker: usize, i: u64) -> String {
        match self {
            Workload::Read => "SELECT * FROM debug_table".to_owned(),
            Workload::Insert => {
                format!(
                    "INSERT INTO debug_table VALUES ({}, 'v{i}/{worker}')",
                    i % 10_000
                )
            }
            Workload::Txn => format!(
                "BEGIN; INSERT INTO debug_table VALUES ({}, 't{i}/{worker}'); COMMIT",
                i % 10_000
            ),
        }
    }

    /// Statements that must succeed during warmup's first contact (setup).
    fn setup(&self) -> Option<&'static str> {
        match self {
            Workload::Read => Some("INSERT INTO debug_table VALUES (1, 'r1')"),
            _ => None,
        }
    }
}

/// Monotonic per-worker op counter so ids never repeat.
static OP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Result of one measured client thread.
pub struct ThreadStats {
    pub latencies_ns: Vec<u64>,
}

/// Run one closed-loop client thread: `warmup` unmeasured ops, then
/// `duration` of measured ops. Pins to `core` when given.
pub fn run_client(
    addr: &str,
    workload: Workload,
    worker: usize,
    warmup: Duration,
    duration: Duration,
    core: Option<u32>,
) -> std::io::Result<ThreadStats> {
    if let Some(core) = core {
        pin_to(core);
    }
    let mut c = Client::connect(addr)?;
    if let Some(setup) = workload.setup() {
        let msgs = c.query(setup)?;
        c.assert_ok(&msgs, "setup");
    }

    let warm_until = Instant::now() + warmup;
    while Instant::now() < warm_until {
        let i = OP_SEQ.fetch_add(1, Ordering::Relaxed);
        let msgs = c.query(&workload.statements(worker, i))?;
        c.assert_ok(&msgs, "warmup op");
    }

    let mut latencies_ns = Vec::new();
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let i = OP_SEQ.fetch_add(1, Ordering::Relaxed);
        let t0 = Instant::now();
        let msgs = c.query(&workload.statements(worker, i))?;
        let t1 = Instant::now();
        c.assert_ok(&msgs, "measured op");
        latencies_ns.push((t1 - t0).as_nanos() as u64);
    }
    Ok(ThreadStats { latencies_ns })
}

/// Aggregate report across all client threads.
pub struct Report {
    pub workload: &'static str,
    pub concurrency: usize,
    pub duration: Duration,
    pub ops: u64,
    pub throughput: f64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
}

pub fn aggregate(
    workload: &'static str,
    concurrency: usize,
    duration: Duration,
    mut stats: Vec<ThreadStats>,
) -> Report {
    let mut all: Vec<u64> = stats
        .iter_mut()
        .flat_map(|s| s.latencies_ns.drain(..))
        .collect();
    let ops = all.len() as u64;
    all.sort_unstable();
    let pick = |p: usize| -> f64 {
        if all.is_empty() {
            return 0.0;
        }
        all[(all.len() * p / 100).min(all.len() - 1)] as f64 / 1000.0
    };
    Report {
        workload,
        concurrency,
        duration,
        ops,
        throughput: ops as f64 / duration.as_secs_f64(),
        p50_us: pick(50),
        p90_us: pick(90),
        p99_us: pick(99),
        max_us: all.last().map(|&v| v as f64 / 1000.0).unwrap_or(0.0),
    }
}

impl Report {
    pub fn json(&self) -> String {
        format!(
            r#"{{"workload":"{}","concurrency":{},"seconds":{:.2},"ops":{},"throughput":{:.1},"p50_us":{:.1},"p90_us":{:.1},"p99_us":{:.1},"max_us":{:.1}}}"#,
            self.workload,
            self.concurrency,
            self.duration.as_secs_f64(),
            self.ops,
            self.throughput,
            self.p50_us,
            self.p90_us,
            self.p99_us,
            self.max_us
        )
    }
}

/// Pin the calling thread to one core (clients and the pgmodel postmaster
/// share this so both sides of the comparison place their threads the same
/// way).
pub fn pin_to(core: u32) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core as usize, &mut set);
        assert_eq!(
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set),
            0,
            "pin to core {core}"
        );
    }
}

/// Pin the calling thread to a comma-separated core list.
pub fn pin_to_list(cores: &[u32]) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for &core in cores {
            libc::CPU_SET(core as usize, &mut set);
        }
        assert_eq!(
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set),
            0,
            "pin to cores {cores:?}"
        );
    }
}

/// Parse "0,2,3" into cores.
pub fn parse_cores(s: &str) -> Vec<u32> {
    s.split(',')
        .map(|c| c.trim().parse().expect("core number"))
        .collect()
}
