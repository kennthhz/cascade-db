//! The load generator: spawns (or attaches to) a server — cascade or the
//! pgmodel twin — drives a closed-loop workload from pinned client threads,
//! and reports throughput, latency percentiles, and (for cascade) the
//! runtime observables the server prints on SIGTERM.
//!
//! One command per data point, e.g.:
//!   bench-harness --server-bin ./cascade-server \
//!     --server-args "--port 5544 --hk-cores 0 --worker-cores 2,3 --wal-dir /tmp/wal" \
//!     --workload txn --concurrency 8 --seconds 10 --client-cores 8,9,10,11

use std::{
    io::{BufRead, BufReader},
    process::{Child, ChildStdout, Command, Stdio},
    time::Duration,
};

use bench::{aggregate, parse_cores, run_client, Workload};

struct Args {
    server_bin: Option<String>,
    server_args: Option<String>,
    addr: Option<String>,
    workload: Workload,
    concurrency: usize,
    seconds: u64,
    warmup_seconds: u64,
    client_cores: Option<Vec<u32>>,
}

fn parse_args() -> Args {
    let mut a = Args {
        server_bin: None,
        server_args: None,
        addr: None,
        workload: Workload::Txn,
        concurrency: 4,
        seconds: 10,
        warmup_seconds: 2,
        client_cores: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--server-bin" => a.server_bin = Some(it.next().expect("value")),
            "--server-args" => a.server_args = Some(it.next().expect("value")),
            "--addr" => a.addr = Some(it.next().expect("value")),
            "--workload" => {
                a.workload = Workload::parse(&it.next().expect("value")).expect("workload")
            }
            "--concurrency" => a.concurrency = it.next().expect("value").parse().expect("int"),
            "--seconds" => a.seconds = it.next().expect("value").parse().expect("int"),
            "--warmup-seconds" => {
                a.warmup_seconds = it.next().expect("value").parse().expect("int")
            }
            "--client-cores" => a.client_cores = Some(parse_cores(&it.next().expect("value"))),
            other => panic!("unknown arg {other}"),
        }
    }
    if a.addr.is_none() && (a.server_bin.is_none() || a.server_args.is_none()) {
        panic!("need --addr (attach) or --server-bin + --server-args (spawn)");
    }
    a
}

/// Kills a spawned server on any exit path (including panics). A leaked
/// server holds its piped stdout write-end open, which wedges anything
/// downstream that reads that pipe to EOF — the same failure mode the
/// parity test's `Pgmodel` Drop guard prevents.
struct ServerGuard(Option<Child>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let _ = child.wait();
        }
    }
}

fn main() {
    let args = parse_args();

    // Spawn or attach. A spawned server's stdout is the control channel:
    // the "ready on ADDR" line, then (cascade) the observables JSON at the
    // end. The guard kills the child on any exit path, panics included —
    // drain_after_sigterm takes the child out when it handles termination.
    let (addr, guard, mut lines) = match (&args.server_bin, &args.server_args) {
        (Some(bin), Some(sargs)) => {
            let mut child = Command::new(bin)
                .args(sargs.split_whitespace())
                .stdout(Stdio::piped())
                .spawn()
                .expect("spawn server");
            let stdout: ChildStdout = child.stdout.take().expect("stdout");
            let mut lines = BufReader::new(stdout).lines();
            let addr = wait_ready(&mut lines);
            (addr, Some(ServerGuard(Some(child))), Some(lines))
        }
        _ => (args.addr.clone().expect("addr"), None, None),
    };
    let mut guard = guard;

    eprintln!(
        "benching {} × {} against {} ({}s + {}s warmup)",
        args.workload.name(),
        args.concurrency,
        addr,
        args.seconds,
        args.warmup_seconds
    );

    let mut handles = Vec::new();
    for worker in 0..args.concurrency {
        let addr = addr.clone();
        let core = args.client_cores.as_ref().map(|cs| cs[worker % cs.len()]);
        handles.push(std::thread::spawn(move || {
            run_client(
                &addr,
                args.workload,
                worker,
                Duration::from_secs(args.warmup_seconds),
                Duration::from_secs(args.seconds),
                core,
            )
            .expect("client runs")
        }));
    }
    let started = std::time::Instant::now();
    let stats: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("join"))
        .collect();
    let elapsed = started.elapsed();

    let report = aggregate(args.workload.name(), args.concurrency, elapsed, stats);
    println!("{}", report.json());
    eprintln!(
        "{:>6} ops/s | p50 {:>8.1}µs p90 {:>8.1}µs p99 {:>8.1}µs max {:>9.1}µs",
        report.throughput as u64, report.p50_us, report.p90_us, report.p99_us, report.max_us
    );

    // Collect the server's final observables (cascade prints one JSON line
    // on SIGTERM; the pgmodel twin has none — default termination).
    if let Some(g) = guard.as_mut() {
        if let Some(mut child) = g.0.take() {
            drain_after_sigterm(&mut child, lines.as_mut().expect("server stdout"));
        }
    }
}

fn wait_ready(lines: &mut std::io::Lines<BufReader<ChildStdout>>) -> String {
    loop {
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(at) = line.find(" on ") {
                    // The addr token ends at whitespace (the pgmodel line
                    // appends an affinity/wal parenthetical).
                    let rest = &line[at + 4..];
                    let end = rest.find(' ').unwrap_or(rest.len());
                    return rest[..end].to_owned();
                }
                eprintln!("[server] {line}");
            }
            Some(Err(e)) => panic!("server stdout closed: {e}"),
            None => panic!("server exited before becoming ready"),
        }
    }
}

fn drain_after_sigterm(child: &mut Child, lines: &mut std::io::Lines<BufReader<ChildStdout>>) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    for line in lines.by_ref().flatten() {
        if line.contains("\"event\":\"cascade_observables\"") {
            println!("{line}");
        }
    }
    let _ = child.wait();
}
