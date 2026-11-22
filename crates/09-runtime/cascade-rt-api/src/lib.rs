//! The minimal runtime contract (storage-layer-design.md §9, step M0): the
//! parts the storage components need before Phase A defines the full
//! runtime adapter.
//!
//! Three operations: spawn a task that stays on the calling core, read the
//! clock, park until a deadline. The runtime ([`cascade-runtime`]'s
//! `GlommioRt`) implements it; tests and components use fakes.
//!
//! Shape rules (S3): implementations are `Send + Sync` — every core shares
//! one object — while the futures it hands back are *not* `Send`: a task
//! runs on the core that spawned it, and a sleep is awaited where it was
//! created. Thread-per-core is the whole point; the type system says so.

use std::time::{Duration, Instant};

use cascade_types::LocalBoxFuture;

/// The runtime's face to the storage components (§5.4, §8): background work
/// — log flushers, page ingest — is spawned through this object, on cores
/// chosen at boot. No component spawns threads or opens files directly
/// (Rule 4); this trait is the only way in.
pub trait Rt: Send + Sync {
    /// Spawns a task on the calling core's executor. The future must be
    /// `'static` (the task outlives the call) and is deliberately *not*
    /// `Send` — it never leaves the core it was spawned on. Fire-and-forget:
    /// components coordinate through their own shared state, not through
    /// task handles.
    fn spawn_local(&self, fut: LocalBoxFuture<'static, ()>);

    /// The clock the components measure commit windows and lease ages with.
    /// Monotonic.
    fn now(&self) -> Instant;

    /// Completes after *at least* `dur`. Never spins: the implementation
    /// parks the task, so a quiet core idles.
    fn sleep(&self, dur: Duration) -> LocalBoxFuture<'static, ()>;
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use futures_lite::future::block_on;
    use static_assertions::{assert_impl_all, assert_not_impl_any, assert_obj_safe};

    use super::*;

    /// A test double: sleeps resolve immediately, spawned tasks run nowhere.
    /// The assertions below are about the contract's *shape*, so the fake
    /// can be empty.
    struct FakeRt;

    impl Rt for FakeRt {
        fn spawn_local(&self, _fut: LocalBoxFuture<'static, ()>) {}
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn sleep(&self, _dur: Duration) -> LocalBoxFuture<'static, ()> {
            Box::pin(async {})
        }
    }

    assert_obj_safe!(Rt);
    // S3: one instance shared by every core.
    assert_impl_all!(FakeRt: Send, Sync);
    // S3: contract futures never cross cores.
    assert_not_impl_any!(LocalBoxFuture<'static, ()>: Send);

    #[test]
    fn spawn_accepts_a_task_holding_core_local_memory() {
        // An `Rc` is !Send — a task may hold one only because `spawn_local`
        // promises the task never leaves the calling core. This test is the
        // compile itself.
        let seen = Rc::new(Cell::new(false));
        let task_seen = Rc::clone(&seen);
        FakeRt.spawn_local(Box::pin(async move {
            task_seen.set(true);
        }));
        assert!(!seen.get(), "the fake runs nothing; the proof is the compile");
    }

    #[test]
    fn sleep_resolves_without_a_real_clock() {
        let rt = FakeRt;
        // The fake's sleep is immediately ready, so component timing tests
        // need no executor and no wall-clock waiting.
        block_on(rt.sleep(Duration::from_secs(3600)));
    }

    #[test]
    fn now_reads_a_monotonic_clock() {
        let rt = FakeRt;
        let t0 = rt.now();
        assert!(rt.now() >= t0);
    }
}
