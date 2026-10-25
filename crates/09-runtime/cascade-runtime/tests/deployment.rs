//! T2.7: kernel-deployment verification (FR-4, §2.4).
//!
//! The log-capture tests assert *loudness* (the record's level is
//! `Warn`, not merely that something was logged) and the code-review
//! contract — a mismatch never refuses boot, and boot's comparison reads
//! the kernel's cmdline, not the config (a boot that "verified" against
//! the config would find no mismatch anywhere and fail the wiring test
//! on any unisolated host, which this one is).

use std::{
    fs,
    sync::{Arc, Mutex},
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin, deployment, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};
use cascade_telemetry::{self, Level};

type Captured = Arc<Mutex<Vec<(Level, String, Vec<(String, String)>)>>>;

/// One sink at a time, and boots inside capture run one at a time — the
/// sink is process-global and tests within this binary run in parallel.
static SINK_LOCK: Mutex<()> = Mutex::new(());

fn capture() -> (Captured, Arc<dyn Fn()>) {
    let records: Captured = Arc::new(Mutex::new(Vec::new()));
    let sink_records = Arc::clone(&records);
    let new_sink = Arc::new(move |level: Level, event: &str, fields: &[(&str, &str)]| {
        sink_records.lock().unwrap().push((
            level,
            event.to_owned(),
            fields.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        ));
    });
    // `take()` in the unlock keeps the closure `Fn` (callable on retry).
    let prev = Mutex::new(cascade_telemetry::set_log_sink(Some(new_sink)));
    let unlock = Arc::new(move || {
        cascade_telemetry::set_log_sink(prev.lock().unwrap().take());
    });
    (records, unlock)
}

fn cfg(hk: u32, workers: &[u32]) -> RuntimeConfig {
    RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: workers.to_vec(),
        }],
        numa_pools: vec![NumaPool { numa: 0, size_mb: 1 }],
        ..RuntimeConfig::default()
    }
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Synthetic full match: nothing is logged (the "match → no warning" half,
/// at the boot wiring's own function).
#[test]
fn full_match_logs_nothing() {
    let _guard = SINK_LOCK.lock().unwrap();
    let (records, unlock) = capture();

    let cfg = cfg(0, &[2, 3]);
    let kernel_text = "quiet isolcpus=2,3 nohz_full=2-3 rcu_nocbs=2,3 irqaffinity=0";
    let mismatches = deployment::verify_and_log(&cfg, Some(kernel_text));

    unlock();
    assert!(mismatches.is_empty(), "a matching deployment has no mismatch");
    assert!(
        records.lock().unwrap().is_empty(),
        "a matching deployment logs nothing, got {:?}",
        records.lock().unwrap()
    );
}

/// Synthetic mismatch: the warning is logged *loudly* — Level::Warn, one
/// record per uncovered parameter, naming the parameter and the exact
/// uncovered cores (the test review's loudness assertion).
#[test]
fn mismatch_warns_loudly_with_param_and_cores() {
    let _guard = SINK_LOCK.lock().unwrap();
    let (records, unlock) = capture();

    // Worker 3 is missing from nohz_full; everything else covers its set.
    let cfg = cfg(0, &[2, 3]);
    let kernel_text = "quiet isolcpus=2,3 nohz_full=2 rcu_nocbs=2-3 irqaffinity=0";
    let mismatches = deployment::verify_and_log(&cfg, Some(kernel_text));

    unlock();
    assert_eq!(mismatches.len(), 1, "exactly one parameter uncovered");

    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1, "one mismatch → one record: {records:?}");
    let (level, event, fields) = &records[0];
    assert_eq!(*level, Level::Warn, "the mismatch warning is LOUD (Warn)");
    assert_eq!(event, "kernel_deployment_mismatch");
    assert_eq!(field(fields, "param"), Some("nohz_full"));
    assert_eq!(field(fields, "role"), Some("worker"));
    assert_eq!(field(fields, "cores"), Some("3"));
}

/// An unreadable kernel cmdline is an honest Info note, not silence and
/// not a fake verification.
#[test]
fn unreadable_cmdline_logs_info_and_verifies_nothing() {
    let _guard = SINK_LOCK.lock().unwrap();
    let (records, unlock) = capture();

    let mismatches = deployment::verify_and_log(&cfg(0, &[2]), None);

    unlock();
    assert!(mismatches.is_empty());
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let (level, event, _) = &records[0];
    assert_eq!(*level, Level::Info);
    assert_eq!(event, "kernel_deployment_unverified");
}

/// The boot wiring, on the real host: boot reads the kernel's own cmdline
/// (`/proc/cmdline`), never refuses on mismatch, and emits exactly the
/// warnings the kernel-vs-config comparison demands — zero only when the
/// host really is isolated. On this WSL2 host (no isolation params at
/// all) a boot that secretly "verified" against the config would log
/// nothing and fail this test.
#[test]
fn boot_warns_exactly_as_the_kernel_cmdline_demands_and_never_refuses() {
    let _guard = SINK_LOCK.lock().unwrap();

    // The expectation, from the kernel itself + the same FR-4 comparison.
    let real_cmdline = fs::read_to_string("/proc/cmdline").ok();
    let expected = match real_cmdline.as_deref() {
        Some(text) => {
            let allowed = pin::current_affinity();
            assert!(allowed.len() >= 2, "wiring test needs at least 2 CPUs");
            deployment::check_deployment(&cfg(allowed[0], &[allowed[1]]), &deployment::parse_cmdline(text)).len()
        }
        None => 0, // unreadable → the Info path, no mismatch records
    };

    let (records, unlock) = capture();
    let mut allowed = pin::current_affinity();
    allowed.truncate(2);
    let boot_cfg = cfg(allowed[0], &[allowed[1]]);
    let topo = SyntheticTopology::new(allowed.iter().copied());
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let boot = Runtime::boot_routed(&boot_cfg, &topo, Some(listener), AuthConfig::default());
    unlock();

    // FR-4: warn, never refuse — even a fully unisolated host boots.
    boot.expect("a kernel-deployment mismatch never refuses boot");

    let records = records.lock().unwrap();
    let mismatch_records: Vec<_> = records
        .iter()
        .filter(|(_, event, _)| event == "kernel_deployment_mismatch")
        .collect();
    assert_eq!(
        mismatch_records.len(),
        expected,
        "boot's warnings must match the kernel-vs-config comparison, got {records:?}"
    );
    for (level, _, _) in &mismatch_records {
        assert_eq!(*level, Level::Warn, "boot's mismatch warnings are LOUD");
    }
    let _ = port;
}

/// Bare metal only: on a host that was actually provisioned per §2.4
/// (isolcpus present), a runtime configured from the *kernel's* isolated
/// set boots with zero mismatch warnings — the end-to-end provisioning
/// check. Skips where isolcpus is absent (CI, WSL2, most VMs).
#[test]
#[ignore = "requires a kernel actually booted with isolcpus (bare metal)"]
fn provisioned_host_boots_without_warnings() {
    let _guard = SINK_LOCK.lock().unwrap();

    let text = fs::read_to_string("/proc/cmdline").expect("bare metal: cmdline readable");
    let kernel = deployment::parse_cmdline(&text);
    if kernel.isolcpus.len() < 2 {
        eprintln!("no usable isolcpus set on this host; nothing to verify");
        return;
    }
    // Provision as §2.4 prescribes: workers from the isolated set,
    // housekeeper from outside it.
    let workers: Vec<u32> = kernel.isolcpus.iter().take(2).copied().collect();
    let hk = (0..=u32::MAX)
        .find(|c| !kernel.isolcpus.contains(c))
        .expect("a non-isolated core for the housekeeper");

    let (records, unlock) = capture();
    let boot_cfg = cfg(hk, &workers);
    let topo = SyntheticTopology::new(workers.iter().copied().chain([hk]));
    let boot = Runtime::boot(&boot_cfg, &topo);
    unlock();

    boot.expect("provisioned cores boot");
    let records = records.lock().unwrap();
    let mismatch_records: Vec<_> = records
        .iter()
        .filter(|(_, event, _)| event == "kernel_deployment_mismatch")
        .collect();
    assert!(
        mismatch_records.is_empty(),
        "a provisioned host must not warn: {mismatch_records:?}"
    );
}
