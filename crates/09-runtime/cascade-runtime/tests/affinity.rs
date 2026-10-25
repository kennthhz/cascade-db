//! T2.4: affinity routing + refusal (FR-10, FR-11 count phase, FR-12; I13).
//!
//! Driven end-to-end through the real `postgres` client: `single` DBs route
//! only within their bound NUMA (and refuse with 53300 naming it when that
//! NUMA is drained), `cross` DBs route machine-wide, and the cluster
//! default is *inherited* by the DB row that has no explicit affinity.

use std::time::{Duration, Instant};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin, config::NumaAffinity, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

struct Fixture {
    addr: String,
    _rt: Runtime,
}

/// hk + two workers on NUMA 0 + one worker on NUMA 1, cap 1 per worker, one
/// pool per NUMA. Under the A13 boot pick (fewest workers, config-order
/// tie-break) a `single` cluster binds NUMA 1 — the *non-obvious* choice,
/// which is what makes these tests catch a router that naively hardcodes
/// "first NUMA".
fn boot(default: NumaAffinity) -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 4, "affinity tests need at least 4 CPUs");
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
        default_db_numa_affinity: default,
        max_conns_per_worker: 1,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default())
        .expect("boot routed");
    Fixture {
        addr: format!("127.0.0.1:{port}"),
        _rt: rt,
    }
}

fn url(fx: &Fixture, user: &str) -> String {
    format!("postgres://{user}:pw@{}/dev", fx.addr)
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

/// `single`: the connection goes to the bound NUMA's worker (A13 picked
/// NUMA 1 — the least-loaded one), never across; a second connection finds
/// the bound NUMA drained and is refused with 53300 naming NUMA 1 (FR-12,
/// R10).
#[test]
fn single_routes_within_bound_numa_and_refuses_when_drained() {
    let fx = boot(NumaAffinity::Single);

    // FR-10: routing reads the snapshot, and the snapshot's A13 pick is
    // NUMA 1 (one worker there vs two on NUMA 0).
    assert_eq!(fx._rt.tierc().resolve(Some("dev")).bound_numa, Some(1));

    // Held for its side effect: the live connection occupying the bound
    // NUMA's only worker.
    let _c1 = postgres::Client::connect(&url(&fx, "alice"), postgres::NoTls)
        .expect("first connection served");
    let rt = &fx._rt;
    wait_until(
        || rt.workers()[2].active_connections() == 1,
        "connection lands on the bound NUMA 1 worker",
    );
    assert_eq!(
        rt.workers()[0].active_connections() + rt.workers()[1].active_connections(),
        0,
        "I13: no connection crosses off the bound NUMA"
    );

    // Bound NUMA drained (its only worker is at cap 1): refuse, naming it.
    let mut c2 = postgres::Client::connect(&url(&fx, "bob"), postgres::NoTls)
        .expect("handshake completes before routing");
    let err = c2
        .simple_query("SELECT 1")
        .expect_err("drained bound NUMA refuses");
    let db = err.as_db_error().expect("refusal is a PG error");
    assert_eq!(db.code().code(), "53300", "FR-12 refusal code");
    assert!(
        db.message().contains("NUMA 1"),
        "refusal names the bound NUMA, got: {}",
        db.message()
    );
    assert_eq!(
        rt.workers()[0].active_connections() + rt.workers()[1].active_connections(),
        0,
        "refused connection was never adopted anywhere"
    );
}

/// `cross`: machine-wide — after the two NUMA 0 workers fill to cap, the
/// third connection lands on the NUMA 1 worker instead of being refused.
/// This is the exact scenario `single` refuses in the test above.
#[test]
fn cross_routes_machine_wide() {
    let fx = boot(NumaAffinity::Cross);
    let rt = &fx._rt;

    // Connect one at a time, letting each count land before the next: the
    // router's count view is best-effort (the admitted T2.4 race), so
    // back-to-back connects can all observe load 0 and pile onto the first
    // route. Settled counts make least-loaded deterministic.
    let mut c1 = postgres::Client::connect(&url(&fx, "u1"), postgres::NoTls)
        .expect("first connection");
    wait_until(|| rt.workers()[0].active_connections() == 1, "c1 on route 0");
    let mut c2 = postgres::Client::connect(&url(&fx, "u2"), postgres::NoTls)
        .expect("second connection");
    wait_until(|| rt.workers()[1].active_connections() == 1, "c2 on route 1");
    let mut c3 = postgres::Client::connect(&url(&fx, "u3"), postgres::NoTls)
        .expect("third connection crosses to NUMA 1");
    wait_until(
        || rt.workers()[2].active_connections() == 1,
        "c3 crosses to the NUMA 1 worker",
    );

    assert_eq!(
        rt.workers()[2].active_connections(),
        1,
        "cross affinity uses workers on any NUMA"
    );
    let _ = (c1.simple_query("SELECT 1"), c2.simple_query("SELECT 1"));
    let _ = c3.simple_query("SELECT 1");
}

/// FR-10's inheritance clause: the database row has no explicit affinity,
/// so the *cluster default* decides. The cross boot here is the
/// discriminator — a router that hardcoded `single` (the enum's own
/// default, the "value happened to match" trap from the test review) would
/// refuse the third connection the cross boot accepts. The default is
/// confirmed to ride the snapshot, not the config object the router cannot
/// even see.
#[test]
fn cluster_default_is_inherited_by_dbs_without_explicit_affinity() {
    let fx = boot(NumaAffinity::Cross);
    assert_eq!(
        fx._rt.tierc().default_affinity(),
        NumaAffinity::Cross,
        "snapshot carries the cluster default"
    );
    // The client name "dev" has no explicit row: it inherits cross and
    // therefore succeeds machine-wide (single would refuse at cap — see
    // `single_routes_within_bound_numa_and_refuses_when_drained`, same
    // layout, same client name). Counts settle between connects — see the
    // cross test for why.
    let rt = &fx._rt;
    let mut c1 = postgres::Client::connect(&url(&fx, "u1"), postgres::NoTls)
        .expect("first connection");
    wait_until(|| rt.workers()[0].active_connections() == 1, "c1 on route 0");
    let mut c2 = postgres::Client::connect(&url(&fx, "u2"), postgres::NoTls)
        .expect("second connection");
    wait_until(|| rt.workers()[1].active_connections() == 1, "c2 on route 1");
    let mut c3 = postgres::Client::connect(&url(&fx, "u3"), postgres::NoTls)
        .expect("inherited cross: third connection crosses NUMAs instead of refusing");
    wait_until(
        || rt.workers()[2].active_connections() == 1,
        "c3 crosses to the NUMA 1 worker",
    );
    let _ = (c1.simple_query("SELECT 1"), c2.simple_query("SELECT 1"));
    let _ = c3.simple_query("SELECT 1");
}
