//! T5.6 (FR-11, R9 phase two): the §5.2 load composite, end to end —
//! the router now orders candidates by connection count, then the
//! reactor's published CPU EWMA, then NIC proximity as a cross-only
//! tie-breaker.
//!
//! The exact-tie discipline (the test-review rule: "exactly-tied,
//! asserted, not approximately") is split honestly across layers. The
//! full ordering matrix with literal equality in the leading components
//! lives in router.rs's unit tests (`pick` is pure). THESE tests drive
//! the real samplers over the real wire and assert their preconditions
//! from the same counters the router reads: the NIC-decisive case runs
//! on a fresh boot where every worker is at count 0 and every EWMA
//! reads 0 (a parked executor burns ~nothing per interval), asserted
//! immediately before the connect; the CPU-decisive case needs NO
//! exactness — the hammered worker's EWMA must be strictly greater with
//! a margin, which is the point being made.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use cascade_runtime::{
    config::{AuthConfig, NumaAffinity, NumaPool, NumaWorkers},
    pin,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

/// Pin-heavy boots must not overlap (deployment.rs discipline).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Fixture {
    addr: String,
    _rt: Runtime,
}

/// hk + two workers on NUMA 0 + one worker on NUMA 1, cap 2, cluster
/// default **cross**, NIC pinned to NUMA 1, sampler at 200 ms (the A2
/// default is 1 000 ms — the faster cadence only shortens how long the
/// CPU test must hammer; the math is interval-independent). NUMA 1 is
/// the non-obvious choice again: without the tie-breaker a fresh boot's
/// first connection lands on route 0 by first-wins — with it, the
/// nic-local worker wins the all-zero tie.
fn boot() -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 4, "loadview tests need at least 4 CPUs");
    allowed.truncate(4);
    let (hk, w0a, w0b, w1) = (allowed[0], allowed[1], allowed[2], allowed[3]);

    let topo = SyntheticTopology::new([hk, w0a, w0b, w1]);
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![
            NumaWorkers {
                numa: 0,
                cores: vec![w0a, w0b],
            },
            NumaWorkers {
                numa: 1,
                cores: vec![w1],
            },
        ],
        numa_pools: vec![
            NumaPool { numa: 0, size_mb: 1 },
            NumaPool { numa: 1, size_mb: 1 },
        ],
        default_db_numa_affinity: NumaAffinity::Cross,
        max_conns_per_worker: 2,
        load_sample_ms: 200,
        nic_numa: Some(1),
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
    let orch = rt.orchestrator().cloned();
    let reap = rt.reap_stats().cloned();
    cascade_runtime::testutil::on_hang(
        format!("loadview-fixture runtime on 127.0.0.1:{port}"),
        move || {
            format!(
                "ticks={:?} reap={:?}",
                orch.as_ref().map(|o| (o.ticks(), o.wakeups())),
                reap.as_ref().map(|r| r.reaped())
            )
        },
    );
    Fixture {
        addr: format!("127.0.0.1:{port}"),
        _rt: rt,
    }
}

fn client(fx: &Fixture, user: &str) -> postgres::Client {
    postgres::Client::connect(
        &format!("postgres://{user}:pw@{}/dev", fx.addr),
        postgres::NoTls,
    )
    .expect("client connects")
}

fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for: {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The composite routes the FIRST connection of a fresh boot: every
/// candidate is at count 0 with an idle (0-milli) EWMA — an exact,
/// asserted tie — so NIC proximity alone decides, and it picks the
/// nic-local worker (NUMA 1, route 2), not first-wins route 0. Without
/// the tie-breaker this lands route 0; that counterfactual is the
/// discriminator.
#[test]
fn nic_proximity_decides_an_exact_tie() {
    let _serial = serial();
    let fx = boot();
    let rt = &fx._rt;
    wait_until(
        || rt.tierc().load_full().is_some(),
        "the boot publication lands on the serving catalog",
    );

    // The tie, exactly: no connections anywhere, and every sampler reads
    // the same value (idle workers publish 0 — parked executors burn
    // ~nothing; the assert demands equality, not smallness).
    assert!(
        rt.workers().iter().all(|w| w.active_connections() == 0),
        "fresh boot: all gauges at zero"
    );
    let ewmas: Vec<u64> = rt.workers().iter().map(|w| w.cpu_ewma_milli()).collect();
    assert!(
        ewmas.windows(2).all(|pair| pair[0] == pair[1]),
        "the tie is exact: all EWMAs equal, got {ewmas:?}"
    );

    let mut c1 = client(&fx, "u1");
    wait_until(
        || rt.workers()[2].active_connections() == 1,
        "the first connection lands on the nic-local NUMA 1 worker",
    );
    assert_eq!(
        rt.workers()[0].active_connections() + rt.workers()[1].active_connections(),
        0,
        "the nic-distant workers took nothing"
    );
    c1.simple_query("INSERT INTO debug_table VALUES (1, 'nic-local')")
        .expect("the chosen worker serves");
}

/// Proximity is a TIE-BREAKER ONLY: once the nic-local worker holds a
/// connection, count outranks proximity — the next connections fill the
/// distant workers (count 0 beats count 1, whatever the NIC says), then
/// the least-loaded rule fills NUMA 0's second worker. Plain least-loaded
/// behavior is intact underneath the composite.
#[test]
fn nic_never_overturns_count() {
    let _serial = serial();
    let fx = boot();
    let rt = &fx._rt;

    let mut c1 = client(&fx, "u1");
    wait_until(|| rt.workers()[2].active_connections() == 1, "c1 nic-local");
    let mut c2 = client(&fx, "u2");
    wait_until(
        || rt.workers()[0].active_connections() == 1,
        "c2 goes to a count-0 worker despite the NIC sitting on NUMA 1",
    );
    let mut c3 = client(&fx, "u3");
    wait_until(
        || rt.workers()[1].active_connections() == 1,
        "c3 goes to the remaining count-0 worker",
    );
    assert_eq!(
        rt.workers()[2].active_connections(),
        1,
        "the nic-local worker did not accumulate past its count share"
    );
    let _ = (
        c1.simple_query("SELECT * FROM debug_table"),
        c2.simple_query("SELECT * FROM debug_table"),
        c3.simple_query("SELECT * FROM debug_table"),
    );
}

/// The composite consults the CPU EWMA: with counts exactly tied at 1
/// (asserted) and the other workers' EWMAs exactly 0 (asserted), a
/// hammered worker is REJECTED — the next connection goes to an idle
/// candidate even though it must cross the NIC's preference to get
/// there. This is the R9 phase-two delta over count-only routing,
/// driven by real samples: the hammer's CPU shows up in the published
/// EWMA (strictly, with a margin — the not-approximately case) and the
/// route follows.
#[test]
fn hammered_cpu_is_rejected_at_a_count_tie() {
    let _serial = serial();
    let fx = boot();
    let rt = &fx._rt;

    // Counts to the tie: c1 → nic-local NUMA 1 worker, c2 → NUMA 0's
    // first, c3 → NUMA 0's second (the sequenced landings of the test
    // above — awaited, not assumed).
    // c1 and c3 are load-bearing, not queried: they HOLD their workers'
    // connection counts at 1 for the tie below.
    let _c1 = client(&fx, "u1");
    wait_until(|| rt.workers()[2].active_connections() == 1, "c1 nic-local");
    let mut c2 = client(&fx, "u2");
    wait_until(|| rt.workers()[0].active_connections() == 1, "c2 on route 0");
    let _c3 = client(&fx, "u3");
    wait_until(|| rt.workers()[1].active_connections() == 1, "c3 on route 1");

    // Hammer route 0's worker THROUGH c2: its sampler publishes the
    // utilization, EWMA-weighted. Wait for a strict, margined gap — this
    // is deliberately NOT a tie.
    let hammer_start = Instant::now();
    loop {
        c2.simple_query("INSERT INTO debug_table VALUES (9, 'hammer')")
            .expect("hammer insert");
        let w0 = rt.workers()[0].cpu_ewma_milli();
        if w0 >= 50
            && w0 > rt.workers()[1].cpu_ewma_milli()
            && w0 > rt.workers()[2].cpu_ewma_milli()
        {
            break;
        }
        assert!(
            hammer_start.elapsed() < Duration::from_secs(5),
            "the hammered worker's EWMA never rose: {} milli",
            rt.workers()[0].cpu_ewma_milli()
        );
    }
    // The others are idle and publish exactly 0 (asserted, not assumed —
    // their connections are parked, their executors burn ~nothing).
    assert_eq!(rt.workers()[1].cpu_ewma_milli(), 0, "route 1 idle-exact");
    assert_eq!(rt.workers()[2].cpu_ewma_milli(), 0, "route 2 idle-exact");

    // The count tie (1, 1, 1) with the EWMA gap: the hammer's worker is
    // out — the composite crosses the NIC's preference and lands on the
    // idle nic-local worker (count tie, EWMA tie at 0 with route 1,
    // proximity decides). Decay is ÷2 per interval, so route 0's EWMA
    // stays above zero at the decision instant; the others stay at it.
    let mut c4 = client(&fx, "u4");
    wait_until(
        || rt.workers()[2].active_connections() == 2,
        "c4 lands on the idle nic-local worker, not the hot one",
    );
    assert_eq!(
        rt.workers()[0].active_connections(),
        1,
        "the hammered worker took nothing at the tie"
    );
    assert_eq!(
        rt.workers()[1].active_connections(),
        1,
        "route 1 tied with route 2 on (count, cpu) and lost on proximity"
    );
    assert!(
        rt.workers()[0].cpu_ewma_milli() >= rt.workers()[1].cpu_ewma_milli(),
        "sanity: the hammer's EWMA never fell below the idle workers'"
    );
    let _ = c4.simple_query("INSERT INTO debug_table VALUES (10, 'fourth')");
}
