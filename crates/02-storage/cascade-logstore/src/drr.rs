//! The deficit-round-robin flush planner (§5.4) — pure and unit-tested,
//! because this is where "no noisy neighbors on the commit path" (§1.1)
//! lives or dies. Each scope with pending bytes grows its deficit by the
//! quantum per flush and contributes up to its counter, cut only at
//! group ends (S16 — every extent holds whole mini-transactions); a
//! mini-transaction larger than the counter is held until it is allowed,
//! then sent whole. Within one flush, scopes with waiting commits are
//! served before scopes that only have streamed bytes; the frame cap
//! ends the plan. A quiet scope's commit waits for at most one flush,
//! and a heavy scope's large writes fall on that scope alone.

use std::collections::VecDeque;

use cascade_types::{Epoch, Lsn, ScopeId};

/// One whole mini-transaction of pending bytes (S16: the planner never
/// splits one — the flusher's queue is cut only at group ends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGroup {
    pub bytes: Vec<u8>,
    /// The LSN one past the group's last byte — the durable-LSN boundary
    /// this group carries when its frame is synced.
    pub end_lsn: Lsn,
}

/// A scope's pending queue and its DRR state (§5.3: an idle scope holds
/// only this small record — no task, no timer).
#[derive(Debug)]
pub struct ScopeQueue {
    pub scope: ScopeId,
    pub epoch: Epoch,
    pub groups: VecDeque<PendingGroup>,
    /// Consumable deficit in bytes (§5.4: grows by the quantum per flush).
    pub deficit: usize,
    /// A commit is waiting on this scope's durability — it is served
    /// before streamed-only scopes (§5.4 item 3).
    pub has_waiter: bool,
}

/// Plans one flush across `queues`, consuming their deficits and their
/// pending groups. Returns (scope, groups taken) in service order.
/// The plan's total never exceeds `frame_cap` (default 1 MiB, §5.4).
pub fn plan_flush(
    queues: &mut [ScopeQueue],
    quantum: usize,
    frame_cap: usize,
) -> Vec<(ScopeId, Vec<PendingGroup>)> {
    // Round 0: every scope with pending bytes earns its quantum (§5.4).
    for q in queues.iter_mut() {
        if !q.groups.is_empty() {
            q.deficit = q.deficit.saturating_add(quantum);
        }
    }
    // Service order: waiters first, then the rest in queue order — the
    // round-robin comes from repeated calls draining deficits round by
    // round, so a heavy scope cannot starve a quiet one.
    let mut order: Vec<usize> = (0..queues.len()).collect();
    order.sort_by_key(|&i| !queues[i].has_waiter);

    let mut plan: Vec<(ScopeId, Vec<PendingGroup>)> = Vec::new();
    let mut used = 0usize;
    loop {
        let mut progressed = false;
        for &i in &order {
            let q = &mut queues[i];
            let Some(next) = q.groups.front() else {
                continue;
            };
            let len = next.bytes.len();
            // Whole groups only (S16); an oversized group waits for its
            // deficit to grow across flushes, then goes whole (§5.4 item 3).
            if len > q.deficit {
                continue;
            }
            if used + len > frame_cap {
                // The frame cap ends the plan (§5.4 item 5).
                return plan;
            }
            let group = q.groups.pop_front().expect("front checked");
            q.deficit -= len;
            used += len;
            match plan.iter_mut().find(|(s, _)| *s == q.scope) {
                Some((_, groups)) => groups.push(group),
                None => plan.push((q.scope, vec![group])),
            }
            progressed = true;
        }
        if !progressed {
            return plan;
        }
    }
}
