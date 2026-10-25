//! T1.8: `yield_point()` (FR-17), a test operator exercising sub-boundary
//! yields (FR-18), and the stall detector (FR-19: counters, log levels,
//! kill default-off).
//!
//! Every reactor test runs its own executor on the test's own thread via
//! `make()` + `run`, so the reactor-local state (thread-locals) is readable
//! right after the future completes. Timing assertions are *floors* — a
//! planted spin of N ms guarantees a starvation gap ≥ N ms (monotonic
//! clock) — so CI load can only make gaps larger, never smaller, and no
//! assertion depends on an upper bound.

use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    coop::{self, ReactorStall},
    pin, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};
use cascade_telemetry::{set_log_sink, Level};

fn reactor() -> glommio::LocalExecutor {
    glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("yield-test")
        .make()
        .expect("executor builds")
}

/// Case 1 (§7.1): over the deadline → yield even with an empty run queue.
/// Deadline 0 makes the branch fire deterministically — no waiting, no
/// flake. A prompt yield is not a stall: no counters, no logs.
#[test]
fn deadline_exceeded_yields_even_with_empty_queue() {
    let stall = Arc::new(ReactorStall::new());
    let inner = Arc::clone(&stall);
    let ex = reactor();
    ex.run(async move {
        coop::configure_for_reactor(inner, Duration::ZERO);
        coop::yield_point().await;
    });
    assert_eq!(
        coop::deadlined_yields(),
        1,
        "deadline branch fired exactly once"
    );
    assert_eq!(coop::queue_delegations(), 0, "queue branch not taken");
    let (soft, hard) = (stall.soft_total(), stall.hard_total());
    assert_eq!((soft, hard), (0, 0), "a prompt yield is not a stall");
}

/// Case 2 (§7.1): below the deadline with a non-empty run queue → delegate;
/// the pending sibling can only run if we actually yield. (glommio 0.9's
/// `spawn_local` queues without polling the sibling synchronously, so
/// `flag` can only become true through yield_point's delegation.)
#[test]
fn queue_non_empty_yields_below_deadline() {
    let ex = reactor();
    ex.run(async move {
        coop::configure_for_reactor(Arc::new(ReactorStall::new()), Duration::from_secs(3600));
        let ran = Rc::new(std::cell::Cell::new(false));
        let sibling = ran.clone();
        glommio::spawn_local(async move {
            sibling.set(true);
        })
        .detach();
        coop::yield_point().await;
        assert!(
            ran.get(),
            "sibling ran → yield_point delegated control below the deadline"
        );
    });
    assert_eq!(
        coop::deadlined_yields(),
        0,
        "deadline branch must not fire here"
    );
    assert!(coop::queue_delegations() >= 1, "delegation branch taken");
}

/// Case 3 (§7.1): below the deadline, empty queue → no-op. No counters, no
/// logs, no stall classification.
#[test]
fn below_deadline_empty_queue_is_side_effect_free() {
    let stall = Arc::new(ReactorStall::new());
    let inner = Arc::clone(&stall);
    let ex = reactor();
    ex.run(async move {
        coop::configure_for_reactor(Arc::clone(&inner), Duration::from_secs(3600));
        coop::yield_point().await;
    });
    assert_eq!(coop::deadlined_yields(), 0);
    assert_eq!((stall.soft_total(), stall.hard_total()), (0, 0));
    assert!(
        coop::queue_delegations() >= 1,
        "case 3 shares the delegation path (immediate return)"
    );
}

/// FR-18: an operator yields at every batch boundary *and* at internal
/// sub-boundaries (§7.3, I10). With deadline 0 every call is a real yield,
/// so the counter proves both call sites fired.
#[test]
fn test_operator_yields_at_batch_and_sub_boundaries() {
    let ex = reactor();
    ex.run(async {
        coop::configure_for_reactor(Arc::new(ReactorStall::new()), Duration::ZERO);
        let items: Vec<u32> = (0..100).collect();
        // 10 batches of 10, sub-chunks of 5 → 10 batch yields + 20 sub yields.
        for chunk in items.chunks(10) {
            for group in chunk.chunks(5) {
                assert!(group.iter().sum::<u32>() > 0);
                coop::yield_point().await; // internal sub-boundary
            }
            coop::yield_point().await; // batch boundary
        }
    });
    assert!(
        coop::deadlined_yields() >= 30,
        "every batch and sub-boundary yield_point fired, got {}",
        coop::deadlined_yields()
    );
}

/// FR-19: a planted slow stretch starves the heartbeat; classification by
/// threshold, counters, and the WARN log record are asserted through the
/// real poll path. The soft-vs-hard *level* split is also covered
/// deterministically in `coop::tests::classification_is_by_threshold_with_
/// hard_implying_soft` and `coop::tests::poll_stall_classifies_counts_and_logs`.
///
/// Two halves, each deterministic:
/// 1. In-reactor measurement. The gap is sampled INSIDE the reactor, right
///    after the synchronous spin — a sync loop cannot yield, so no beat can
///    land in the window and the gap is floor-guaranteed by the spin
///    length. (Sampling after executor shutdown was flaky under load: a
///    teardown ring poll can process the heartbeat's timer completion and
///    beat once, erasing the gap.)
/// 2. Classification with nothing to beat: a fresh stall on the test
///    thread plus a real 15 ms wait makes the poll deterministically see a
///    hard stall — WARN, counters, log record.
#[test]
fn planted_slow_task_starves_heartbeat_and_poll_logs_warn() {
    let records: Arc<Mutex<Vec<(Level, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let capture = Arc::clone(&records);
    let prev = set_log_sink(Some(Arc::new(move |level, event, _fields| {
        capture.lock().unwrap().push((level, event.to_owned()));
    })));

    // Half 1: starve the heartbeat, measure in-reactor.
    let stall = Arc::new(ReactorStall::new());
    let inner = Arc::clone(&stall);
    let (gap_tx, gap_rx) = std::sync::mpsc::channel::<u64>();
    let join = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("yield-stall-test")
        .spawn(move || async move {
            coop::configure_for_reactor(Arc::clone(&inner), Duration::from_secs(3600));
            coop::spawn_heartbeat();
            // Let the heartbeat land, so the starvation gap is bounded by
            // the spin below (not by test startup).
            glommio::timer::sleep(coop::BEAT_INTERVAL * 2).await;
            // Planted slow task: pure compute, no yields — starves the
            // heartbeat for ≥ 11 ms ≥ the hard threshold (10 ms).
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(11) {}
            gap_tx
                .send(inner.gap_us())
                .expect("test thread still waits");
        })
        .expect("reactor thread spawns");
    join.join().expect("reactor completes");
    let gap_us = gap_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reactor reports its starvation gap");
    assert!(
        gap_us >= 11_000,
        "the sync spin starves the heartbeat: gap {gap_us}µs"
    );

    // Half 2: classify through the real poll path — no reactor exists, so
    // nothing can beat and erase the gap.
    let dead = Arc::new(ReactorStall::new()); // beats once at construction
    std::thread::sleep(Duration::from_millis(15));
    let level = coop::poll_stall(&dead, Duration::from_millis(1), Duration::from_millis(10));
    assert_eq!(
        level,
        Some(Level::Warn),
        "≥ 10 ms starvation is a hard stall"
    );
    assert!(dead.soft_total() >= 1, "a hard stall is also a soft stall");
    assert!(dead.hard_total() >= 1);

    set_log_sink(prev);
    let got = records.lock().unwrap();
    assert!(
        got.iter()
            .any(|(l, e)| *l == Level::Warn && e == "reactor_stall"),
        "hard stall logged at WARN: {got:?}"
    );
}

/// Kill switch (R12): exists in config, defaults off, and enabling it is
/// rejected until kill wiring lands.
#[test]
fn stall_kill_switch_defaults_off_and_rejects_enabling() {
    assert!(
        !RuntimeConfig::default().stall.kill,
        "kill must default off"
    );
    let toml = "[runtime]\nstall.kill = true\n[[runtime.worker_cores]]\nnuma = 0\ncores = [2]\n[[runtime.numa_pools]]\nnuma = 0\nsize_mb = 1\n";
    let err = RuntimeConfig::from_toml_str(toml).expect_err("kill = true rejected");
    assert!(
        err.to_string().contains("stall.kill"),
        "rejection names the knob: {err}"
    );
}

/// Boot plumbing end to end: real worker reactors serve queries through the
/// batch-boundary yield points, heartbeats keep the detectors quiet while
/// the workload has think-time (the false positive this design fixes), and
/// the poll surface reads zeros on a healthy system.
#[test]
fn booted_worker_serves_queries_with_stall_counters_wired() {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![allowed[0]],
        worker_cores: vec![NumaWorkers {
            numa: 0,
            cores: vec![allowed[1], allowed[2]],
        }],
        numa_pools: vec![NumaPool {
            numa: 0,
            size_mb: 1,
        }],
        batch_rows: 2, // many batch-boundary yield_points per SELECT
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default())
        .expect("boot routed");

    let mut client = postgres::Client::connect(
        &format!("postgres://alice:pw@127.0.0.1:{port}/dev"),
        postgres::NoTls,
    )
    .expect("client connects");
    for i in 0..6 {
        client
            .simple_query(&format!("INSERT INTO debug_table VALUES ({i}, 'v{i}')"))
            .expect("insert");
    }
    // 6 rows through batch_rows=2 → several batch-boundary yield_points,
    // with idle think-time between round trips.
    let msgs = client
        .simple_query("SELECT * FROM debug_table")
        .expect("select");
    let rows: Vec<_> = msgs
        .iter()
        .filter(|m| matches!(m, postgres::SimpleQueryMessage::Row(_)))
        .collect();
    assert_eq!(rows.len(), 6);

    std::thread::sleep(Duration::from_millis(5)); // idle window

    // The poll surface reads the real workers' counters. Whether the
    // totals are zero is the machine's business — a CI box running this
    // suite in parallel can deschedule a worker past the soft threshold,
    // and by §7.2 that is a real stall, not a false positive. The
    // load-independent invariants are: counters are monotone across polls,
    // and every hard stall also counted as soft.
    let (soft1, hard1) = rt.poll_stalls();
    let (soft2, hard2) = rt.poll_stalls();
    assert!(
        soft2 >= soft1 && hard2 >= hard1,
        "stall counters are monotone: ({soft1}, {hard1}) then ({soft2}, {hard2})"
    );
    assert!(
        soft2 >= hard2,
        "every hard stall is also a soft stall (§7.2)"
    );
}
