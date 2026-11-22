//! HLC clock tests (WP2 T2.1, docs/storage-wp2-txn-manager.md D3).
//!
//! All precision runs on injected physical sources — no wall-clock timing
//! in an assertion. The wall-clock test at the end only checks loose
//! contract bounds.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use cascade_runtime::txnmgr::HlcClock;
use cascade_types::{Hlc, VersionWord, HLC_EPOCH_UNIX_MS};

fn frozen(ms: u64) -> HlcClock {
    HlcClock::with_source(Box::new(move || ms))
}

fn stepped(first: u64) -> (HlcClock, Arc<AtomicU64>) {
    let cur = Arc::new(AtomicU64::new(first));
    let src = Arc::clone(&cur);
    let clock = HlcClock::with_source(Box::new(move || src.load(Ordering::SeqCst)));
    (clock, cur)
}

/// Under a frozen wall, every issue is a pure logical +1: same ms, logical
/// walks 1, 2, 3, … — and never 0 (VersionWord::EMPTY).
#[test]
fn logical_walks_under_frozen_wall() {
    let clock = frozen(5_000);
    let a = clock.issue();
    let b = clock.issue();
    let c = clock.issue();
    assert_eq!((a.physical_ms(), a.logical()), (5_000, 1));
    assert_eq!((b.physical_ms(), b.logical()), (5_000, 2));
    assert_eq!((c.physical_ms(), c.logical()), (5_000, 3));
    assert_ne!(a, Hlc(0));
}

/// The HLD rule `ts = max(prev + 1, now)`: when the physical source moves
/// past the last issued millisecond, the ts snaps forward and the logical
/// counter restarts.
#[test]
fn physical_snap_resets_logical() {
    let (clock, cur) = stepped(5_000);
    for expected_logical in [1u64, 2, 3] {
        let ts = clock.issue();
        assert_eq!((ts.physical_ms(), ts.logical()), (5_000, expected_logical));
    }
    cur.store(5_001, Ordering::SeqCst);
    let ts = clock.issue();
    assert_eq!((ts.physical_ms(), ts.logical()), (5_001, 0));
    // …and the logical walk resumes within the new ms.
    let ts = clock.issue();
    assert_eq!((ts.physical_ms(), ts.logical()), (5_001, 1));
}

/// `observe` folds a foreign ts forward: the next issue exceeds it.
/// A stale (older) ts is a no-op.
#[test]
fn observe_folds_forward_and_ignores_stale() {
    let clock = frozen(5_000);
    clock.observe(Hlc::from_parts(9_000, 7)).expect("legal ts");
    let ts = clock.issue();
    assert_eq!((ts.physical_ms(), ts.logical()), (9_000, 8));

    clock.observe(Hlc::from_parts(8_000, 5)).expect("legal ts");
    let ts = clock.issue();
    assert_eq!((ts.physical_ms(), ts.logical()), (9_000, 9));
}

/// `peek` is read-only: stable across calls, strictly below the next issue.
#[test]
fn peek_does_not_advance() {
    let clock = frozen(5_000);
    let p1 = clock.peek();
    let p2 = clock.peek();
    assert_eq!(p1, p2);
    assert!(clock.issue() > p1);
    assert!(clock.peek() > p1);
}

/// The recovery seed (WP2 D3): first issue is strictly beyond the floor.
/// A floor below the wall snaps to the wall; a floor above it pins the
/// clock until the wall catches up — either way `issue() > floor`.
#[test]
fn seed_is_respected() {
    // Floor above any wall this test will ever see: year ~2185 in HLC ms.
    let clock = HlcClock::new_seeded(Hlc::from_parts(5_000_000_000_000, 0));
    let ts = clock.issue();
    assert_eq!((ts.physical_ms(), ts.logical()), (5_000_000_000_000, 1));

    // Floor far below the wall: prev snaps to the wall floor at
    // construction, so issue lands in sane 2026+ territory.
    let clock = HlcClock::new_seeded(Hlc::from_parts(1_000, 0));
    let ts = clock.issue();
    assert!(ts.physical_ms() > 100_000_000, "wall floor not applied: {ts:?}");
}

/// Wall clock sanity — loose bounds only: inside the epoch contract, and
/// plausibly "2026 or later" (HLC ms ≥ 10^10 ≈ 116 days past the epoch).
#[test]
fn wall_clock_respects_contract() {
    let clock = HlcClock::new();
    let p = clock.peek().physical_ms();
    assert!(p <= Hlc::PHYSICAL_MAX_MS);
    assert!(p > 10_000_000_000, "wall HLC ms implausibly small: {p}");
    // The epoch itself is a compile-time constant of the contract — pin it
    // at const-eval time, not test time.
    const _: () = assert!(HLC_EPOCH_UNIX_MS > 1_000_000_000_000);
}

/// The F1 boundary: any ts the clock can ever issue stamps a committed
/// version word without tripping `VersionWord::committed`'s bit-63 guard.
#[test]
fn issued_ts_stamps_version_words() {
    let clock = frozen(5_000);
    let ts = clock.issue();
    let _ = VersionWord::committed(ts); // panics if the contract breaks
}

/// The epoch contract's loud refusals: a physical source already past
/// PHYSICAL_MAX_MS dies at construction; a legal source at the boundary
/// dies when the logical carry would cross it.
#[test]
#[should_panic(expected = "past PHYSICAL_MAX_MS")]
fn construction_refuses_overflowing_source() {
    let _ = HlcClock::with_source(Box::new(|| 1 << 47));
}

#[test]
#[should_panic(expected = "carry past PHYSICAL_MAX_MS")]
fn issue_refuses_logical_carry_past_the_boundary() {
    // Physical part at the maximum; walking the logical counter to full
    // (65536 issues) then issuing once more must carry — and refuse.
    let clock = frozen(Hlc::PHYSICAL_MAX_MS);
    for _ in 0..=u16::MAX as u64 {
        clock.issue();
    }
}

/// `observe` takes remote input: a ts outside the contract is an Err, not
/// a panic — the caller surfaces the protocol error.
#[test]
fn observe_refuses_overflowing_ts() {
    let clock = frozen(5_000);
    assert_eq!(
        clock.observe(Hlc((1 << 47) << 16)),
        Err(cascade_runtime::txnmgr::HlcOverflow)
    );
    // The clock did not move.
    assert_eq!(clock.peek().physical_ms(), 5_000);
}

/// The packing boundary: the exact maximum is legal in both parts (bit 63
/// stays clear); one ms past it is the constructor guard's panic.
#[test]
fn from_parts_takes_the_exact_boundary() {
    let ts = Hlc::from_parts(Hlc::PHYSICAL_MAX_MS, u16::MAX);
    assert_eq!(
        (ts.physical_ms(), ts.logical()),
        (Hlc::PHYSICAL_MAX_MS, u16::MAX as u64)
    );
    assert_eq!(ts, Hlc(((1 << 47) - 1) << 16 | u16::MAX as u64));
}

#[test]
#[should_panic(expected = "past PHYSICAL_MAX_MS")]
fn from_parts_refuses_one_ms_past_the_boundary() {
    let _ = Hlc::from_parts(1 << 47, 0);
}
