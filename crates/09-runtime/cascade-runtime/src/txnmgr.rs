//! The txn manager's clock: the hybrid logical clock (HLD v2 §6, WP2 D3 —
//! docs/storage-wp2-txn-manager.md).
//!
//! Status-table, commit-section, and closed-ts machinery (WP2 D1/D2/D4)
//! land in this module with T2.2–T2.4; the clock moves first because the
//! commit path's T2.3 migration needs it and it has no runtime
//! dependencies.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cascade_types::{Hlc, HLC_EPOCH_UNIX_MS};

/// The HLC epoch-contract violation: a physical component past 2^47 ms
/// since [`HLC_EPOCH_UNIX_MS`] would pack into a word with bit 63 set,
/// colliding with the version word's uncommitted tag (WP1 F1). The custom
/// epoch keeps real wall clocks ~4400 years away from this; hitting it
/// means a clock ignored the contract. Raised by [`HlcClock::observe`] —
/// the one entry point that takes timestamps from outside the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("HLC physical component past 2^47 ms since the HLC epoch — would set version-word bit 63 (WP1 F1 epoch contract)")]
pub struct HlcOverflow;

/// Where an [`HlcClock`] reads physical time from.
enum PhysicalSource {
    /// The wall clock, as ms since [`HLC_EPOCH_UNIX_MS`].
    Wall,
    /// An injected ms source — deterministic tests now, replica/remote
    /// clocks later. `Send + Sync` so the clock can be `Arc`-shared across
    /// reactors like every other per-DB structure.
    Injected(Box<dyn Fn() -> u64 + Send + Sync>),
}

/// The hybrid logical clock (HLD v2 §6, WP2 D3): the source of every
/// `commit_ts`. `issue()` returns `max(prev_issued + 1, pack(now))` —
/// strictly monotonic within the process, never 0 (which is
/// [`cascade_types::VersionWord::EMPTY`]), with the 16-bit logical counter
/// walking within one physical millisecond and carrying into the next.
///
/// Shared as `&self` across reactors (interior mutability via atomic CAS —
/// a lost race only makes the winner's ts slightly larger, never smaller).
/// Monotonic across restarts comes from the recovery seed
/// ([`HlcClock::new_seeded`], WP2 D3), not from this type.
///
/// Lives in the runtime (not cascade-types) — the types crate is
/// data-only (its hard invariant 1); a clock is stateful logic. The packed
/// encoding and its guards stay in `cascade_types::Hlc`.
pub struct HlcClock {
    prev: AtomicU64,
    physical: PhysicalSource,
}

/// Wall-clock ms since the HLC epoch, refusing loudly anything that would
/// silently break the epoch contract: a clock before 1970 or before the
/// HLC epoch has no legal encoding (WP1 F1 counts ms from 2026-01-01Z).
fn wall_hlc_ms() -> u64 {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before the Unix epoch")
        .as_millis() as u64;
    match unix_ms.checked_sub(HLC_EPOCH_UNIX_MS) {
        Some(ms) => ms,
        None => panic!(
            "system clock predates the HLC epoch (2026-01-01Z): unix_ms {unix_ms} — \
             the custom-epoch contract (WP1 F1) requires every issued ts to count from it"
        ),
    }
}

/// Pack with the contract guard, panicking — constructor inputs violating
/// the epoch contract are programmer errors, the same class
/// [`cascade_types::VersionWord::committed`] panics on.
fn checked_pack(ms: u64) -> Hlc {
    Hlc::from_parts(ms, 0)
}

impl Default for HlcClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HlcClock {
    /// A wall-clock HLC starting at the current millisecond.
    pub fn new() -> Self {
        Self::with_source(Box::new(wall_hlc_ms))
    }

    /// A wall-clock HLC whose first `issue()` is strictly beyond `floor` —
    /// the recovery seed (WP2 D3): `floor` is the max over replayed
    /// COMMIT commit_ts values and HLC_HIGH_WATER `max_issued` records.
    /// The wall-derived floor applies too — the seed is the max of the
    /// two, so a slow wall can never reissue replayed history.
    pub fn new_seeded(floor: Hlc) -> Self {
        assert!(
            floor.physical_ms() <= Hlc::PHYSICAL_MAX_MS,
            "recovery seed {} outside the epoch contract (physical_ms {} > {})",
            floor.0,
            floor.physical_ms(),
            Hlc::PHYSICAL_MAX_MS
        );
        let prev = floor.0.max(checked_pack(wall_hlc_ms()).0);
        HlcClock {
            prev: AtomicU64::new(prev),
            physical: PhysicalSource::Wall,
        }
    }

    /// A clock reading physical time from `source` (ms since the HLC
    /// epoch) — deterministic tests, and later replica/remote clocks. The
    /// starting point is one reading, so an injected source already past
    /// [`Hlc::PHYSICAL_MAX_MS`] is refused here, loudly, not at first
    /// issue.
    pub fn with_source(source: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        let start = source();
        HlcClock {
            prev: AtomicU64::new(checked_pack(start).0),
            physical: PhysicalSource::Injected(source),
        }
    }

    /// The next commit_ts (HLD v2 §6: `ts = max(prev + 1, now)`).
    /// Panics on the epoch-contract overflow — the carry past
    /// [`Hlc::PHYSICAL_MAX_MS`] (physical max ms with a full logical
    /// counter) cannot be fixed here; a clock at year ~6488 must stop, not
    /// corrupt version words.
    pub fn issue(&self) -> Hlc {
        let now = match &self.physical {
            PhysicalSource::Wall => wall_hlc_ms(),
            PhysicalSource::Injected(f) => f(),
        };
        assert!(
            now <= Hlc::PHYSICAL_MAX_MS,
            "HLC physical source reports {now} ms — past PHYSICAL_MAX_MS ({})",
            Hlc::PHYSICAL_MAX_MS
        );
        let packed_now = now << Hlc::LOGICAL_BITS;
        let mut prev = self.prev.load(Ordering::Acquire);
        loop {
            let next = if packed_now > prev { packed_now } else { prev + 1 };
            assert!(
                next >> Hlc::LOGICAL_BITS <= Hlc::PHYSICAL_MAX_MS,
                "HLC logical carry past PHYSICAL_MAX_MS — the epoch contract \
                 (WP1 F1) forbids issuing further timestamps"
            );
            match self
                .prev
                .compare_exchange(prev, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Hlc(next),
                Err(actual) => prev = actual,
            }
        }
    }

    /// The highest issued ts, without advancing the clock. The closed-ts
    /// heartbeat's floor (WP2 D4): every future `issue()` returns strictly
    /// more than this, so promising "no future commit_ts ≤ peek()" leans
    /// only on the records that issued ≤ this being durable.
    pub fn peek(&self) -> Hlc {
        Hlc(self.prev.load(Ordering::Acquire))
    }

    /// Folds a foreign timestamp in (HLD v2 §6 `hlc.observe`): later
    /// `issue()` calls return strictly more than `ts`. A stale ts is a
    /// no-op. Remote input, so refusal is a [`Result`], not a panic — the
    /// caller surfaces the protocol error.
    pub fn observe(&self, ts: Hlc) -> Result<(), HlcOverflow> {
        if ts.physical_ms() > Hlc::PHYSICAL_MAX_MS {
            return Err(HlcOverflow);
        }
        let mut prev = self.prev.load(Ordering::Acquire);
        while prev < ts.0 {
            match self
                .prev
                .compare_exchange(prev, ts.0, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(actual) => prev = actual,
            }
        }
        Ok(())
    }
}
