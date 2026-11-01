//! T5.4 (FR-29, FR-36): the DDL end-to-end — the bounce. `CREATE ROLE`
//! leaves the session through the Tier C write channel, the serialized
//! writer applies it, the publication fans out, and the ack comes back
//! — these tests drive that whole chain over the real wire.
//!
//! The plan's test-review question — "is the 'different worker' actually
//! pinned, or same-reactor luck on a 2-worker box?" — is answered with an
//! ASSERTION, not a hope: the router's least-loaded rule sends a second
//! live connection to the other worker deterministically, and the test
//! checks `debug_worker()` names differ before making any cross-worker
//! claim.
//!
//! The "no cache invalidation" half of the spec is behavioral too: there
//! is no invalidation mechanism in the system to disable, so the test
//! proves the read side is a pure pointer load — the second worker's
//! session sees the new role through its own catalog's snapshot, learned
//! only from the fan-out store that landed on its ring.

use std::{
    sync::{
        atomic::{Ordering},
        Mutex,
    },
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

/// Pin-heavy boots (1 housekeeper + 2 workers per runtime) must not
/// overlap (deployment.rs discipline): overlapping boots fail on the
/// core-verified-twice check for contention, not bugs.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Fixture {
    url: String,
    rt: Runtime,
}

fn boot() -> Fixture {
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
        format!("ddl-fixture runtime on 127.0.0.1:{port}"),
        move || {
            format!(
                "ticks={:?} reap={:?}",
                orch.as_ref().map(|o| (o.ticks(), o.wakeups())),
                reap.as_ref().map(|r| r.reaped())
            )
        },
    );
    Fixture {
        url: format!("postgres://alice:pw@127.0.0.1:{port}/dev"),
        rt,
    }
}

fn client(fx: &Fixture) -> postgres::Client {
    postgres::Client::connect(&fx.url, postgres::NoTls).expect("client connects")
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

/// The reactor thread a connection's session runs on (`debug_worker()`),
/// as a set of rows.
fn worker_rows(c: &mut postgres::Client) -> Vec<String> {
    c.simple_query("SELECT debug_worker()")
        .expect("debug_worker")
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).expect("name").to_owned()),
            _ => None,
        })
        .collect()
}

/// The role names THIS connection's reactor serves from its own snapshot.
fn role_rows(c: &mut postgres::Client) -> Vec<String> {
    c.simple_query("SELECT debug_roles()")
        .expect("debug_roles")
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).expect("role").to_owned()),
            _ => None,
        })
        .collect()
}

/// The flagship: after the ack, a DIFFERENT connection on a DIFFERENT
/// worker (asserted, not hoped) sees the role with no cache invalidation
/// — the read side is the pointer load. The duplicate is refused by the
/// WRITER with 42710 and the snapshot is unchanged on both reactors.
#[test]
fn create_role_reaches_a_different_workers_connection_without_invalidation() {
    let _serial = serial();
    let fx = boot();
    let mut a = client(&fx);

    // Baseline: no roles anywhere. The first connection lands on one
    // worker; the issuer-first property means ITS reactor's snapshot
    // serves the role the moment the ack lands.
    let worker_a = worker_rows(&mut a);
    assert_eq!(worker_a.len(), 1, "one session, one reactor: {worker_a:?}");
    assert!(role_rows(&mut a).is_empty(), "a fresh cluster has no roles");

    a.simple_query("CREATE ROLE prose").expect("CREATE ROLE acks");
    assert_eq!(
        role_rows(&mut a),
        vec!["prose".to_owned()],
        "the issuing reactor serves its own change at ack time"
    );

    // The second connection, WHILE the first is alive: the least-loaded
    // router must land it on the OTHER worker — and the test refuses to
    // proceed on luck: if this box somehow served both on one reactor,
    // the cross-worker claim below would be vacuous, so it fails here.
    let mut b = client(&fx);
    let worker_b = worker_rows(&mut b);
    assert_eq!(worker_b.len(), 1, "one session, one reactor: {worker_b:?}");
    assert_ne!(
        worker_a, worker_b,
        "the two connections must sit on different reactors for this \
         test to say anything (got {worker_a:?} twice)"
    );

    // The spec's visibility claim: B sees the role with no cache
    // invalidation — nothing pushed the role to B's session; B's
    // reactor's catalog adopted the published snapshot, and B's read
    // is a pointer load into it.
    assert_eq!(
        role_rows(&mut b),
        vec!["prose".to_owned()],
        "the other worker's connection sees the change after the ack"
    );

    // The duplicate is refused by the writer — PG's duplicate_object —
    // with the message naming the role.
    let err = b.simple_query("CREATE ROLE prose").expect_err("duplicate refused");
    assert_eq!(sqlstate(&err), "42710", "duplicate_object");
    assert!(
        err_message(&err).contains("role \"prose\" already exists"),
        "PG's wording"
    );

    // Failed apply → snapshot unchanged, on BOTH reactors: still exactly
    // one entry, no residue from the refused apply.
    assert_eq!(role_rows(&mut a), vec!["prose".to_owned()]);
    assert_eq!(role_rows(&mut b), vec!["prose".to_owned()]);
}

/// The failed-apply case on its own terms: the writer refuses, acks
/// Failed, and stays alive — the next change applies, and the OLTP path
/// is untouched by the refusal.
#[test]
fn failed_apply_refuses_and_the_writer_serves_on() {
    let _serial = serial();
    let fx = boot();
    let mut a = client(&fx);
    let mut b = client(&fx);
    let worker_a = worker_rows(&mut a);
    let worker_b = worker_rows(&mut b);
    assert_ne!(
        worker_a, worker_b,
        "cross-worker assertions need different reactors (got {worker_a:?} twice)"
    );

    a.simple_query("CREATE ROLE x").expect("first role");
    let err = b.simple_query("CREATE ROLE x").expect_err("duplicate refused");
    assert_eq!(sqlstate(&err), "42710");

    // The writer is alive after a Failed ack: a different role applies
    // and lands on both reactors, sorted.
    a.simple_query("CREATE ROLE y").expect("writer serves after a refusal");
    assert_eq!(role_rows(&mut a), vec!["x".to_owned(), "y".to_owned()]);
    assert_eq!(role_rows(&mut b), vec!["x".to_owned(), "y".to_owned()]);

    // And the OLTP path neither noticed nor cared: a commit round-trips.
    a.simple_query("INSERT INTO debug_table VALUES (1, 'after-ddl')")
        .expect("oltp insert after a refused DDL");
    let rows = a
        .simple_query("SELECT val FROM debug_table WHERE id = 1")
        .expect("point select");
    let vals: Vec<&str> = rows
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => r.get(0),
            _ => None,
        })
        .collect();
    assert_eq!(vals, vec!["after-ddl"]);
}

/// DDL is instant-apply on the writer, so a transaction block cannot
/// honestly contain it: refused with PG's active_sql_transaction code,
/// and the refused statement never created anything.
#[test]
fn create_role_inside_a_transaction_block_is_refused_and_creates_nothing() {
    let _serial = serial();
    let fx = boot();
    let mut c = client(&fx);

    c.simple_query("BEGIN").expect("begin");
    let err = c
        .simple_query("CREATE ROLE ghost")
        .expect_err("DDL in a block is refused");
    assert_eq!(sqlstate(&err), "25001", "active_sql_transaction");
    c.simple_query("ROLLBACK").expect("rollback");
    assert!(
        role_rows(&mut c).is_empty(),
        "the refused statement never created its role"
    );

    // Autocommit is the honest shape, and it works.
    c.simple_query("CREATE ROLE real").expect("autocommit DDL");
    assert_eq!(role_rows(&mut c), vec!["real".to_owned()]);
}

/// FR-36's test, not just the review bullet: the debug counter on the
/// write channels shows an N-query OLTP workload issues ZERO submit_to
/// calls from the query path — and exactly ONE when a DDL crosses.
#[test]
fn oltp_issues_zero_submits_and_ddl_exactly_one() {
    let _serial = serial();
    let fx = boot();
    let submitted = || -> u64 {
        fx.rt
            .tierc_write_stats()
            .iter()
            .map(|s| s.submitted.load(Ordering::Relaxed))
            .sum()
    };
    assert_eq!(
        submitted(),
        0,
        "the boot itself never submits through the write channels"
    );

    // N OLTP statements over both workers: inserts (the committing path),
    // point selects, a transaction — everything the query path does. Not
    // one of them may touch a write channel.
    let mut a = client(&fx);
    let mut b = client(&fx);
    for i in 0..25 {
        a.simple_query(&format!("INSERT INTO debug_table VALUES ({i}, 'oltp-{i}')"))
            .expect("insert");
        b.simple_query(&format!("SELECT val FROM debug_table WHERE id = {i}"))
            .expect("point select");
    }
    a.simple_query("BEGIN").expect("begin");
    a.simple_query("INSERT INTO debug_table VALUES (100, 'tx')")
        .expect("tx insert");
    a.simple_query("COMMIT").expect("commit");
    assert_eq!(
        submitted(),
        0,
        "50 OLTP statements + a transaction, and a write channel moved — \
         the query path called submit_to (FR-36 violation)"
    );

    // The counter sees what it claims: one DDL, one submit.
    a.simple_query("CREATE ROLE probe").expect("the DDL");
    assert_eq!(
        submitted(),
        1,
        "exactly one submit_to crossed for one CREATE ROLE"
    );
    assert_eq!(role_rows(&mut b), vec!["probe".to_owned()]);
}
