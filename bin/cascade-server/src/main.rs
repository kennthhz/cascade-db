//! Cascade DB server entry point.
//!
//! Minimal M1-era server: explicit core/port arguments (the TOML config
//! surface and catalog wiring land in later phases), boots the routed
//! runtime from `docs/runtime-implementation-plan.md` §10.1, serves until
//! SIGTERM/SIGINT, runs the §10.2 graceful shutdown (T4.4 — drain, final
//! flushes, clean marker, reactor drops), then prints one JSON line of
//! runtime observables — the stall totals (§7.2) and the WAL writer's
//! watermarks and fdatasync barrier count (FR-26/FR-27) — for the bench
//! harness to collect.

use std::{
    sync::atomic::{AtomicBool, Ordering},
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
    let mut idle_reap_ms: u64 = 0;

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
            "--idle-reap-ms" => {
                idle_reap_ms = it
                    .next()
                    .expect("--idle-reap-ms value")
                    .parse()
                    .expect("idle reap ms")
            }
            other => {
                eprintln!("unknown arg {other}");
                std::process::exit(2);
            }
        }
    }
    if hk_cores.is_empty() || worker_cores.is_empty() {
        eprintln!("usage: cascade-server --port N --hk-cores a --worker-cores b,c [--wal-dir DIR] [--batch-rows N] [--demo-rows N] [--idle-reap-ms N]");
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
        idle_reap_ms,
        ..RuntimeConfig::default()
    };

    // FR-7: recovery runs *before* the socket binds. A boot that refused
    // (corrupt log) never accepts a connection; a boot that replayed never
    // serves a row its WAL cannot vouch for. M6 (storage §9): recovery
    // reads the LOCAL LOG STORE through LogSource — this pre-bind pass
    // refuses a corrupt store before bind; boot re-opens the store and
    // replays again on its own terms.
    let db = cascade_runtime::tenant::dev_context().db;
    let scope = cascade_types::ScopeId(u64::from(db.0));
    if cfg.wal_dir.is_some() {
        let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("cascade-store-boot")
            .make()
            .expect("bootstrap executor");
        let refused: Result<(), String> = ex.run(async {
            let device = std::sync::Arc::new(cascade_blockdev::GlommioBlockDevice::new(
                cfg.wal_dir.as_ref().expect("checked").join("store"),
            ));
            let store = match cascade_logstore::LogStore::open(device).await {
                Ok(s) => s,
                Err(e) => return Err(e.to_string()),
            };
            let span = cascade_telemetry::Span::noop();
            let ctx = cascade_storage_api::OpCtx::new(
                cascade_storage_api::TenantContext::for_database(db),
                &span,
            );
            let _ = cascade_storage_api::LogScopes::create_scope(
                &*store,
                &ctx,
                scope,
                cascade_types::Epoch::FIRST,
                cascade_storage_api::KeyRef { key_id: 1 },
            )
            .await;
            use cascade_storage_api::LogSink as _;
            let durable = store.durable_lsn(scope).expect("durable");
            cascade_runtime::recovery::prepare_from_source(&*store, scope, durable)
                .await
                .map(|plan| {
                    eprintln!(
                        "cascade-server: recovered: replaying from byte {} ({} row(s))",
                        plan.resume_offset,
                        plan.seed_rows.len(),
                    );
                })
                .map_err(|e| e.to_string())
        });
        if let Err(e) = refused {
            eprintln!("cascade-server: boot refused: {e}");
            std::process::exit(1);
        }
    }

    let listener = AcceptLoop::bind("127.0.0.1", port).expect("bind");
    let bound = listener.local_addr().expect("addr").port();
    let rt = Runtime::boot_routed(
        &cfg,
        &topo,
        Some(listener),
        Default::default(),
        None, // M6: boot computes the plan from the log store itself
    )
    .expect("boot routed");
    println!("cascade-server: ready on 127.0.0.1:{bound}");
    use std::io::Write;
    std::io::stdout().flush().ok();

    while !SHUTDOWN.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Graceful shutdown (T4.4, FR-8, §10.2 — verbatim, in order): the flag
    // and the accept wake start the drain on the serving housekeeper
    // (refuse new → drain in-flight statements, deadline-bounded → final
    // per-DB WAL flush, home reactor first → final flush pass on every
    // designated reactor → clean marker); the joins here are §10.2 step 5 —
    // dropping each reactor kills its session tasks and closes their fds,
    // rings, and WAL handles. The trace in the report is the sequence,
    // completion-ordered.
    let (soft, hard) = rt.poll_stalls();
    let wal = rt.debug_wal().cloned();
    let report = rt.shutdown();
    eprintln!("cascade-server: shutdown phases: {:?}", report.phases);
    // M6: the clean marker died with the file path — the log store
    // handles its own tail at open (§5.7), so the next boot replays the
    // durable log regardless.

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
