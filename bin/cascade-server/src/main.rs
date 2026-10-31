//! Cascade DB server entry point.
//!
//! Minimal M1-era server: explicit core/port arguments (the TOML config
//! surface and catalog wiring land in later phases), boots the routed
//! runtime from `docs/runtime-implementation-plan.md` §10.1, serves until
//! SIGTERM/SIGINT, then prints one JSON line of runtime observables —
//! the stall totals (§7.2) and the WAL writer's watermarks and fdatasync
//! barrier count (FR-26/FR-27) — for the bench harness to collect.

use std::{
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    time::Duration,
};

use cascade_runtime::{
    config::{NumaPool, NumaWorkers, RuntimeConfig},
    pin,
    topology::SyntheticTopology,
    AcceptLoop, Runtime,
};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    // Async-signal-safe: one relaxed atomic store.
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn main() {
    let mut port: u16 = 5432;
    let mut hk_cores: Vec<u32> = Vec::new();
    let mut worker_cores: Vec<u32> = Vec::new();
    let mut wal_dir: Option<std::path::PathBuf> = None;
    let mut batch_rows: u32 = 1024;
    let mut demo_rows: u32 = 0;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().expect("--port value").parse().expect("port"),
            "--hk-cores" => hk_cores = parse_cores(&it.next().expect("--hk-cores value")),
            "--worker-cores" => {
                worker_cores = parse_cores(&it.next().expect("--worker-cores value"))
            }
            "--wal-dir" => {
                wal_dir = Some(std::path::PathBuf::from(
                    it.next().expect("--wal-dir value"),
                ))
            }
            "--batch-rows" => {
                batch_rows = it
                    .next()
                    .expect("--batch-rows value")
                    .parse()
                    .expect("rows")
            }
            "--demo-rows" => {
                demo_rows = it
                    .next()
                    .expect("--demo-rows value")
                    .parse()
                    .expect("demo rows")
            }
            other => {
                eprintln!("unknown arg {other}");
                std::process::exit(2);
            }
        }
    }
    if hk_cores.is_empty() || worker_cores.is_empty() {
        eprintln!("usage: cascade-server --port N --hk-cores a --worker-cores b,c [--wal-dir DIR] [--batch-rows N] [--demo-rows N]");
        std::process::exit(2);
    }

    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    // Bench/dev placement: the operator's cores, taken as the whole truth
    // (the /sys topology provider and TOML config arrive in later phases).
    let mut allowed = pin::current_affinity();
    allowed.retain(|c| hk_cores.contains(c) || worker_cores.contains(c));
    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = RuntimeConfig {
        housekeeper_cores: hk_cores.clone(),
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: worker_cores.clone(),
        }],
        numa_pools: vec![NumaPool {
            numa: 0,
            size_mb: 64,
        }],
        listen: cascade_runtime::config::ListenConfig {
            host: "127.0.0.1".into(),
            port,
        },
        batch_rows,
        wal_dir,
        demo_rows,
        ..RuntimeConfig::default()
    };

    // FR-7: recovery runs *before* the socket binds. A boot that refused
    // (corrupt log) never accepts a connection; a boot that replayed never
    // serves a row its WAL cannot vouch for.
    let db = cascade_runtime::tenant::dev_context().db;
    let plan = match cfg.wal_dir.as_ref() {
        Some(dir) => match cascade_runtime::recovery::prepare(dir, db) {
            Ok(plan) => {
                if plan.unclean {
                    eprintln!(
                        "cascade-server: crash recovery: replaying from byte {} ({} row(s){})",
                        plan.resume_offset,
                        plan.seed_rows.len(),
                        plan.torn_tail
                            .map(|l| format!(", torn tail truncated at {:#x}", l.0))
                            .unwrap_or_default(),
                    );
                }
                Some(plan)
            }
            Err(e) => {
                eprintln!("cascade-server: boot refused: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    let listener = AcceptLoop::bind("127.0.0.1", port).expect("bind");
    let bound = listener.local_addr().expect("addr").port();
    let rt = Runtime::boot_routed(
        &cfg,
        &topo,
        Some(listener),
        Default::default(),
        plan.as_ref(),
    )
    .expect("boot routed");
    println!("cascade-server: ready on 127.0.0.1:{bound}");
    use std::io::Write;
    std::io::stdout().flush().ok();

    while !SHUTDOWN.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Graceful shutdown (T3.3): drain → final flush → clean marker. The
    // runtime drops first so no worker can append past the final sync; the
    // brief drain sleep lets in-flight flushes land (the full drain is
    // M4's — this slice's shutdown window has no concurrent clients to
    // drain in the harness).
    let (soft, hard) = rt.poll_stalls();
    let wal = rt.debug_wal().cloned();
    drop(rt);
    std::thread::sleep(Duration::from_millis(200));
    if let (Some(writer), Some(dir)) = (&wal, cfg.wal_dir.as_ref()) {
        let writer = Arc::clone(writer);
        let dir = dir.clone();
        let outcome = std::thread::Builder::new()
            .name("cascade-shutdown".into())
            .spawn(move || -> Result<(), String> {
                let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("cascade-shutdown-io")
                    .make()
                    .map_err(|e| format!("executor: {e}"))?;
                ex.run(async {
                    let db = cascade_runtime::tenant::dev_context().db;
                    let handle = cascade_wal::WalHandle::open(&dir, db)
                        .await
                        .map_err(|e| format!("wal handle: {e}"))?;
                    let filled = writer.filled();
                    writer
                        .flush_wal(&handle, filled)
                        .await
                        .map_err(|e| format!("final flush: {e}"))?;
                    cascade_wal::write_clean_marker(&dir, db, writer.durable())
                        .map_err(|e| format!("clean marker: {e}"))?;
                    Ok(())
                })
            })
            .expect("shutdown thread spawns")
            .join()
            .expect("shutdown thread joins");
        if let Err(e) = outcome {
            eprintln!("cascade-server: clean marker not written: {e}");
            std::process::exit(1);
        }
    }

    // One JSON observables line: the numbers the architecture claims live
    // or die on (stalls ≈ 0 under healthy load; bytes-per-fsync shows the
    // group-commit batching). Read off the writer Arc kept from before the
    // runtime dropped — the numbers are final here.
    let (syncs, durable, filled) = match wal.as_ref() {
        Some(w) => (w.syncs(), w.durable().0, w.filled().0),
        None => (0, 0, 0),
    };
    println!(
        "{{\"event\":\"cascade_observables\",\"stall_soft_total\":{soft},\"stall_hard_total\":{hard},\"wal_syncs\":{syncs},\"wal_durable_lsn\":{durable},\"wal_filled_lsn\":{filled}}}"
    );
    std::io::stdout().flush().ok();
}

fn parse_cores(s: &str) -> Vec<u32> {
    s.split(',')
        .map(|c| c.trim().parse().expect("core number"))
        .collect()
}
