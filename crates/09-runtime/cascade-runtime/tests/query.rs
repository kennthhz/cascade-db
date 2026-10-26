//! T1.7: Tier A session + simple-query loop (FR-14, FR-16), driven end to
//! end through the real `postgres` client: startup → query → vectorized
//! result batches → ReadyForQuery, over the debug SQL subset (plan A1).
//!
//! Table state is per worker reactor (slice scope), so every test inserts
//! and selects over the *same* connection — sessions are pinned, which is
//! exactly the FR-14 guarantee.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin, AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};

struct Fixture {
    url: String,
    addr: String,
    _rt: Runtime,
}

fn boot(batch_rows: u32) -> Fixture {
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
        batch_rows,
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default())
        .expect("boot routed");
    Fixture {
        url: format!("postgres://alice:pw@127.0.0.1:{port}/dev"),
        addr: format!("127.0.0.1:{port}"),
        _rt: rt,
    }
}

fn client(fx: &Fixture) -> postgres::Client {
    postgres::Client::connect(&fx.url, postgres::NoTls).expect("client connects")
}

fn sqlstate(err: postgres::Error) -> String {
    err.as_db_error()
        .map(|e| e.code().code().to_owned())
        .unwrap_or_else(|| format!("not a db error: {err}"))
}

/// Collect (row column values) from a simple query's result.
fn rows_of(msgs: &[postgres::SimpleQueryMessage]) -> Vec<Vec<Option<String>>> {
    msgs.iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => {
                Some((0..r.len()).map(|i| r.get(i).map(str::to_owned)).collect())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn insert_then_select_roundtrips_with_projection() {
    let fx = boot(1024);
    let mut c = client(&fx);

    c.simple_query("INSERT INTO debug_table VALUES (1, 'one')")
        .expect("insert");
    c.simple_query("INSERT INTO debug_table VALUES (2, 'two')")
        .expect("insert");

    let all = rows_of(
        &c.simple_query("SELECT * FROM debug_table")
            .expect("select *"),
    );
    assert_eq!(
        all,
        vec![
            vec![Some("1".into()), Some("one".into())],
            vec![Some("2".into()), Some("two".into())]
        ]
    );

    let proj = rows_of(
        &c.simple_query("SELECT val FROM debug_table")
            .expect("projection"),
    );
    assert_eq!(
        proj,
        vec![vec![Some("one".into())], vec![Some("two".into())]]
    );

    // CommandComplete counts: "SELECT 2" → 2 rows reported.
    let tags: Vec<u64> = c
        .simple_query("SELECT id FROM debug_table")
        .expect("select")
        .into_iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::CommandComplete(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(tags, vec![2], "CommandComplete reports the row count");

    // The demo's query shape through the wire: the point filter parsed by
    // libpg_query, resolved against the fixed schema, applied in the
    // engine. Either written order must hit the same row.
    let point = rows_of(
        &c.simple_query("SELECT id, val FROM debug_table WHERE id = 2")
            .expect("point select"),
    );
    assert_eq!(point, vec![vec![Some("2".into()), Some("two".into())]]);
    let reversed = rows_of(
        &c.simple_query("SELECT val FROM debug_table WHERE 2 = id")
            .expect("reversed point select"),
    );
    assert_eq!(reversed, vec![vec![Some("two".into())]]);
    let miss = rows_of(
        &c.simple_query("SELECT val FROM debug_table WHERE id = 99")
            .expect("point miss"),
    );
    assert!(miss.is_empty(), "no match → no rows, got {miss:?}");
}

#[test]
fn transactions_buffer_until_commit_and_rollback_discards() {
    let fx = boot(1024);
    let mut c = client(&fx);

    c.simple_query("INSERT INTO debug_table VALUES (10, 'auto')")
        .expect("autocommit insert");

    c.simple_query("BEGIN").expect("begin");
    c.simple_query("INSERT INTO debug_table VALUES (11, 'pending')")
        .expect("txn insert");
    // Own uncommitted inserts are visible inside the transaction.
    let seen = rows_of(
        &c.simple_query("SELECT * FROM debug_table")
            .expect("in-txn select"),
    );
    assert_eq!(
        seen.len(),
        2,
        "autocommitted + pending rows visible to self"
    );

    c.simple_query("ROLLBACK").expect("rollback");
    let after = rows_of(
        &c.simple_query("SELECT * FROM debug_table")
            .expect("post-rollback"),
    );
    assert_eq!(after.len(), 1, "rolled-back insert is gone");
    assert_eq!(after[0][0].as_deref(), Some("10"));

    c.simple_query("BEGIN").expect("begin 2");
    c.simple_query("INSERT INTO debug_table VALUES (12, 'kept')")
        .expect("txn insert 2");
    c.simple_query("COMMIT").expect("commit");
    let kept = rows_of(
        &c.simple_query("SELECT * FROM debug_table")
            .expect("post-commit"),
    );
    assert_eq!(kept.len(), 2, "committed insert persisted");
    assert_eq!(kept[1][0].as_deref(), Some("12"));
}

#[test]
fn subset_errors_carry_pg_sqlstates_and_failed_txn_refuses() {
    let fx = boot(1024);
    let mut c = client(&fx);

    // DROP parses cleanly under the real grammar (libpg_query since the
    // demo); the engine refuses it as *unsupported*, not malformed —
    // 0A000, the code a PG extension would raise.
    assert_eq!(
        sqlstate(c.simple_query("DROP TABLE x").unwrap_err()),
        "0A000"
    );
    assert_eq!(
        sqlstate(c.simple_query("SELECT * FROM nope").unwrap_err()),
        "42P01"
    );
    assert_eq!(
        sqlstate(c.simple_query("SELECT nope FROM debug_table").unwrap_err()),
        "42703"
    );

    // Aborted transaction: statement fails → everything until ROLLBACK is
    // refused with 25P02, then the connection recovers.
    c.simple_query("BEGIN").expect("begin");
    assert_eq!(
        sqlstate(c.simple_query("SELECT * FROM nope").unwrap_err()),
        "42P01",
        "the error that aborts the txn keeps its own code"
    );
    assert_eq!(
        sqlstate(c.simple_query("SELECT * FROM debug_table").unwrap_err()),
        "25P02",
        "post-abort statements are refused"
    );
    assert_eq!(
        sqlstate(c.simple_query("BEGIN").unwrap_err()),
        "25P02",
        "nested begin refused while aborted"
    );
    c.simple_query("ROLLBACK")
        .expect("rollback clears the abort");
    c.simple_query("SELECT * FROM debug_table")
        .expect("connection recovered after rollback");

    // COMMIT with nothing open is a real error; COMMIT of an aborted txn
    // rolls back (PG semantics).
    assert_eq!(sqlstate(c.simple_query("COMMIT").unwrap_err()), "25P01");
    c.simple_query("BEGIN").expect("begin 2");
    c.simple_query("SELECT * FROM nope").expect_err("abort it");
    c.simple_query("COMMIT")
        .expect("commit of aborted txn acts as rollback");
    c.simple_query("SELECT * FROM debug_table")
        .expect("still usable");
}

#[test]
fn multi_statement_batch_executes_in_order_until_first_error() {
    let fx = boot(1024);
    let mut c = client(&fx);

    let msgs = c
        .simple_query(
            "INSERT INTO debug_table VALUES (1, 'a'); INSERT INTO debug_table VALUES (2, 'b'); \
             SELECT * FROM debug_table",
        )
        .expect("three-statement batch");
    let rows = rows_of(&msgs);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Some("1".into()), Some("a".into())]);
    assert_eq!(rows[1], vec![Some("2".into()), Some("b".into())]);
    // One CommandComplete per statement.
    let tags = msgs
        .iter()
        .filter(|m| matches!(m, postgres::SimpleQueryMessage::CommandComplete(_)))
        .count();
    assert_eq!(tags, 3);

    // Error in the middle (autocommit): statements before the error are
    // applied per-statement; the rest of the batch is skipped.
    let err = c
        .simple_query(
            "INSERT INTO debug_table VALUES (3, 'c'); SELECT * FROM nope; \
                       INSERT INTO debug_table VALUES (4, 'd')",
        )
        .expect_err("second statement fails");
    assert_eq!(sqlstate(err), "42P01");
    let rows = rows_of(&c.simple_query("SELECT * FROM debug_table").expect("select"));
    assert_eq!(
        rows.len(),
        3,
        "stmt 1 applied, stmts after the error skipped"
    );
    assert!(
        !rows.iter().any(|r| r[0].as_deref() == Some("4")),
        "skipped statement never ran"
    );

    // Inside a transaction the same failure aborts everything pending —
    // COMMIT acts as ROLLBACK and discards the pre-error statements too.
    c.simple_query("BEGIN").expect("begin");
    let err = c
        .simple_query("INSERT INTO debug_table VALUES (5, 'e'); SELECT * FROM nope")
        .expect_err("second statement fails");
    assert_eq!(sqlstate(err), "42P01");
    c.simple_query("COMMIT")
        .expect("commit of aborted txn rolls back");
    let rows = rows_of(&c.simple_query("SELECT * FROM debug_table").expect("select"));
    assert_eq!(
        rows.len(),
        3,
        "the in-txn insert was discarded with the abort"
    );
    assert!(!rows.iter().any(|r| r[0].as_deref() == Some("5")));
}

#[test]
fn debug_worker_names_the_reactor_thread() {
    let fx = boot(1024);
    let mut c = client(&fx);
    let rows = rows_of(
        &c.simple_query("SELECT debug_worker()")
            .expect("debug_worker"),
    );
    assert_eq!(rows.len(), 1);
    let name = rows[0][0].as_deref().expect("name");
    assert!(
        name.starts_with("cascade-w-"),
        "session runs on a worker reactor, got {name:?}"
    );
}

#[test]
fn batch_rows_chunking_is_transparent_to_the_client() {
    // batch_rows = 2 with 5 result rows: the session must frame DataRows in
    // vectorized writes at the configured boundary and the client must not
    // notice any chunking.
    let fx = boot(2);
    let mut c = client(&fx);
    for i in 0..5 {
        c.simple_query(&format!("INSERT INTO debug_table VALUES ({i}, 'v{i}')"))
            .expect("insert");
    }
    let rows = rows_of(&c.simple_query("SELECT * FROM debug_table").expect("select"));
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0].as_deref(), Some(i.to_string().as_str()));
        assert_eq!(row[1].as_deref(), Some(format!("v{i}").as_str()));
    }
}

/// Raw-socket probes for paths the `postgres` client can't easily produce:
/// the empty query and a protocol violation.
#[test]
fn empty_query_gets_empty_query_response_and_violation_closes() {
    let fx = boot(1024);
    let mut sock = TcpStream::connect(&fx.addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // Startup (trust mode) — reuse the raw handshake.
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for field in ["user", "alice", "database", "dev", ""] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    sock.write_all(&(body.len() as i32 + 4).to_be_bytes())
        .unwrap();
    sock.write_all(&body).unwrap();
    loop {
        let mut hdr = [0u8; 5];
        sock.read_exact(&mut hdr).unwrap();
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).unwrap();
        if hdr[0] == b'Z' {
            break;
        }
    }

    // Empty query → EmptyQueryResponse ('I') then ReadyForQuery.
    sock.write_all(&[b'Q', 0, 0, 0, 4]).unwrap();
    assert_eq!(read_msg(&mut sock).0, b'I', "EmptyQueryResponse");
    assert_eq!(
        read_msg(&mut sock).0,
        b'Z',
        "ReadyForQuery after empty query"
    );

    // Extended-protocol message ('P' Parse) — outside the slice: ErrorResponse
    // then the session closes rather than misread the desynced stream.
    sock.write_all(&[b'P', 0, 0, 0, 8, 0, 0, 0, 0]).unwrap();
    assert_eq!(
        read_msg(&mut sock).0,
        b'E',
        "protocol violation answered with ErrorResponse"
    );
    let mut eof = [0u8; 1];
    let got = sock.read(&mut eof).unwrap_or(0);
    assert_eq!(got, 0, "session closes after a framing violation");
}

/// A hostile 4 GB length header must get an ErrorResponse and a close —
/// never a worker-side 4 GB allocation (regression: read_message used to
/// allocate before validating).
#[test]
fn oversized_length_header_rejected_without_allocation() {
    let fx = boot(1024);
    let mut sock = TcpStream::connect(&fx.addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    raw_startup_trust(&mut sock);

    sock.write_all(&[b'Q', 0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
    let (t, payload) = read_msg(&mut sock);
    assert_eq!(t, b'E', "ErrorResponse for absurd length");
    assert!(String::from_utf8_lossy(&payload).contains("protocol violation"));
    let mut eof = [0u8; 1];
    assert_eq!(
        sock.read(&mut eof).unwrap_or(0),
        0,
        "session closed after bad length"
    );

    // And the worker is still alive: a fresh client works.
    let mut c = client(&fx);
    c.simple_query("SELECT debug_worker()")
        .expect("worker unaffected by the hostile client");
}

/// Full-message reader: header *and* payload — a header-only read leaves
/// the RFB body byte in the stream and desyncs everything after.
fn read_msg(sock: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 5];
    sock.read_exact(&mut hdr).unwrap();
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload).unwrap();
    (hdr[0], payload)
}

/// Startup handshake for raw probes (trust mode) — shared by this file's
/// raw tests.
fn raw_startup_trust(sock: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for field in ["user", "alice", "database", "dev", ""] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    sock.write_all(&(body.len() as i32 + 4).to_be_bytes())
        .unwrap();
    sock.write_all(&body).unwrap();
    loop {
        let mut hdr = [0u8; 5];
        sock.read_exact(&mut hdr).unwrap();
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize - 4;
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).unwrap();
        if hdr[0] == b'Z' {
            break;
        }
    }
}
