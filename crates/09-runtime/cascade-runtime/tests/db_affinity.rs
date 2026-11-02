//! T5.5 (FR-10, FR-12, FR-38): the affinity DDL surface drives the LIVE
//! snapshot. T2.4 proved routing honors the *boot-config* snapshot; these
//! tests prove a database created over the wire — `CREATE DATABASE … WITH
//! numa_affinity = …` — lands in the snapshot the writer publishes and that
//! routing follows it with no restart and no cache invalidation.
//!
//! Every boot here uses a **cross** cluster default deliberately: the
//! discriminator for each claim is the created ROW, not the cluster default
//! (the T2.4 test-review trap — a `single` default would make "routes to the
//! bound NUMA" true even for a router that never read the row).

use std::{
    sync::{
        atomic::Ordering,
        Mutex,
    },
    time::{Duration, Instant},
};

use cascade_runtime::{
    config::{AuthConfig, NumaAffinity, NumaPool, NumaWorkers},
    pin,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

/// Pin-heavy boots (1 housekeeper + 3 workers per runtime) must not overlap
/// (deployment.rs discipline): overlapping boots fail on the
/// core-verified-twice check for contention, not bugs.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Fixture {
    addr: String,
    _rt: Runtime,
}

/// hk + two workers on NUMA 0 + one worker on NUMA 1, cap 1 per worker,
/// cluster default **cross**. Under the A13 pick (fewest workers,
/// config-order tie-break) a created `single` DB binds NUMA 1 — the
/// non-obvious choice, same reasoning as affinity.rs's fixture.
fn boot() -> Fixture {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 4, "db_affinity tests need at least 4 CPUs");
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
        // NIC tie-breaker pinned off: these landings assume the count-only
        // tie order (first-wins) — loadview.rs owns the composite's determinism.
        nic_numa: None,
        max_conns_per_worker: 1,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    cascade_runtime::testutil::arm_suite_watchdog();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default(), None)
        .expect("boot routed");
    // Hang diagnostics, read at fire time (Arc-backed clones): ticks frozen
    // ⇒ dead housekeeper ring; ticks advancing + reaped == 0 while a client
    // hangs ⇒ dead worker ring.
    let orch = rt.orchestrator().cloned();
    let reap = rt.reap_stats().cloned();
    cascade_runtime::testutil::on_hang(
        format!("db_affinity-fixture runtime on 127.0.0.1:{port}"),
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

fn client(fx: &Fixture, db: &str, user: &str) -> postgres::Client {
    postgres::Client::connect(
        &format!("postgres://{user}:pw@{}/{db}", fx.addr),
        postgres::NoTls,
    )
    .expect("client connects")
}

fn sqlstate(err: &postgres::Error) -> String {
    err.as_db_error()
        .map(|e| e.code().code().to_owned())
        .unwrap_or_else(|| format!("not a db error: {err}"))
}

fn err_message(err: &postgres::Error) -> String {
    err.as_db_error()
        .map(|e| e.message().to_owned())
        .unwrap_or_else(|| format!("not a db error: {err}"))
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

/// The reactor thread a connection's session runs on (`debug_worker()`).
fn worker_name(c: &mut postgres::Client) -> String {
    c.simple_query("SELECT debug_worker()")
        .expect("debug_worker")
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).expect("name").to_owned()),
            _ => None,
        })
        .next()
        .expect("exactly one row")
}

/// The per-database affinity rows THIS connection's reactor serves from
/// its own snapshot: `(database, affinity, numa)` — numa `-` when the row
/// binds nothing.
fn db_rows(c: &mut postgres::Client) -> Vec<(String, String, String)> {
    c.simple_query("SELECT debug_databases()")
        .expect("debug_databases")
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some((
                r.get(0).expect("database").to_owned(),
                r.get(1).expect("affinity").to_owned(),
                r.get(2).expect("numa").to_owned(),
            )),
            _ => None,
        })
        .collect()
}

/// The writer's submit counter, summed over the workers (FR-36's probe).
fn submitted(fx: &Fixture) -> u64 {
    fx._rt
        .tierc_write_stats()
        .iter()
        .map(|s| s.submitted.load(Ordering::Relaxed))
        .sum()
}

/// The flagship: the DDL creates the row in the live snapshot, and ROUTING
/// follows it — a `nova` connection lands only on the bound NUMA's worker
/// (A13 picked NUMA 1: one worker there vs two on NUMA 0) while the
/// identically-shaped `dev` connection sits on NUMA 0 under the cross
/// default. With the bound NUMA's worker at cap, the next `nova` connection
/// is refused 53300 naming NUMA 1 — never silently crossed (FR-12) — and a
/// router still reading the cross default would have ROUTED it to NUMA 0
/// instead, so the row (not the default) is what decided.
#[test]
fn created_single_database_routes_within_its_bound_numa_and_refuses_at_cap() {
    let _serial = serial();
    let fx = boot();
    let rt = &fx._rt;
    wait_until(
        || rt.tierc().load_full().is_some(),
        "the boot publication lands on the serving catalog",
    );

    // The DDL issuer rides the cluster default (cross): least-loaded tie
    // → the first route. Settled before anything else connects — see
    // affinity.rs for why the count view wants settling.
    let mut a = client(&fx, "dev", "alice");
    wait_until(|| rt.workers()[0].active_connections() == 1, "issuer on route 0");

    a.simple_query("CREATE DATABASE nova WITH numa_affinity = 'single'")
        .expect("CREATE DATABASE acks");

    // The row is in the LIVE snapshot by ack time (R11: the ack follows
    // the stores) — read straight off the serving catalog, the same
    // pointer routing reads.
    wait_until(
        || {
            matches!(
                rt.tierc()
                    .load()
                    .as_ref()
                    .map(|s| s.resolve(Some("nova"))),
                Some(p) if p.affinity == NumaAffinity::Single && p.bound_numa == Some(1)
            )
        },
        "the created row lands in the serving snapshot bound to NUMA 1",
    );

    // A connection to `nova` must land on the NUMA 1 worker. Had the row
    // been missed (stale config, absent row), `nova` would resolve cross
    // and least-loaded would send it to the IDLE NUMA 0 worker — route 1 —
    // which is exactly what the zero below forbids.
    let mut n1 = client(&fx, "nova", "bob");
    wait_until(
        || rt.workers()[2].active_connections() == 1,
        "the nova connection lands on its bound NUMA 1 worker",
    );
    assert_eq!(
        rt.workers()[1].active_connections(),
        0,
        "I13: the created single DB never crossed off its bound NUMA"
    );
    n1.simple_query("INSERT INTO debug_table VALUES (1, 'nova-serving')")
        .expect("the bound worker serves");

    // The bound NUMA's only worker is at cap 1: refuse, naming the NUMA —
    // the FR-12 wording, driven by a CREATED row this time (T2.4 proved it
    // from boot config).
    let mut n2 = client(&fx, "nova", "carol");
    let err = n2
        .simple_query("SELECT 1")
        .expect_err("drained bound NUMA refuses");
    assert_eq!(sqlstate(&err), "53300", "FR-12 refusal code");
    assert!(
        err_message(&err).contains("NUMA 1"),
        "refusal names the bound NUMA, got: {}",
        err_message(&err)
    );
    assert_eq!(
        rt.workers()[1].active_connections(),
        0,
        "the refused connection was never adopted anywhere"
    );
    n1.simple_query("INSERT INTO debug_table VALUES (2, 'nova-still-serving')")
        .expect("the bound worker keeps serving");
}

/// FR-38's record half: `CREATE … WITH numa_affinity` lands as a
/// `datnumaffinity` row (`debug_databases()` renders it), the bare form
/// freezes the cluster default INTO a row, and both are visible on ANOTHER
/// reactor's connection with no cache invalidation — the pointer load
/// again. The submit counter pins FR-36 for the new arm: two DDL, two
/// submissions, and the reads submit nothing (the query path's submit_to
/// arms stay exactly the DDL ones).
#[test]
fn created_rows_are_recorded_and_visible_on_another_reactor() {
    let _serial = serial();
    let fx = boot();

    let mut a = client(&fx, "dev", "alice");
    let mut b = client(&fx, "dev", "bob");
    // Least-loaded determinism: a → route 0, b → route 1. The test refuses
    // to make cross-reactor claims on same-reactor luck (ddl.rs's rule).
    let (wa, wb) = (worker_name(&mut a), worker_name(&mut b));
    assert_ne!(
        wa, wb,
        "the two connections must sit on different reactors for this test \
         to say anything (got {wa:?} twice)"
    );
    assert!(db_rows(&mut a).is_empty(), "a fresh cluster has no rows");
    assert_eq!(submitted(&fx), 0, "connections alone never submit");

    a.simple_query("CREATE DATABASE nova WITH numa_affinity = 'single'")
        .expect("explicit affinity acks");
    a.simple_query("CREATE DATABASE plain").expect("bare form acks");
    assert_eq!(
        submitted(&fx),
        2,
        "two CREATE DATABASE statements, exactly two submit_to crossings"
    );

    // datnumaffinity recorded — explicit, and the bare form's frozen
    // default (cross here, binding nothing). Sorted, the text spellings
    // the wire carries.
    let expected = vec![
        ("nova".to_owned(), "single".to_owned(), "1".to_owned()),
        ("plain".to_owned(), "cross".to_owned(), "-".to_owned()),
    ];
    assert_eq!(
        db_rows(&mut b),
        expected,
        "the other reactor serves the rows after the ack — no invalidation"
    );
    assert_eq!(db_rows(&mut a), expected, "the issuing reactor serves them too");
    assert_eq!(
        submitted(&fx),
        2,
        "the debug_databases reads submitted nothing (FR-36: reads never cross)"
    );
}

/// FR-38's honest-refusal half, over the wire: `ALTER DATABASE … SET
/// numa_affinity` is VALIDATED (a garbage value is 22023 — the parameter
/// is known) and then refused `unsupported_feature` (0A000) — the
/// migration runner is a later deliverable, and no migration is attempted:
/// the row is untouched and routing still honors the ORIGINAL binding.
#[test]
fn alter_database_affinity_is_validated_then_refused_and_changes_nothing() {
    let _serial = serial();
    let fx = boot();
    let rt = &fx._rt;

    let mut a = client(&fx, "dev", "alice");
    a.simple_query("CREATE DATABASE nova WITH numa_affinity = 'single'")
        .expect("create acks");
    let baseline = db_rows(&mut a);
    assert_eq!(
        baseline,
        vec![("nova".to_owned(), "single".to_owned(), "1".to_owned())]
    );

    // Well-formed: parsed, resolved — refused by the FEATURE, with the
    // unsupported_feature code the plan pins.
    let err = a
        .simple_query("ALTER DATABASE nova SET numa_affinity = 'cross'")
        .expect_err("ALTER is refused");
    assert_eq!(sqlstate(&err), "0A000", "unsupported_feature");
    assert!(
        err_message(&err).contains("not supported"),
        "the refusal names the limitation, got: {}",
        err_message(&err)
    );

    // Garbage value: refused as a VALUE (22023) — validation happens
    // before the feature question, on the wire, not just in resolution.
    let err = a
        .simple_query("ALTER DATABASE nova SET numa_affinity = 'sideways'")
        .expect_err("bad value refused");
    assert_eq!(sqlstate(&err), "22023", "invalid_parameter_value");

    // Any other parameter names a GUC store this engine does not have —
    // refused, not silently stored somewhere invisible.
    let err = a
        .simple_query("ALTER DATABASE nova SET work_mem = '64MB'")
        .expect_err("unknown parameter refused");
    assert_eq!(sqlstate(&err), "0A000");

    // No state changed — the test-review bullet, asserted on the rows…
    assert_eq!(
        db_rows(&mut a),
        baseline,
        "three refusals, zero mutations"
    );
    // …and on ROUTING: the original binding still decides (an attempted
    // migration would have re-homed the row; the cross default would have
    // sent a fresh nova connection to the idle route 1).
    let _n1 = client(&fx, "nova", "bob");
    wait_until(
        || rt.workers()[2].active_connections() == 1,
        "routing unchanged: nova still binds NUMA 1 after the refusals",
    );
    assert_eq!(
        rt.workers()[1].active_connections(),
        0,
        "the refusal attempted no migration"
    );
}

/// The writer-refusal path for the new change: a duplicate name is 42P04
/// (PG's duplicate_database) naming the database, the refused apply
/// mutated nothing (the affinity half included), and the writer serves on.
/// A transaction block is refused like its CREATE ROLE sibling (25001) —
/// instant-apply DDL cannot honestly roll back — and creates nothing.
#[test]
fn duplicate_create_refuses_and_a_transaction_block_creates_nothing() {
    let _serial = serial();
    let fx = boot();
    let mut a = client(&fx, "dev", "alice");

    a.simple_query("CREATE DATABASE nova WITH numa_affinity = 'single'")
        .expect("first create acks");

    // The duplicate — asked for with a DIFFERENT affinity, so a silent
    // upsert would show here — is refused by the writer.
    let err = a
        .simple_query("CREATE DATABASE nova WITH numa_affinity = 'cross'")
        .expect_err("duplicate refused");
    assert_eq!(sqlstate(&err), "42P04", "duplicate_database");
    assert!(
        err_message(&err).contains("database \"nova\" already exists"),
        "PG's wording, got: {}",
        err_message(&err)
    );
    assert_eq!(
        db_rows(&mut a),
        vec![("nova".to_owned(), "single".to_owned(), "1".to_owned())],
        "the refused CROSS re-create did not overwrite the row"
    );

    // In a transaction block: refused before the bounce (25001), nothing
    // created.
    a.simple_query("BEGIN").expect("begin");
    let err = a
        .simple_query("CREATE DATABASE ghost")
        .expect_err("DDL in a block is refused");
    assert_eq!(sqlstate(&err), "25001", "active_sql_transaction");
    a.simple_query("ROLLBACK").expect("rollback");
    assert_eq!(db_rows(&mut a).len(), 1, "the ghost never got a row");

    // The writer is alive after the refusals: the next create applies and
    // sorts in.
    a.simple_query("CREATE DATABASE phoenix").expect("writer serves on");
    assert_eq!(
        db_rows(&mut a),
        vec![
            ("nova".to_owned(), "single".to_owned(), "1".to_owned()),
            ("phoenix".to_owned(), "cross".to_owned(), "-".to_owned()),
        ]
    );
}
