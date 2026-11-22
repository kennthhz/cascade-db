//! The glommio implementation of the minimal [`Rt`] contract
//! (storage-layer-design.md §9, step M0): spawn on the calling core's
//! executor, read the monotonic clock, park until a deadline. Phase A grows
//! this into the full runtime adapter; the storage components see only the
//! [`Rt`] trait object.
//!
//! `GlommioRt` is `Send + Sync` — cores share one instance — and every
//! method acts on the executor of the core that called it, which is exactly
//! what glommio's thread-per-core executor model provides.

use std::time::{Duration, Instant};

use cascade_rt_api::Rt;
use cascade_types::LocalBoxFuture;

/// The `Rt` for glommio executors.
#[derive(Debug, Default, Clone, Copy)]
pub struct GlommioRt;

impl Rt for GlommioRt {
    fn spawn_local(&self, fut: LocalBoxFuture<'static, ()>) {
        // glommio's `spawn_local` requires the calling thread to run an
        // executor and accepts !Send 'static futures — precisely the
        // contract's promise. Detached: callers coordinate through shared
        // state, not through handles (the trait doc). This is the same
        // primitive every background loop in this crate uses (boot.rs,
        // submit.rs, coop.rs).
        let _task = glommio::spawn_local(fut).detach();
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, dur: Duration) -> LocalBoxFuture<'static, ()> {
        Box::pin(glommio::timer::sleep(dur))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn spawned_tasks_run_on_the_calling_executor() {
        glommio::LocalExecutor::default().run(async {
            let rt = GlommioRt;
            let hits = Arc::new(AtomicUsize::new(0));
            let task_hits = Arc::clone(&hits);
            rt.spawn_local(Box::pin(async move {
                task_hits.fetch_add(1, Ordering::Release);
            }));
            // Two short sleeps: the spawned task gets its turns while this
            // task is parked. Ordering, not timing — no wall-clock asserts.
            rt.sleep(Duration::from_millis(1)).await;
            rt.sleep(Duration::from_millis(1)).await;
            assert_eq!(
                hits.load(Ordering::Acquire),
                1,
                "spawned task ran on this executor before the second sleep resolved"
            );
        });
    }
}
