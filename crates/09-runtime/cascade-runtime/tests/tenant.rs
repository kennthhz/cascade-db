//! T1.6: TenantContext + Span plumbing (FR-20, FR-21).
//!
//! The compile-shape assertion is the signatures themselves: every hot-path
//! function takes `&TenantContext` and `&Span`, so a call without context
//! does not type-check. What a *runtime* test can add is propagation
//! identity: the exact context constructed at session start is what the
//! hot path records — not just "some context".

use std::time::{Duration, Instant};

use cascade_runtime::{
    config::{AuthConfig, NumaPool, NumaWorkers},
    pin,
    tenant::dev_context,
    AcceptLoop, Runtime, RuntimeConfig, SyntheticTopology,
};
use cascade_types::{DatabaseId, TenantId};

fn wait_until(pred: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn hot_path_records_the_exact_session_context() {
    let mut allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    allowed.truncate(3);

    let topo = SyntheticTopology::new(allowed.iter().copied());
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![allowed[0]],
        worker_cores: vec![NumaWorkers { numa: 0, cores: vec![allowed[1], allowed[2]] }],
        numa_pools: vec![NumaPool { numa: 0, size_mb: 1 }],
        ..RuntimeConfig::default()
    };
    let listener = AcceptLoop::bind("127.0.0.1", 0).unwrap();
    let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let rt = Runtime::boot_routed(&cfg, &topo, Some(listener), AuthConfig::default()).unwrap();

    // Identity, not just non-null: the session constructs `dev_context()`;
    // the hot path must record queries under exactly that (db, tenant) key —
    // and under no other.
    let expected = dev_context();
    let other = cascade_tenant::TenantContext {
        db: DatabaseId(999),
        tenant: Some(TenantId(7)),
    };

    let mut sock = std::net::TcpStream::connect(&addr).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for field in ["user", "alice", "database", "dev", ""] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    use std::io::{Read, Write};
    sock.write_all(&(body.len() as i32 + 4).to_be_bytes()).unwrap();
    sock.write_all(&body).unwrap();
    // Through ReadyForQuery (housekeeper handshake).
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

    // Two query chunks over the session.
    for chunk in [b"q1".as_slice(), b"q2".as_slice()] {
        sock.write_all(chunk).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = vec![0u8; chunk.len() + 64];
        let _ = sock.read(&mut buf).unwrap(); // echo; content irrelevant here
    }
    drop(sock);

    wait_until(
        || {
            rt.workers()
                .iter()
                .map(|w| w.tenant_counters().query_count(&expected))
                .sum::<u64>()
                == 2
        },
        "both queries recorded under the dev context",
    );
    assert_eq!(
        rt.workers()
            .iter()
            .map(|w| w.tenant_counters().query_count(&other))
            .sum::<u64>(),
        0,
        "no query may be recorded under a different tenant key"
    );
}

#[test]
fn counters_tag_per_tenant_pair() {
    use cascade_runtime::tenant::TenantCounters;
    use cascade_telemetry::Span;

    let counters = TenantCounters::new();
    let dev = dev_context();
    let paid = cascade_tenant::TenantContext {
        db: DatabaseId(42),
        tenant: Some(TenantId(1)),
    };
    let span = Span::noop();

    cascade_runtime::tenant::debug_hot_path(&dev, &span, &counters);
    cascade_runtime::tenant::debug_hot_path(&paid, &span, &counters);
    cascade_runtime::tenant::debug_hot_path(&paid, &span, &counters);

    assert_eq!(counters.query_count(&dev), 1);
    assert_eq!(counters.query_count(&paid), 2);
    assert_eq!(
        counters.query_count(&cascade_tenant::TenantContext {
            db: DatabaseId(42),
            tenant: Some(TenantId(2)),
        }),
        0,
        "same db, different tenant: separate bucket"
    );
}
