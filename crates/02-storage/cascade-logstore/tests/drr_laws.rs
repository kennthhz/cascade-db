//! M4 slice 2 red: the deficit-round-robin flush planner (§5.4) as pure
//! law. Each scope with pending bytes grows a deficit counter by the
//! quantum per flush and contributes up to its counter — cut only at
//! group ends (S16: every extent holds whole mini-transactions); a
//! mini-transaction larger than the counter is sent whole once the
//! counter allows it. Within one flush, scopes with waiting commits are
//! served before scopes that only have streamed bytes. The frame cap
//! stops the plan.

use std::collections::VecDeque;

use cascade_logstore::drr::{plan_flush, PendingGroup, ScopeQueue};
use cascade_types::{Epoch, Lsn, ScopeId};

fn group(bytes: &[u8], end_lsn: u64) -> PendingGroup {
    PendingGroup {
        bytes: bytes.to_vec(),
        end_lsn: Lsn(end_lsn),
    }
}

fn queue(scope: u64, deficit: usize, waiter: bool, groups: Vec<PendingGroup>) -> ScopeQueue {
    ScopeQueue {
        scope: ScopeId(scope),
        epoch: Epoch(1),
        groups: VecDeque::from(groups),
        deficit,
        has_waiter: waiter,
    }
}

/// The quantum is an upper bound per scope per flush, but a whole group
/// is never split: the cut lands on a group end (S16).
#[test]
fn quantum_cuts_at_group_ends_only() {
    let mut queues = vec![queue(
        1,
        0,
        true,
        vec![
            group(&[0; 60_000], 60_000),
            group(&[0; 60_000], 120_000),
            group(&[0; 60_000], 180_000),
        ],
    )];
    let plan = plan_flush(&mut queues, 64 * 1024, 1024 * 1024);
    // Deficit is 64 KiB: only the first 60 KiB group fits whole.
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].1.len(), 1, "one whole group per the quantum");
    assert_eq!(queues[0].groups.len(), 2, "the rest stays queued");
    assert_eq!(queues[0].deficit, 64 * 1024 - 60_000, "the deficit pays for what was sent");
}

/// A group larger than the quantum is held until the counter allows it —
/// and then sent whole (180 KiB needs three quanta).
#[test]
fn an_oversized_group_sends_whole_once_allowed() {
    let big = vec![group(&[0; 180_000], 180_000)];
    let mut queues = vec![queue(1, 0, true, big)];
    let plan = plan_flush(&mut queues, 64 * 1024, 1024 * 1024);
    assert!(plan.is_empty(), "64 KiB cannot carry a 180 KiB group");
    assert_eq!(queues[0].groups.len(), 1);
    let plan = plan_flush(&mut queues, 64 * 1024, 1024 * 1024);
    assert!(plan.is_empty(), "128 KiB still cannot");
    let plan = plan_flush(&mut queues, 64 * 1024, 1024 * 1024);
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].1[0].bytes.len(), 180_000, "sent whole, never split");
}

/// The frame cap stops the plan: nothing contributes past 1 MiB.
#[test]
fn the_frame_cap_stops_the_plan() {
    let mut queues = vec![
        queue(1, 1024 * 1024, true, vec![group(&[0; 600_000], 600_000)]),
        queue(2, 1024 * 1024, true, vec![group(&[0; 600_000], 600_000)]),
    ];
    let plan = plan_flush(&mut queues, 64 * 1024, 1024 * 1024);
    let total: usize = plan.iter().map(|(_, g)| g.iter().map(|p| p.bytes.len()).sum::<usize>()).sum();
    assert!(
        total <= 1024 * 1024,
        "the frame cap bounds one flush: {total}"
    );
}

/// Waiting commits are served before streamed-only scopes (§5.4 item 3).
#[test]
fn waiters_are_served_first() {
    let mut queues = vec![
        queue(1, 64 * 1024, false, vec![group(&[0; 64 * 1024], 64 * 1024)]),
        queue(2, 0, true, vec![group(&[0; 64 * 1024], 64 * 1024)]),
    ];
    let plan = plan_flush(&mut queues, 64 * 1024, 1024 * 1024);
    // The waiter (scope 2, deficit 0→quantum) is served; scope 1's
    // queued bytes wait for a later frame when the cap allows.
    assert_eq!(plan[0].0, ScopeId(2), "the waiting commit goes first");
}

/// Fair sharing: two heavy scopes alternate their consumption (DRR) —
/// one quantum each per flush, so neither drains the other's queue
/// before the other's first group.
#[test]
fn heavy_scopes_alternate() {
    let mut queues = vec![
        queue(1, 0, false, vec![group(&[0; 64 * 1024], 1), group(&[0; 64 * 1024], 2), group(&[0; 64 * 1024], 3)]),
        queue(2, 0, false, vec![group(&[0; 64 * 1024], 1), group(&[0; 64 * 1024], 2), group(&[0; 64 * 1024], 3)]),
    ];
    let plan = plan_flush(&mut queues, 64 * 1024, 1024 * 1024);
    let sent: Vec<u64> = plan.iter().map(|(s, _)| s.0).collect();
    assert!(sent.contains(&1) && sent.contains(&2), "both scopes share one frame: {sent:?}");
    // One quantum each: one group gone, two still queued on both.
    assert_eq!(queues[0].groups.len(), 2);
    assert_eq!(queues[1].groups.len(), 2);
}
