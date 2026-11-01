//! The Tier C catalog: the cluster-metadata snapshot and its pointer-load
//! read path (FR-28; spec §6.3, §8.4; T5.2).
//!
//! FR-28 is the whole contract: every reactor serves metadata from its
//! local `ArcSwap<ClusterSnapshot>`, and a read is `load()` — one pointer
//! copy, no lock, no compare-exchange (§6.3). §8.4 names `ArcSwap` the
//! chosen primitive because its three guarantees are exactly Tier C's
//! needs: reads are wait-free, writers never block readers, and a replaced
//! snapshot frees itself through `Arc`'s refcount the moment its last
//! reader finishes. [`Catalog`] wraps that `ArcSwap`, and the tests in
//! this module pin all three guarantees plus the reader/writer race
//! between them.
//!
//! The two read forms, and when each is legal:
//!
//! * [`Catalog::load`] returns an [`arc_swap::Guard`] — a pointer copy
//!   that does not even bump the refcount (a debt slot, confirmed into a
//!   real reference only on the rare slow path). Finish the read
//!   synchronously on the reading ring: a live `Guard` occupies one of
//!   the thread's debt slots (per-thread, shared across every ArcSwap
//!   instance), so long-lived guards push that thread's other `load()`s
//!   onto the slower refcount path — a `Guard` never spans an `.await`
//!   (that is what [`Catalog::load_full`] is for). This is the FR-28
//!   hot-path read.
//! * [`Catalog::load_full`] returns an owned `Arc` — one refcount
//!   increment — for the rare reader that must hold its snapshot across
//!   an await point.
//!
//! I9 gives the write side its shape: reads go through `load`/`load_full`
//! only, and the ONLY write is [`Catalog::store`], `pub(crate)` — no
//! external crate can write, and in-crate the housekeeper is the
//! designated writer (FR-29: one serialized writer, `submit_to` fan-out
//! with R11's bounded wait). That writer is T5.3's task; until it lands,
//! boot publishes once at construction ([`Catalog::new`]) and the
//! snapshot is immutable after boot — the T2.4 posture, unchanged. The
//! wrapper is deliberate beyond the visibility: `ArcSwapAny` also exposes
//! `swap`/`compare_exchange`/`rcu`-style writes, and a single-narrow
//! surface is what makes "no second writer path" (I9) reviewable by grep.
//!
//! Per-reactor instances are likewise T5.3's fan-out shape: R11's laggard
//! adoption requires each reactor to adopt a new snapshot on ITS next
//! pointer load, which needs one `ArcSwap` per reactor. Today the router
//! (plus boot's one resolve for the WAL home) is the only Tier C reader,
//! holding the boot's single instance.
//!
//! The snapshot payload is still the routing half (FR-10): per-DB rows
//! (`pg_database.datnumaffinity` / `datnumanode`) and roles arrive with
//! the real catalog work (T5.4/T5.5); today every database — named or
//! not — inherits the cluster default, which is exactly the inheritance
//! FR-10 requires and the T2.4 test-review's "no explicit affinity" case.
//! A `single` bound NUMA is picked per A13 (least-loaded NUMA at first
//! activation); at boot every NUMA is idle, so the standing proxy is
//! fewest workers, tie-broken by config order. (The payload carries
//! FR-28's literal type name, `ClusterSnapshot` — the T2.4 static
//! snapshot took the spec's name when it grew the ArcSwap read path.)

use std::sync::Arc;

use arc_swap::{ArcSwap, Guard};

use crate::config::NumaAffinity;

/// The placement a connection's database resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub affinity: NumaAffinity,
    /// For `single` affinity: the bound NUMA (A13 pick). `None` when the
    /// cluster has no workers at all — routing then has no candidates.
    pub bound_numa: Option<u32>,
}

/// The cluster metadata snapshot every reactor serves reads from — FR-28's
/// `ArcSwap<ClusterSnapshot>` payload. Immutable by construction: readers
/// never observe a half-updated snapshot, the writer (housekeeper, T5.3)
/// builds a fresh one and publishes it whole.
#[derive(Debug)]
pub struct ClusterSnapshot {
    default_affinity: NumaAffinity,
    /// A13 bound NUMA for default-`single` databases.
    default_bound: Option<u32>,
}

impl ClusterSnapshot {
    /// Builds the snapshot from the cluster default and the boot-time
    /// worker layout (numa → worker count, in config order). `single`
    /// binds per A13; `cross` binds nothing.
    pub fn build(default_affinity: NumaAffinity, worker_numas: &[u32]) -> Self {
        let default_bound = match default_affinity {
            NumaAffinity::Single => least_loaded_numa(worker_numas),
            NumaAffinity::Cross => None,
        };
        ClusterSnapshot {
            default_affinity,
            default_bound,
        }
    }

    /// FR-10 lookup: the placement for a connection's database name.
    /// With no catalog rows yet, every name inherits the cluster default.
    pub fn resolve(&self, _database: Option<&str>) -> Placement {
        Placement {
            affinity: self.default_affinity,
            bound_numa: self.default_bound,
        }
    }

    /// The cluster default the snapshot was built with (diagnostics).
    pub fn default_affinity(&self) -> NumaAffinity {
        self.default_affinity
    }
}

/// The per-reactor catalog holder — FR-28's `ArcSwap<ClusterSnapshot>`.
/// Shared as `Arc<Catalog>`; construction is cheap enough that T5.3 can
/// hand one to every reactor.
#[derive(Debug)]
pub struct Catalog {
    swap: ArcSwap<ClusterSnapshot>,
}

// The holder crosses the boot: the T5.3 writer runs on the housekeeper's
// path and readers live on every ring; the payload must survive a fan-out
// job (`Send`) and serve concurrent readers from all reactors (`Sync`).
static_assertions::assert_impl_all!(Catalog: Send, Sync);
static_assertions::assert_impl_all!(ClusterSnapshot: Send, Sync);

impl Catalog {
    /// Publishes the initial snapshot — boot's only write (the T5.3
    /// housekeeper writer takes over after this).
    pub fn new(initial: ClusterSnapshot) -> Self {
        Catalog {
            swap: ArcSwap::from_pointee(initial),
        }
    }

    /// FR-28's read: one pointer copy, no lock, no compare-exchange
    /// (§6.3). The [`Guard`] derefs to the published [`Arc`] without
    /// bumping its refcount; finish the read synchronously — it must not
    /// span an `.await` (see the module doc for the debt-slot reason).
    /// A snapshot observed through a guard stays alive and coherent no
    /// matter how many times the writer replaces it (the "never dropped
    /// under a reader" pin in the tests).
    pub fn load(&self) -> Guard<Arc<ClusterSnapshot>> {
        self.swap.load()
    }

    /// The owned form of [`Catalog::load`]: one refcount increment, for
    /// a reader that must hold its snapshot across an await point.
    pub fn load_full(&self) -> Arc<ClusterSnapshot> {
        self.swap.load_full()
    }

    /// The single write path (I9): the housekeeper publishes a freshly
    /// built immutable snapshot whole — never in place, never compared.
    /// Readers observe either the old snapshot or the new one, and the
    /// old one drops when its last reader finishes (§8.4). `pub(crate)`
    /// so the writer stays in-crate; the T5.3 fan-out is its only
    /// intended caller.
    pub(crate) fn store(&self, next: Arc<ClusterSnapshot>) {
        self.swap.store(next);
    }
}

/// A13's boot-time proxy: the NUMA hosting the fewest workers (most
/// headroom per worker), ties broken by first appearance in config order.
/// `None` when the cluster has no workers (config validation forbids it,
/// but a directly-built config reaches this before `validate`).
fn least_loaded_numa(worker_numas: &[u32]) -> Option<u32> {
    let mut counts: Vec<(u32, u32)> = Vec::new();
    for &numa in worker_numas {
        match counts.iter_mut().find(|(n, _)| *n == numa) {
            Some((_, c)) => *c += 1,
            None => counts.push((numa, 1)),
        }
    }
    counts
        .iter()
        .min_by_key(|(_, count)| *count)
        .map(|(numa, _)| *numa)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn single_binds_the_least_loaded_numa() {
        // Fewest workers wins over config order: numa 1 has one worker to
        // numa 0's two.
        let snap = ClusterSnapshot::build(NumaAffinity::Single, &[0, 0, 1]);
        assert_eq!(
            snap.resolve(Some("dev")),
            Placement {
                affinity: NumaAffinity::Single,
                bound_numa: Some(1)
            }
        );
        // Fewest workers wins regardless of config order: numa 1 leads the
        // list with one worker, numa 0 follows with two.
        let snap = ClusterSnapshot::build(NumaAffinity::Single, &[1, 0, 0]);
        assert_eq!(snap.resolve(None).bound_numa, Some(1));
        // An exact tie (one worker per numa): config order decides — numa 1
        // was seen first, so numa 1 wins over numa 0.
        let snap = ClusterSnapshot::build(NumaAffinity::Single, &[1, 0]);
        assert_eq!(snap.resolve(None).bound_numa, Some(1));
        // No workers at all: nothing to bind.
        assert_eq!(
            ClusterSnapshot::build(NumaAffinity::Single, &[]).resolve(None).bound_numa,
            None
        );
    }

    #[test]
    fn cross_binds_nothing() {
        let snap = ClusterSnapshot::build(NumaAffinity::Cross, &[0, 1, 1]);
        assert_eq!(
            snap.resolve(Some("anything")),
            Placement {
                affinity: NumaAffinity::Cross,
                bound_numa: None
            }
        );
    }

    /// The T5.2 race test, with real interleaving forced and asserted: a
    /// writer publishes 5 000 whole snapshots while three readers load
    /// concurrently (all threads rendezvous on a barrier first). Every
    /// observation must be a *complete* snapshot (the `single` ⇒ bound
    /// invariant — a torn read would show the `cross` shape), a reader
    /// must never observe revisions out of order (the pointer only moves
    /// forward), and after the writer finishes every reader must see the
    /// final revision. The test fails if no reader ever saw the pointer
    /// MOVE — that would mean it ran without interleaving, "just run and
    /// hope" in the plan's words.
    #[test]
    fn concurrent_readers_never_see_a_torn_or_regressing_snapshot() {
        const WRITES: u32 = 5_000;
        const READERS: usize = 3;
        let catalog = Arc::new(Catalog::new(ClusterSnapshot {
            default_affinity: NumaAffinity::Single,
            default_bound: Some(0),
        }));
        let barrier = Arc::new(std::sync::Barrier::new(READERS + 1));
        let done = Arc::new(AtomicBool::new(false));

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let catalog = Arc::clone(&catalog);
                let barrier = Arc::clone(&barrier);
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    barrier.wait();
                    let mut distinct = 0;
                    let mut last = 0;
                    let mut iters = 0;
                    // The floor keeps every reader looping even if the
                    // writer somehow outpaces its first check; the done
                    // flag then ends the spin promptly.
                    while !done.load(Ordering::Acquire) || iters < 200 {
                        // Each observation: a complete snapshot. The
                        // revision rides in `bound_numa` — every
                        // published snapshot is `single`-bound to its
                        // own revision number.
                        let placement = catalog.load().resolve(Some("probe"));
                        assert_eq!(
                            placement.affinity,
                            NumaAffinity::Single,
                            "a read must be a whole snapshot, never torn"
                        );
                        let rev = placement
                            .bound_numa
                            .expect("single-affinity snapshots are bound");
                        assert!(
                            rev >= last,
                            "a reader never observes the pointer going back"
                        );
                        if rev != last {
                            distinct += 1;
                        }
                        last = rev;
                        if iters % 64 == 0 {
                            std::thread::yield_now();
                        }
                        iters += 1;
                    }
                    // Publication is visible: after the writer is done,
                    // the final revision is what every reader gets.
                    let final_placement = catalog.load().resolve(Some("probe"));
                    assert_eq!(final_placement.bound_numa, Some(WRITES - 1));
                    (distinct, last)
                })
            })
            .collect();

        barrier.wait();
        for rev in 0..WRITES {
            catalog.store(Arc::new(ClusterSnapshot {
                default_affinity: NumaAffinity::Single,
                default_bound: Some(rev),
            }));
            if rev % 64 == 0 {
                std::thread::yield_now();
            }
        }
        done.store(true, Ordering::Release);

        let interleaved = readers
            .into_iter()
            .map(|h| h.join().expect("reader thread joins"))
            .any(|(distinct, _)| distinct >= 1);
        assert!(
            interleaved,
            "no reader ever saw the pointer move — the race test did not interleave"
        );
    }

    /// §8.4's third guarantee, by refcount: a replaced snapshot is
    /// released by the holder the moment the store lands, and its memory
    /// frees exactly when the last reader finishes (the `Weak` upgrade
    /// fails). `load_full`-only reads keep this deterministic — Guards
    /// use debt slots whose release timing is an implementation detail;
    /// the guard-pinning test below covers the Guard case behaviorally.
    #[test]
    fn replaced_snapshot_is_reclaimed_once_readers_finish() {
        let catalog = Catalog::new(ClusterSnapshot::build(NumaAffinity::Cross, &[0, 1]));
        let old = catalog.load_full();
        assert_eq!(
            Arc::strong_count(&old),
            2,
            "holder holds one, this reader holds one"
        );
        let weak = Arc::downgrade(&old);

        catalog.store(Arc::new(ClusterSnapshot::build(
            NumaAffinity::Single,
            &[0, 1],
        )));
        assert_eq!(
            Arc::strong_count(&old),
            1,
            "the holder released the replaced snapshot at the store"
        );
        drop(old);
        assert!(
            weak.upgrade().is_none(),
            "snapshot memory reclaimed once the last reader finished"
        );
    }

    /// The review bullet pinned from the positive side: a reader holding
    /// a Guard while the writer replaces the snapshot twice still sees
    /// ITS OWN coherent snapshot (never dropped under it, never torn),
    /// the pinned memory frees when the reader finishes, and fresh reads
    /// observe the latest publication.
    #[test]
    fn guard_pins_its_snapshot_across_replacement_and_frees_it_when_finished() {
        let catalog = Catalog::new(ClusterSnapshot::build(NumaAffinity::Single, &[0, 1, 1]));
        let guard = catalog.load();
        let pinned: Arc<ClusterSnapshot> = Arc::clone(&*guard);
        let weak = Arc::downgrade(&pinned);

        // The writer replaces twice while the read is "in flight".
        catalog.store(Arc::new(ClusterSnapshot::build(NumaAffinity::Cross, &[0, 1])));
        catalog.store(Arc::new(ClusterSnapshot::build(NumaAffinity::Single, &[1, 1])));

        assert_eq!(
            guard.resolve(None).bound_numa,
            Some(0),
            "the guard still sees its own coherent snapshot"
        );
        drop(guard);
        drop(pinned);
        assert!(
            weak.upgrade().is_none(),
            "the pinned snapshot freed once the last reader finished"
        );
        assert_eq!(
            catalog.load().resolve(None).bound_numa,
            Some(1),
            "fresh reads see the latest publication"
        );
    }
}
