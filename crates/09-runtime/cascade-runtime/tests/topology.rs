//! T1.2: `/sys` topology parsing and FR-3 SMT rules.
//!
//! Fixture trees under `fixtures/sys-topology/` mirror the sysfs layout so
//! CI never depends on the host's real CPU shape.

use std::path::Path;

use cascade_runtime::{
    config::{NumaPool, NumaWorkers, SmtMode},
    pin,
    topology::{parse_cpu_list, SysTopology, SyntheticTopology, TopologyError, TopologyProvider},
    BootError, Runtime, RuntimeConfig,
};

fn fixture_root(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sys-topology")
        .join(name)
}

fn fixture(name: &str) -> SysTopology {
    SysTopology::from_root(&fixture_root(name)).expect("fixture parses")
}

#[test]
fn single_core_layout() {
    let topo = fixture("single-core");
    assert_eq!(topo.online_cpus().unwrap(), vec![0]);
    assert_eq!(topo.thread_siblings(0), vec![0]);
    assert_eq!(topo.numa_node(0), Some(0));
}

#[test]
fn smt_pair_layout() {
    let topo = fixture("smt-pairs");
    assert_eq!(topo.online_cpus().unwrap(), vec![0, 1, 2, 3]);
    assert_eq!(topo.thread_siblings(0), vec![0, 2], "0 and 2 share a physical core");
    assert_eq!(topo.thread_siblings(3), vec![1, 3]);
    assert_eq!(topo.numa_node(2), Some(0));
}

#[test]
fn two_numa_layout() {
    let topo = fixture("two-numa");
    assert_eq!(topo.numa_node(0), Some(0));
    assert_eq!(topo.numa_node(1), Some(0));
    assert_eq!(topo.numa_node(2), Some(1));
    assert_eq!(topo.numa_node(3), Some(1));
}

#[test]
fn non_contiguous_layout() {
    let topo = fixture("non-contiguous");
    assert_eq!(topo.online_cpus().unwrap(), vec![0, 2, 4]);
}

#[test]
fn malformed_sibling_list_is_an_error() {
    match SysTopology::from_root(&fixture_root("malformed")) {
        Err(TopologyError::Parse { path, .. }) => {
            assert!(path.contains("thread_siblings_list"), "names the file: {path}");
        }
        other => panic!("expected TopologyError::Parse, got {other:?}"),
    }
}

#[test]
fn offline_cpus_are_not_reported() {
    let topo = fixture("offline");
    assert_eq!(
        topo.online_cpus().unwrap(),
        vec![0, 1],
        "cpu2 has dirs but is not in `online`"
    );
    // Unknown-but-offline cores report no siblings rather than invented ones.
    assert_eq!(topo.thread_siblings(2), Vec::<u32>::new());
}

#[test]
fn missing_node_tree_degrades_to_numa_unknown() {
    let topo = fixture("no-numa");
    assert_eq!(topo.online_cpus().unwrap(), vec![0, 1]);
    assert_eq!(topo.numa_node(0), None, "no node/ tree — NUMA unknown, not guessed");
}

#[test]
fn missing_sys_root_is_an_error() {
    match SysTopology::from_root(Path::new("/nonexistent-sys-root")) {
        Err(TopologyError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected Io(NotFound), got {other:?}"),
    }
}

#[test]
fn cpu_list_parser() {
    assert_eq!(parse_cpu_list("0-3,8,10-11").unwrap(), vec![0, 1, 2, 3, 8, 10, 11]);
    assert_eq!(parse_cpu_list("7").unwrap(), vec![7]);
    assert_eq!(parse_cpu_list("").unwrap(), Vec::<u32>::new());
    assert!(parse_cpu_list("x").is_err());
    assert!(parse_cpu_list("5-2").is_err(), "inverted range");
    assert!(parse_cpu_list("1-").is_err());
    assert!(parse_cpu_list("0-100000000").is_err(), "implausible range");
}

/// FR-3: physical mode refuses any config assigning two SMT siblings to any
/// role; the same config boots in logical mode.
#[test]
fn physical_mode_refuses_sibling_assignment() {
    let allowed = pin::current_affinity();
    assert!(allowed.len() >= 3, "needs at least 3 CPUs");
    let (hk, wa, wb) = (allowed[0], allowed[1], allowed[2]);

    let topo = SyntheticTopology::new([hk, wa, wb]).with_siblings(wa, wb);
    let cfg = RuntimeConfig {
        housekeeper_cores: vec![hk],
        worker_cores: vec![NumaWorkers { numa: 0, cores: vec![wa, wb] }],
        numa_pools: vec![NumaPool { numa: 0, size_mb: 1 }],
        ..RuntimeConfig::default() // mode = physical
    };

    match Runtime::boot(&cfg, &topo).expect_err("siblings must refuse in physical mode") {
        BootError::SmtSiblingsAssigned { a, b } => {
            assert!(a == wa && b == wb, "names the sibling pair: {a},{b}");
        }
        other => panic!("expected SmtSiblingsAssigned, got {other:?}"),
    }

    // Same topology, logical mode: boots fine.
    let mut logical = cfg.clone();
    logical.topology.mode = SmtMode::Logical;
    Runtime::boot(&logical, &topo).expect("logical mode allows siblings");
}
