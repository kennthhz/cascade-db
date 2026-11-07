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
//! with R11's bounded wait). The wrapper is deliberate beyond the
//! visibility: `ArcSwapAny` also exposes `swap`/`compare_exchange`/
//! `rcu`-style writes, and a single-narrow surface is what makes "no
//! second writer path" (I9) reviewable by grep. The writer itself is
//! T5.3's `start_writer` (below); a [`Catalog`] constructed EMPTY —
//! `Catalog::empty` — has nothing to serve until its first store lands,
//! which is exactly the boot-order gate: workers spawn before §10.1
//! step 7's publication and refuse traffic (`57P03`, [`gate_handoff`])
//! until their copy arrives (FR-6).
//!
//! Per-reactor instances are the T5.3 fan-out shape (every reactor holds
//! its own `ArcSwap`; R11's laggard adoption means each adopts a new
//! snapshot on ITS next pointer load). The serialized writer runs on the
//! serving housekeeper's ring: worker `submit_to`s land in per-worker
//! write channels (the T5.1 fan-in), their job futures funnel the change
//! through a same-ring local channel into `writer_loop` — the ONE task
//! that applies to [`ClusterState`] and publishes — and each publication
//! fans out as `submit_to` store jobs whose futures store into each
//! target's own catalog on that target's ring. R11 governs the wait:
//! the issuing reactor's store is submitted and awaited FIRST (its
//! session always sees its own change), the whole fan-out shares
//! `tierc_fanout_wait_ms`, and the ack is [`TierCAck::Applied`] or
//! [`TierCAck::Pending(n)`](TierCAck::Pending) — pending never means
//! lost, because the store job sits queued in the laggard's publication
//! channel and lands there (lazily, on its next pointer load) when the
//! ring drains.
//!
//! The snapshot payload is still the routing half (FR-10) plus the
//! per-DB affinity rows the writer fills: a row (`pg_database
//! .datnumaffinity` / `datnumanode`, name-keyed until T5.5's catalog
//! work replaces the keying) overrides the cluster default for that
//! database, and every other name — including `None` — inherits the
//! default, which is exactly the inheritance FR-10 requires and the
//! T2.4 test-review's "no explicit affinity" case. A `single` bound
//! NUMA is picked per A13 (least-loaded NUMA at first activation); at
//! boot every NUMA is idle, so the standing proxy is fewest workers,
//! tie-broken by config order — the same proxy the writer applies when
//! a change binds a new row. (The payload carries FR-28's literal type
//! name, `ClusterSnapshot` — the T2.4 static snapshot took the spec's
//! name when it grew the ArcSwap read path.)

use std::{
    cell::RefCell,
    future::Future,
    net::TcpStream,
    os::unix::io::FromRawFd,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use arc_swap::{ArcSwapOption, Guard};
use glommio::channels::local_channel::{LocalReceiver, LocalSender, new_unbounded};
use glommio::channels::shared_channel::SharedReceiver;

use crate::{
    config::NumaAffinity,
    handoff::Handoff,
    pg::send_fatal,
    submit::{self, oneshot, SubmitError, SubmitHandle, SubmitInit, SubmitJob, SubmitStats},
};

/// The placement a connection's database resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub affinity: NumaAffinity,
    /// For `single` affinity: the bound NUMA (A13 pick). `None` when the
    /// cluster has no workers at all — routing then has no candidates.
    pub bound_numa: Option<u32>,
}

/// One per-database affinity row — FR-10's `pg_database
/// .datnumaffinity` / `datnumanode`, name-keyed until T5.5's catalog
/// work replaces the keying.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DbRow {
    affinity: NumaAffinity,
    bound_numa: Option<u32>,
}

/// The cluster metadata snapshot every reactor serves reads from — FR-28's
/// `ArcSwap<ClusterSnapshot>` payload. Immutable by construction: readers
/// never observe a half-updated snapshot, the writer (the serialized
/// `writer_loop`) builds a fresh one and publishes it whole. `Clone` is
/// for the writer: it applies to [`ClusterState`] in place and publishes
/// a fresh clone per change (whole-snapshot publication, never patching
/// a live snapshot).
#[derive(Debug, Clone)]
pub struct ClusterSnapshot {
    default_affinity: NumaAffinity,
    /// A13 bound NUMA for default-`single` databases.
    default_bound: Option<u32>,
    /// Per-database affinity rows (FR-10). Names absent from the map —
    /// and `None` — inherit the cluster default.
    databases: std::collections::BTreeMap<String, DbRow>,
    /// Cluster-wide role names (T5.4's CREATE ROLE-class DDL). A set —
    /// duplicates are the writer's refusal (42710), so entries are unique
    /// by construction. Sorted iteration for stable wire output.
    roles: std::collections::BTreeSet<String>,
}

impl ClusterSnapshot {
    /// Builds the snapshot from the cluster default and the boot-time
    /// worker layout — ONE ENTRY PER WORKER, the entry being that
    /// worker's NUMA id, in config order (boot.rs flattens
    /// `worker_cores` exactly so; `least_loaded_numa` counts entries).
    /// `single`
    /// binds per A13; `cross` binds nothing. The per-DB map starts
    /// empty — no rows exist until the first change applies (T5.3's
    /// writer; T5.5 wires the DDL surface to it).
    pub fn build(default_affinity: NumaAffinity, worker_numas: &[u32]) -> Self {
        let default_bound = match default_affinity {
            NumaAffinity::Single => least_loaded_numa(worker_numas),
            NumaAffinity::Cross => None,
        };
        ClusterSnapshot {
            default_affinity,
            default_bound,
            databases: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeSet::new(),
        }
    }

    /// FR-10 lookup: the placement for a connection's database name.
    /// A row for the name overrides the cluster default; every other
    /// name — including `None` — inherits it.
    pub fn resolve(&self, database: Option<&str>) -> Placement {
        let row = database.and_then(|name| self.databases.get(name));
        let (affinity, bound_numa) = match row {
            Some(row) => (row.affinity, row.bound_numa),
            None => (self.default_affinity, self.default_bound),
        };
        Placement { affinity, bound_numa }
    }

    /// The cluster default the snapshot was built with (diagnostics).
    pub fn default_affinity(&self) -> NumaAffinity {
        self.default_affinity
    }

    /// The cluster's role names (T5.4), sorted — the read side of the
    /// CREATE ROLE-class DDL. Served from whichever snapshot a pointer
    /// load produced: no invalidation, no callback — a reactor learns a
    /// new role exactly when its catalog adopts a newer snapshot.
    pub fn roles(&self) -> impl Iterator<Item = &str> {
        self.roles.iter().map(String::as_str)
    }

    /// The per-database affinity rows (T5.5, FR-38's `datnumaffinity` /
    /// `datnumanode`): `(name, affinity spelling, bound NUMA)` in name
    /// order — `None` when the row binds nothing (`cross`). Only rows
    /// the DDL (or an affinity change) created appear here; names
    /// absent from the map inherit the cluster default (FR-10).
    pub fn databases(&self) -> impl Iterator<Item = (&str, &'static str, Option<u32>)> {
        self.databases
            .iter()
            .map(|(name, row)| (name.as_str(), row.affinity.as_str(), row.bound_numa))
    }
}

/// The per-reactor catalog holder — FR-28's `ArcSwap<ClusterSnapshot>`.
/// Shared as `Arc<Catalog>`; construction is cheap enough that T5.3
/// hands one to every reactor. `ArcSwapOption` because a WORKER's
/// catalog boots EMPTY (FR-6's gate reads the absence); the `Option`
/// resolves on this reactor's first publication store.
#[derive(Debug)]
pub struct Catalog {
    swap: ArcSwapOption<ClusterSnapshot>,
}

// The holder crosses the boot: the T5.3 writer runs on the housekeeper's
// path and readers live on every ring; the payload must survive a fan-out
// job (`Send`) and serve concurrent readers from all reactors (`Sync`).
static_assertions::assert_impl_all!(Catalog: Send, Sync);
static_assertions::assert_impl_all!(ClusterSnapshot: Send, Sync);

impl Catalog {
    /// An empty catalog: nothing published yet. Every WORKER boots with
    /// one (FR-6's gate — it refuses traffic with `57P03` until the
    /// initial publication lands on its ring); the serving housekeeper's
    /// own catalog starts published via [`Catalog::new`].
    pub fn empty() -> Self {
        Catalog {
            swap: ArcSwapOption::empty(),
        }
    }

    /// Publishes the initial snapshot at construction — boot's write for
    /// the serving housekeeper (the serialized writer takes over after
    /// this; workers get theirs via the fan-out, starting empty).
    pub fn new(initial: ClusterSnapshot) -> Self {
        Catalog {
            swap: ArcSwapOption::new(Some(Arc::new(initial))),
        }
    }

    /// FR-28's read: one pointer copy, no lock, no compare-exchange
    /// (§6.3). The [`Guard`] derefs to the published `Option<Arc>` —
    /// `None` before this reactor's first publication lands (the FR-6
    /// gate reads that, not the payload). Finish the read
    /// synchronously — it must not span an `.await` (see the module doc
    /// for the debt-slot reason). A snapshot observed through a guard
    /// stays alive and coherent no matter how many times the writer
    /// replaces it (the "never dropped under a reader" pin in the
    /// tests).
    pub fn load(&self) -> Guard<Option<Arc<ClusterSnapshot>>> {
        self.swap.load()
    }

    /// The owned form of [`Catalog::load`]: one refcount increment, for
    /// a reader that must hold its snapshot across an await point.
    /// `None` before the first publication lands on this reactor.
    pub fn load_full(&self) -> Option<Arc<ClusterSnapshot>> {
        self.swap.load_full()
    }

    /// The single write path (I9): the writer publishes a freshly built
    /// immutable snapshot whole — never in place, never compared.
    /// Readers observe either the old snapshot or the new one, and the
    /// old one drops when its last reader finishes (§8.4). `pub(crate)`
    /// so the writer stays in-crate; the fan-out store jobs (`writer_loop`
    /// and the boot's initial publication) are its only intended
    /// callers.
    pub(crate) fn store(&self, next: Arc<ClusterSnapshot>) {
        self.swap.store(Some(next));
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

/// PG error code for "the database system is starting up" — the refusal
/// the FR-6 gate (workers, pre-publication) answers with. The standard's
/// `cannot_connect_now`.
pub(crate) const CANNOT_CONNECT_NOW: &str = "57P03";

/// The per-worker write channel's bound (FR-29's fan-in). Admission is
/// eager and synchronous (`try_send`): a full channel refuses up-front —
/// pressure surfaces at the DDL caller instead of hiding in a queue.
/// 16 in-flight changes per worker dwarfs any real DDL rate; the
/// retry-within-budget backpressure is deferred until a caller exists
/// that needs it.
pub(crate) const TIERC_WRITE_CAP: usize = 16;

/// The per-worker publication channel's bound (the fan-out's queue). A
/// stalled worker's store jobs sit here and land when its ring drains —
/// lazy adoption — without ever blocking the writer past R11's budget.
/// 64 queued store jobs per worker is far past any realistic fan-out
/// depth.
pub(crate) const TIERC_PUBLISH_CAP: usize = 64;

/// A Tier C change request (FR-29): the serialized writer applies these,
/// one at a time. Grew its first DDL variant in T5.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierCChange {
    /// Binds `name` to `affinity` (FR-10's per-DB rows, name-keyed
    /// until T5.5). `Single` picks its bound NUMA per A13 at apply time —
    /// the least-loaded NUMA when the change lands.
    SetDatabaseAffinity { name: String, affinity: NumaAffinity },
    /// Creates a cluster role (T5.4, FR-29's DDL path). Applied against
    /// the writer's [`ClusterState`], published whole, and visible to
    /// every reactor through its next pointer load — no cache
    /// invalidation anywhere.
    CreateRole { name: String },
    /// Creates a database (T5.5, FR-38's `datnumaffinity`). `None`
    /// resolves to the cluster default AT CREATION and freezes into the
    /// row (PG-faithful: inheritance is a creation-time decision, and
    /// the row is what makes a duplicate name refusable — `42P04`).
    /// `Single` binds per A13 at apply, like [`TierCChange::SetDatabaseAffinity`].
    CreateDatabase {
        name: String,
        affinity: Option<NumaAffinity>,
    },
}

/// Why the writer refused a change: the apply failed against the current
/// state, and the snapshot is UNCHANGED — nothing published, no fan-out,
/// nothing to roll back (the refusal happens before the state moves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// A role with this name already exists — PG's `duplicate_object`.
    DuplicateRole { name: String },
    /// A database with this name already exists — PG's
    /// `duplicate_database`.
    DuplicateDatabase { name: String },
}

impl ApplyError {
    /// The SQLSTATE + message pair the wire carries for this refusal.
    pub fn pg(&self) -> (&'static str, String) {
        match self {
            ApplyError::DuplicateRole { name } => (
                "42710",
                format!("role \"{name}\" already exists"),
            ),
            ApplyError::DuplicateDatabase { name } => (
                "42P04",
                format!("database \"{name}\" already exists"),
            ),
        }
    }
}

/// R11's mutation result. `Applied` when every reactor confirmed its
/// store within `tierc_fanout_wait_ms`; `Pending(n)` with the laggard
/// count otherwise. Pending never means lost: the laggards' store jobs
/// are queued in their publication channels and land when those rings
/// drain (lazy adoption on the next pointer load) — the budget bounds
/// only the WRITER's wait, never the delivery. `Failed` is the refused
/// apply: the snapshot is unchanged and no fan-out ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierCAck {
    Applied,
    Pending(u32),
    Failed(ApplyError),
}

/// The writer's in-memory Tier C state — the ONE mutable copy (FR-29:
/// the single serialized writer owns it; nothing else touches it).
/// Applies changes in place, publishes immutable clones.
pub(crate) struct ClusterState {
    current: ClusterSnapshot,
    worker_numas: Vec<u32>,
}

impl ClusterState {
    /// Boot hands the writer its already-built initial snapshot (boot
    /// resolved the WAL home against it before the writer existed).
    fn from_boot(initial: ClusterSnapshot, worker_numas: Vec<u32>) -> Self {
        ClusterState {
            current: initial,
            worker_numas,
        }
    }

    /// Applies one change in place (single-threaded by construction —
    /// nothing else holds this state), or refuses it: `Err` means the
    /// state is EXACTLY as it was (every variant validates before its
    /// first mutation, so a refusal can never leave a half-applied
    /// change behind). `single` binds per A13, the same least-loaded
    /// proxy boot uses; T5.6's live load view replaces the proxy.
    fn apply(&mut self, change: &TierCChange) -> Result<(), ApplyError> {
        match change {
            TierCChange::SetDatabaseAffinity { name, affinity } => {
                let bound_numa = match affinity {
                    NumaAffinity::Single => least_loaded_numa(&self.worker_numas),
                    NumaAffinity::Cross => None,
                };
                self.current.databases.insert(
                    name.clone(),
                    DbRow {
                        affinity: *affinity,
                        bound_numa,
                    },
                );
                Ok(())
            }
            TierCChange::CreateRole { name } => {
                if self.current.roles.contains(name) {
                    return Err(ApplyError::DuplicateRole { name: name.clone() });
                }
                self.current.roles.insert(name.clone());
                Ok(())
            }
            TierCChange::CreateDatabase { name, affinity } => {
                // Validate before the first mutation, like every variant:
                // a refusal leaves the state exactly as it was.
                if self.current.databases.contains_key(name) {
                    return Err(ApplyError::DuplicateDatabase { name: name.clone() });
                }
                // The bare form freezes the cluster default at creation
                // (PG-faithful inheritance); the explicit form records
                // what the caller asked for. `Single` binds per A13's
                // standing proxy (fewest workers, config-order ties) —
                // the same rule boot used for the default.
                let affinity = affinity.unwrap_or(self.current.default_affinity);
                let bound_numa = match affinity {
                    NumaAffinity::Single => least_loaded_numa(&self.worker_numas),
                    NumaAffinity::Cross => None,
                };
                self.current.databases.insert(
                    name.clone(),
                    DbRow {
                        affinity,
                        bound_numa,
                    },
                );
                Ok(())
            }
        }
    }

    /// The current content as a fresh immutable snapshot — what the
    /// fan-out publishes whole.
    fn publish(&self) -> Arc<ClusterSnapshot> {
        Arc::new(self.current.clone())
    }
}

/// One serialized-writer command. `Apply` carries the issuing reactor's
/// core id (R11's fan-out ordering key) and the ack channel.
pub(crate) enum WriteCmd {
    Apply(u32, TierCChange, oneshot::Sender<TierCAck>),
}

thread_local! {
    /// The write funnel into the serialized writer — a same-ring local
    /// channel, installed once by [`start_writer`] on the serving
    /// housekeeper's ring. Job futures running there (via the write
    /// channels' `submit_loop`s) reach it: the closure crossed, the
    /// future it constructs is local (the T5.1 rule). `try_send` makes
    /// the funnel lookup borrow-scoped — never held across an await.
    static FUNNEL: RefCell<Option<LocalSender<WriteCmd>>> = const { RefCell::new(None) };
}

/// The Tier C wiring boot hands to [`start_writer`]: the serving
/// housekeeper's own catalog, one empty catalog per worker, the
/// per-worker write channels (fan-in) and publication channel inits
/// (fan-out), and the cluster facts the writer needs. All Vec orders
/// are the workers' per-core order — [`fan_out`]'s issuer-first
/// ordering keys on `worker_cores`. The struct is `pub` only because
/// `Router::serve`'s signature mentions it; the fields are crate-
/// private, so only boot can build one.
pub struct TierCPlane {
    /// The serving housekeeper's own catalog — the router reads it on
    /// this same ring.
    pub(crate) catalog: Arc<Catalog>,
    /// One empty catalog per worker (same order as `worker_cores`);
    /// each fills when its publication store lands.
    pub(crate) worker_catalogs: Vec<Arc<Catalog>>,
    /// The workers' core ids, same order.
    pub(crate) worker_cores: Vec<u32>,
    /// Per-worker write-channel receivers (the fan-in); connected and
    /// served by `submit_loop`s on THIS ring inside [`start_writer`].
    pub(crate) write_receivers: Vec<SharedReceiver<SubmitJob>>,
    pub(crate) write_stats: Vec<Arc<SubmitStats>>,
    /// Per-worker publication-channel inits (the fan-out); connected
    /// here — the worker ends connect their halves and run their own
    /// `submit_loop`s.
    pub(crate) publish_inits: Vec<SubmitInit>,
    /// R11's bounded wait (`tierc_fanout_wait_ms`), shared by each
    /// fan-out's stores.
    pub(crate) fanout_wait: Duration,
    /// The boot layout — [`ClusterState`]'s A13 seed (the cluster
    /// default rides in `initial`).
    pub(crate) worker_numas: Vec<u32>,
}

/// The mutation entry point (FR-29). Callable from ANY ring: the change
/// crosses the ring's own write channel as a `submit_to` job, is
/// funneled (same-ring local channel, [`FUNNEL`]) into the ONE
/// `writer_loop` on the serving housekeeper's ring, and the caller
/// awaits the R11 ack. On a worker ring the job runs through that
/// worker's write channel; on the housekeeper ring itself the funnel is
/// direct.
///
/// `Err(SubmitError::Lost)` when this ring has no live writer (never
/// served, or the serving housekeeper is gone) or the writer dropped
/// the ack — never a panic, never a hang (the T5.1 blast-radius rule).
pub async fn submit_change(
    issuer_core: u32,
    change: TierCChange,
) -> Result<TierCAck, SubmitError> {
    let (ack_tx, ack_rx) = oneshot::channel::<TierCAck>();
    let delivered = FUNNEL.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|funnel| {
                funnel
                    .try_send(WriteCmd::Apply(issuer_core, change, ack_tx))
                    .is_ok()
            })
            .unwrap_or(false)
    });
    if !delivered {
        // No funnel on this ring (never served) or the channel refused:
        // honest loss, surfaced at the caller.
        return Err(SubmitError::Lost);
    }
    ack_rx
        .await
        .map_err(|oneshot::Canceled| SubmitError::Lost)
}

/// Installs the Tier C writer on the serving housekeeper's ring (FR-29).
/// Connects the per-worker write channels and serves them with
/// `submit_loop`s (the fan-in), connects the publication channels (the
/// fan-out handles), spawns the ONE `writer_loop` task, and returns the
/// receiver that fires once the INITIAL snapshot has been stored on
/// every reactor — §10.1 step 7. The caller must not open its accept
/// loop before that fires (FR-6: Tier C published before any worker
/// serves traffic). Runs on the housekeeper ring (`Router::serve`).
pub(crate) async fn start_writer(
    plane: TierCPlane,
    initial: ClusterSnapshot,
) -> oneshot::Receiver<()> {
    let (cmd_tx, cmd_rx) = new_unbounded::<WriteCmd>();
    FUNNEL.with(|slot| *slot.borrow_mut() = Some(cmd_tx));

    // The fan-in: every worker's write channel gets a forwarder on this
    // ring; its job futures find the funnel through the thread_local.
    for (rx, stats) in plane
        .write_receivers
        .into_iter()
        .zip(plane.write_stats)
    {
        let rx = rx.connect().await;
        glommio::spawn_local(submit::submit_loop(rx, stats)).detach();
    }

    // The fan-out handles: publication-channel inits connect here; the
    // worker ends hold the matching receivers (order mirrors the
    // workers' write→publish connect order — no cross-family cycle).
    let mut handles = Vec::with_capacity(plane.worker_cores.len());
    for ((core, init), catalog) in plane
        .worker_cores
        .into_iter()
        .zip(plane.publish_inits)
        .zip(plane.worker_catalogs)
    {
        let handle = init.connect().await;
        handles.push((core, handle, catalog));
    }

    let (published_tx, published_rx) = oneshot::channel();
    glommio::spawn_local(writer_loop(
        cmd_rx,
        plane.catalog,
        handles,
        initial,
        plane.worker_numas,
        plane.fanout_wait,
        published_tx,
    ))
    .detach();
    published_rx
}

/// The serialized writer (FR-29): the ONE task that owns
/// [`ClusterState`]. Applies each funneled change, publishes a fresh
/// immutable snapshot whole — own catalog first (the router reads it on
/// this ring), then the fan-out — and acks with R11's result. Ends when
/// every producer dropped (the funnel ends with the serving ring).
async fn writer_loop(
    cmds: LocalReceiver<WriteCmd>,
    catalog: Arc<Catalog>,
    handles: Vec<(u32, SubmitHandle, Arc<Catalog>)>,
    initial: ClusterSnapshot,
    worker_numas: Vec<u32>,
    budget: Duration,
    published: oneshot::Sender<()>,
) {
    let mut state = ClusterState::from_boot(initial, worker_numas);

    // §10.1 step 7's publish half: the initial snapshot stores here and
    // fans out BEFORE this function's completion lets `start_writer`'s
    // caller open the accept loop (FR-6). Workers spawned early are
    // gated on it — [`gate_handoff`].
    let initial_snapshot = state.publish();
    catalog.store(Arc::clone(&initial_snapshot));
    fan_out(&handles, None, &initial_snapshot, budget).await;
    let _ = published.send(());

    while let Some(cmd) = cmds.recv().await {
        let WriteCmd::Apply(issuer, change, ack) = cmd;
        // Test-only hold (testutil's H2 discriminator park): a held
        // change sits received-but-unapplied — its session's gauge is
        // up, its ack unsent — until the test releases. Production
        // cost: one relaxed load per DDL.
        while crate::testutil::writer_held_before_apply() {
            glommio::timer::sleep(Duration::from_millis(1)).await;
        }
        // The ack is ALWAYS last and the stores ALWAYS follow one
        // successful apply: a refused change (Failed) publishes nothing
        // and fans nothing out — the ack travels alone. The ordering is
        // R11's contract: no caller ever learns "applied" from anything
        // but a completed store sequence.
        let ack_value = match state.apply(&change) {
            Ok(()) => {
                let snap = state.publish();
                // Own catalog first — the router on this ring serves the
                // next connection from it even before the fan-out
                // confirms.
                catalog.store(Arc::clone(&snap));
                fan_out(&handles, Some(issuer), &snap, budget).await
            }
            Err(e) => TierCAck::Failed(e),
        };
        let _ = ack.send(ack_value);
    }
}

/// R11's fan-out: submit a whole-snapshot store to every reactor's
/// catalog, the ISSUING reactor first (its session must see its own
/// change when the ack lands — and it is awaited first), all stores
/// sharing one budget. `issuer: None` is the boot publication (§10.1
/// step 7, natural order). Returns [`TierCAck::Applied`] when every
/// store confirmed within the budget, [`TierCAck::Pending(n)`] with the
/// laggard count otherwise.
///
/// Pending never means lost: a store job whose `Submit` we stop waiting
/// on was already admitted to its publication channel (`submit_to`
/// admits eagerly), so it lands when that ring drains — the dropped
/// `Submit` future only abandons the ack, not the job. The one true
/// loss is a `Saturated` admission (channel full — >64 un-drained
/// fan-outs against one reactor, unreachable in practice and surfaced
/// as pending here; real backpressure is deferred with
/// [`TIERC_WRITE_CAP`]).
/// Waits for one fan-out store job, at most `budget`. The hand-rolled
/// stand-in for `timer::timeout`: glommio's version only fits futures
/// resolving to ITS `Result` type (`GlommioError<()>`), and [`Submit`]
/// resolves with [`SubmitError`] — so this races the two futures with
/// the caller's waker (either waking re-polls both, and the submit is
/// checked first, so a confirm and a deadline in the same wake counts
/// as confirmed). `None` = unconfirmed: the budget died first, or the
/// submit resolved with an error (reactor gone). Dropping the submit at
/// the deadline only abandons the ack — the job was already admitted to
/// its publication queue (see [`fan_out`]).
async fn budgeted<R: Send + 'static>(budget: Duration, fut: submit::Submit<R>) -> Option<R> {
    struct Race<R> {
        fut: submit::Submit<R>,
        timer: glommio::timer::Timer,
    }
    impl<R: Send + 'static> Future for Race<R> {
        type Output = Option<R>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            if let Poll::Ready(value) = Pin::new(&mut this.fut).poll(cx) {
                return Poll::Ready(value.ok());
            }
            if Pin::new(&mut this.timer).poll(cx).is_ready() {
                return Poll::Ready(None);
            }
            Poll::Pending
        }
    }
    Race {
        fut,
        timer: glommio::timer::Timer::new(budget),
    }
    .await
}

async fn fan_out(
    handles: &[(u32, SubmitHandle, Arc<Catalog>)],
    issuer: Option<u32>,
    snap: &Arc<ClusterSnapshot>,
    budget: Duration,
) -> TierCAck {
    let deadline = Instant::now() + budget;
    // Submission order: issuing reactor first. Its store job enters an
    // empty queue and lands while the rest are still being submitted.
    let mut order: Vec<usize> = (0..handles.len()).collect();
    if let Some(core) = issuer {
        if let Some(pos) = handles.iter().position(|(c, _, _)| *c == core) {
            order.swap(0, pos);
        }
    }
    let mut pending = Vec::with_capacity(order.len());
    for idx in order {
        let (_, handle, catalog) = &handles[idx];
        let job_catalog = Arc::clone(catalog);
        let job_snap = Arc::clone(snap);
        pending.push(handle.submit_to(move || {
            let catalog = job_catalog;
            let snap = job_snap;
            async move {
                catalog.store(snap);
            }
        }));
    }
    // Await in submission order under one shared deadline.
    let mut unconfirmed = 0u32;
    for fut in pending {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match budgeted(remaining, fut).await {
            Some(()) => {}
            // Budget blown (jobs stay queued for lazy adoption) and
            // Lost/Saturated (reactor gone — shutdown): both count as
            // unconfirmed; the ack is honest about what answered in
            // time.
            None => unconfirmed += 1,
        }
    }
    if unconfirmed == 0 {
        TierCAck::Applied
    } else {
        TierCAck::Pending(unconfirmed)
    }
}

/// FR-6's worker-side gate (§10.1 step 7 → step 9): a handoff arriving
/// before THIS reactor's initial snapshot has landed is refused with PG
/// `57P03` ([`CANNOT_CONNECT_NOW`], "the database system is starting
/// up") — never served from an absent catalog. The fd is adopted here
/// exactly once (the same adoption `session::run` makes on the `Ok`
/// path) and closed with the error response; `Ok` hands the untouched
/// [`Handoff`] back to the worker loop.
pub(crate) fn gate_handoff(handoff: Handoff, catalog: &Catalog) -> Result<Handoff, ()> {
    if catalog.load().as_ref().is_some() {
        return Ok(handoff);
    }
    // SAFETY: the fd arrives through the handoff channel and this call
    // is its single owner — on the Ok path `session::run` adopts it the
    // same way; here the stream's drop closes it after the error write.
    let mut stream = unsafe { TcpStream::from_raw_fd(handoff.fd) };
    let _ = send_fatal(
        &mut stream,
        CANNOT_CONNECT_NOW,
        "the database system is starting up",
    );
    Err(())
}

#[cfg(test)]
mod tests {
    use std::{io::Read, os::unix::io::IntoRawFd, sync::Mutex};

    use super::*;
    use glommio::LocalExecutorBuilder;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A default-`single` snapshot bound to `bound` — the race payload.
    fn snap(bound: Option<u32>) -> ClusterSnapshot {
        ClusterSnapshot {
            default_affinity: NumaAffinity::Single,
            default_bound: bound,
            databases: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeSet::new(),
        }
    }

    /// Pin-heavy fixtures must not overlap (deployment.rs discipline);
    /// the plain-thread race tests below don't pin and don't take it.
    static WRITER_SERIAL: Mutex<()> = Mutex::new(());

    fn writer_serial() -> std::sync::MutexGuard<'static, ()> {
        WRITER_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

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

    /// T5.4's apply contract, on the writer's own state: a CreateRole
    /// applies and publishes into `roles`; a duplicate is REFUSED with
    /// the 42710 error and leaves the state exactly as it was (no
    /// half-applied residue, no second entry) — the review bullet's
    /// "partial-failure" case, pinned at the state layer.
    #[test]
    fn create_role_applies_once_and_refuses_duplicates_without_mutation() {
        let mut state = ClusterState::from_boot(
            ClusterSnapshot::build(NumaAffinity::Cross, &[0]),
            vec![0],
        );
        state
            .apply(&TierCChange::CreateRole { name: "alice".into() })
            .expect("first create applies");
        assert_eq!(state.current.roles.iter().collect::<Vec<_>>(), ["alice"]);

        let err = state
            .apply(&TierCChange::CreateRole { name: "alice".into() })
            .expect_err("duplicate is refused");
        assert_eq!(
            err,
            ApplyError::DuplicateRole { name: "alice".into() }
        );
        assert_eq!(err.pg(), ("42710", "role \"alice\" already exists".into()));
        // The refusal mutated nothing: still exactly one role, and the
        // affinity half untouched.
        assert_eq!(state.current.roles.iter().collect::<Vec<_>>(), ["alice"]);
        assert!(state.current.databases.is_empty());

        // Roles and affinity rows coexist; each stays in its own half.
        state
            .apply(&TierCChange::CreateRole { name: "bob".into() })
            .expect("second role applies");
        state
            .apply(&TierCChange::SetDatabaseAffinity {
                name: "dev".into(),
                affinity: NumaAffinity::Single,
            })
            .expect("affinity row applies");
        assert_eq!(
            state.current.roles.iter().collect::<Vec<_>>(),
            ["alice", "bob"]
        );
        assert_eq!(state.current.databases.len(), 1);
        // Sorted, for stable wire output.
        assert_eq!(state.publish().roles().collect::<Vec<_>>(), ["alice", "bob"]);
    }

    /// T5.5's apply contract, on the writer's own state: CreateDatabase
    /// records its row (explicit affinity, or the frozen cluster default
    /// for the bare form), `Single` binds per A13, and a duplicate is
    /// REFUSED with 42P04 leaving the state exactly as it was — the
    /// partial-failure property, pinned at the state layer like T5.4's
    /// duplicate-role test.
    #[test]
    fn create_database_records_the_row_and_refuses_duplicates_without_mutation() {
        let mut state = ClusterState::from_boot(
            ClusterSnapshot::build(NumaAffinity::Single, &[7, 7, 3]),
            vec![7, 7, 3],
        );
        // A13's proxy: the layout is one entry per worker — two workers
        // on numa 7, one on numa 3 — so a created `single` DB binds
        // THERE (fewest workers), not to the config-order-first numa.
        state
            .apply(&TierCChange::CreateDatabase {
                name: "oltp".into(),
                affinity: Some(NumaAffinity::Single),
            })
            .expect("single create applies");
        assert_eq!(
            state.current.databases.get("oltp"),
            Some(&DbRow { affinity: NumaAffinity::Single, bound_numa: Some(3) })
        );

        // Bare form: the cluster default (Single here) frozen at
        // creation, bound per the same proxy.
        state
            .apply(&TierCChange::CreateDatabase { name: "plain".into(), affinity: None })
            .expect("bare create applies");
        assert_eq!(
            state.current.databases.get("plain"),
            Some(&DbRow { affinity: NumaAffinity::Single, bound_numa: Some(3) })
        );

        // `cross` records no binding at all.
        state
            .apply(&TierCChange::CreateDatabase {
                name: "bulk".into(),
                affinity: Some(NumaAffinity::Cross),
            })
            .expect("cross create applies");
        assert_eq!(
            state.current.databases.get("bulk"),
            Some(&DbRow { affinity: NumaAffinity::Cross, bound_numa: None })
        );

        // The duplicate is refused — 42P04, PG's wording — and the
        // refusal mutated nothing: three rows, the affinity half
        // untouched, roles untouched.
        let err = state
            .apply(&TierCChange::CreateDatabase {
                name: "oltp".into(),
                affinity: Some(NumaAffinity::Cross),
            })
            .expect_err("duplicate is refused");
        assert_eq!(err, ApplyError::DuplicateDatabase { name: "oltp".into() });
        assert_eq!(
            err.pg(),
            ("42P04", "database \"oltp\" already exists".into())
        );
        assert_eq!(state.current.databases.len(), 3);
        assert_eq!(
            state.current.databases.get("oltp").map(|r| r.affinity),
            Some(NumaAffinity::Single),
            "the refused CROSS re-create did not overwrite the row"
        );
        assert!(state.current.roles.is_empty());

        // The snapshot view: sorted names, the text spellings, the
        // bindings — what debug_databases serves.
        assert_eq!(
            state.publish().databases().collect::<Vec<_>>(),
            [
                ("bulk", "cross", None),
                ("oltp", "single", Some(3)),
                ("plain", "single", Some(3)),
            ]
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
        let catalog = Arc::new(Catalog::new(snap(Some(0))));
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
                        let placement = catalog
                            .load()
                            .as_ref()
                            .expect("published")
                            .resolve(Some("probe"));
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
                    let final_placement = catalog
                        .load()
                        .as_ref()
                        .expect("published")
                        .resolve(Some("probe"));
                    assert_eq!(final_placement.bound_numa, Some(WRITES - 1));
                    (distinct, last)
                })
            })
            .collect();

        barrier.wait();
        for rev in 0..WRITES {
            catalog.store(Arc::new(snap(Some(rev))));
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
        let old = catalog.load_full().expect("published");
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
        let pinned = Arc::clone(guard.as_ref().expect("published"));
        let weak = Arc::downgrade(&pinned);

        // The writer replaces twice while the read is "in flight".
        catalog.store(Arc::new(ClusterSnapshot::build(NumaAffinity::Cross, &[0, 1])));
        catalog.store(Arc::new(ClusterSnapshot::build(NumaAffinity::Single, &[1, 1])));

        assert_eq!(
            guard.as_ref().expect("published").resolve(None).bound_numa,
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
            catalog
                .load()
                .as_ref()
                .expect("published")
                .resolve(None)
                .bound_numa,
            Some(1),
            "fresh reads see the latest publication"
        );
    }

    /// Boots a worker ring on `core`: connects its write init (blocking
    /// until the writer's forwarder side connects — the T4.5 lattice),
    /// connects its publication receiver, optionally holds the
    /// publication loop shut behind `gate` (the instrumented stall),
    /// then drains publication jobs and runs the test body. The stall
    /// leaves queued store jobs un-drained — R11's laggard shape,
    /// controlled, not a sleep guess.
    fn spawn_worker<B, F>(
        core: usize,
        done: Arc<AtomicBool>,
        write_init: SubmitInit,
        publish_rx: SharedReceiver<SubmitJob>,
        publish_stats: Arc<SubmitStats>,
        gate: Option<Arc<AtomicBool>>,
        body: B,
    ) -> glommio::ExecutorJoinHandle<()>
    where
        B: FnOnce(SubmitHandle) -> F + Send + 'static,
        F: Future<Output = ()> + 'static,
    {
        LocalExecutorBuilder::new(glommio::Placement::Fixed(core))
            .name("tierc-worker")
            .spawn(move || async move {
                let handle = write_init.connect().await;
                let publish = publish_rx.connect().await;
                if let Some(gate) = gate {
                    while !gate.load(Ordering::SeqCst) {
                        glommio::timer::sleep(Duration::from_millis(1)).await;
                    }
                }
                glommio::spawn_local(submit::submit_loop(publish, publish_stats)).detach();
                body(handle).await;
                // The reactor's loops — and its catalog — live in THIS
                // task: hold the ring open until the test is done (the
                // real worker holds it for the server's lifetime).
                while !done.load(Ordering::SeqCst) {
                    glommio::timer::sleep(Duration::from_millis(2)).await;
                }
            })
            .expect("tierc worker executor spawns")
    }

    /// A worker body that submits one `SetDatabaseAffinity` change for
    /// `db` through its write channel and reports the ack (with the
    /// wall time the round trip took) to the test thread. The submit's
    /// delivery `Result` and `submit_change`'s own flatten into one:
    /// either the ack value or `Lost`.
    fn change_body(
        core: u32,
        db: &'static str,
        tx: std::sync::mpsc::Sender<(Result<TierCAck, SubmitError>, Duration)>,
    ) -> impl FnOnce(SubmitHandle) -> Pin<Box<dyn Future<Output = ()> + 'static>> {
        move |handle: SubmitHandle| {
            let tx = tx;
            Box::pin(async move {
                let change = TierCChange::SetDatabaseAffinity {
                    name: db.to_string(),
                    affinity: NumaAffinity::Cross,
                };
                let started = Instant::now();
                let result = match handle
                    .submit_to(move || async move { submit_change(core, change).await })
                    .await
                {
                    Ok(inner) => inner,
                    Err(_) => Err(SubmitError::Lost),
                };
                let _ = tx.send((result, started.elapsed()));
            })
        }
    }

    /// Two distinct worker cores (the unbound housekeeper runs on the
    /// test thread).
    fn cores() -> (usize, usize) {
        let allowed = crate::pin::current_affinity();
        assert!(allowed.len() >= 3, "needs 3 CPUs: housekeeper + 2 workers");
        (allowed[1] as usize, allowed[2] as usize)
    }

    /// Bounded sync poll for lazy adoption on the test thread (1 ms
    /// grain, `budget_ms` ceiling) — a real wait on real work, not a
    /// sleep guess.
    fn poll_until(mut pred: impl FnMut() -> bool, budget_ms: u64) {
        for _ in 0..budget_ms {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(pred(), "lazy adoption did not land within {budget_ms} ms");
    }

    /// Deterministic fixture teardown: flip the hold-open flag, then
    /// join the housekeeper and worker threads. Dropping at test end
    /// (even on panic) keeps the next serialized test from overlapping
    /// any pinned executor.
    struct FixtureGuard {
        done: Arc<AtomicBool>,
        hk: Option<std::thread::JoinHandle<()>>,
        workers: Vec<glommio::ExecutorJoinHandle<()>>,
    }

    impl Drop for FixtureGuard {
        fn drop(&mut self) {
            self.done.store(true, Ordering::SeqCst);
            if let Some(hk) = self.hk.take() {
                let _ = hk.join();
            }
            for w in self.workers.drain(..) {
                let _ = w.join();
            }
        }
    }

    /// The shared writer fixture: two worker rings plus a housekeeper
    /// ring on its own thread (held open for the test's lifetime — the
    /// writer and its forwarders die with the executor, exactly like
    /// Router::serve holds them for the server's lifetime), full
    /// channel wiring in boot's shape (write inits → workers, write
    /// receivers → the plane; publication inits → the plane,
    /// publication receivers → workers), returned once the boot
    /// publication has landed. Gated workers skip the fixture's
    /// boot-publication assert (their initial store is still queued —
    /// the tests assert that emptiness themselves). Workers are
    /// spawned BEFORE the writer runs — their connects block until the
    /// writer's sides connect.
    #[allow(clippy::type_complexity)]
    fn writer_fixture<B0, F0, B1, F1>(
        w0_core: usize,
        w1_core: usize,
        gate0: Option<Arc<AtomicBool>>,
        gate1: Option<Arc<AtomicBool>>,
        body0: B0,
        body1: B1,
    ) -> (
        Arc<Catalog>,
        Arc<Catalog>,
        Arc<Catalog>,
        FixtureGuard,
    )
    where
        B0: FnOnce(SubmitHandle) -> F0 + Send + 'static,
        F0: Future<Output = ()> + 'static,
        B1: FnOnce(SubmitHandle) -> F1 + Send + 'static,
        F1: Future<Output = ()> + 'static,
    {
        let hk_catalog = Arc::new(Catalog::empty());
        let cat0 = Arc::new(Catalog::empty());
        let cat1 = Arc::new(Catalog::empty());

        // Write channels: inits → workers (their submit_to handles),
        // receivers → the housekeeper's forwarders. Publication
        // channels: inits → the writer (fan-out handles), receivers →
        // the workers.
        let (w0_init, wrx0) = submit::channel(TIERC_WRITE_CAP);
        let (w1_init, wrx1) = submit::channel(TIERC_WRITE_CAP);
        let wstats0 = w0_init.stats();
        let wstats1 = w1_init.stats();
        let (p0_init, prx0) = submit::channel(TIERC_PUBLISH_CAP);
        let (p1_init, prx1) = submit::channel(TIERC_PUBLISH_CAP);
        let pstats0 = p0_init.stats();
        let pstats1 = p1_init.stats();

        let done = Arc::new(AtomicBool::new(false));
        let join0 = spawn_worker(
            w0_core,
            Arc::clone(&done),
            w0_init,
            prx0,
            pstats0,
            gate0.clone(),
            body0,
        );
        let join1 = spawn_worker(
            w1_core,
            Arc::clone(&done),
            w1_init,
            prx1,
            pstats1,
            gate1.clone(),
            body1,
        );

        let plane = TierCPlane {
            catalog: Arc::clone(&hk_catalog),
            worker_catalogs: vec![Arc::clone(&cat0), Arc::clone(&cat1)],
            worker_cores: vec![w0_core as u32, w1_core as u32],
            write_receivers: vec![wrx0, wrx1],
            write_stats: vec![wstats0, wstats1],
            publish_inits: vec![p0_init, p1_init],
            fanout_wait: Duration::from_millis(400),
            worker_numas: vec![0, 0],
        };
        let initial = ClusterSnapshot::build(NumaAffinity::Single, &[0, 0]);
        let thread_done = Arc::clone(&done);
        let hk = std::thread::Builder::new()
            .name("tierc-hk".to_string())
            .spawn(move || {
                let hk = LocalExecutorBuilder::new(glommio::Placement::Unbound)
                    .name("tierc-hk-ring")
                    .make()
                    .expect("housekeeper executor");
                hk.run(async move {
                    let published = start_writer(plane, initial).await;
                    published.await.expect("boot publication fires");
                    // Hold the ring (and so the writer and its
                    // forwarders) open until the test is done — the
                    // serving housekeeper does this with its accept
                    // loop.
                    while !thread_done.load(Ordering::SeqCst) {
                        glommio::timer::sleep(Duration::from_millis(2)).await;
                    }
                });
            })
            .expect("housekeeper thread spawns");

        // The boot-order property, multi-reactor (FR-6): publication
        // landed before the writer's caller could proceed to serve —
        // here, before the fixture returns. The hk store is synchronous
        // in writer_loop; the workers' stores are fan-out jobs on their
        // own rings, so each ungated catalog is POLLED for, not asserted
        // synchronously (the fan-out is asynchronous by design). A gated
        // worker's store is still queued (R11's budget bounded the boot
        // wait); the test that gated it asserts that emptiness itself.
        let gates = [gate0.is_none(), gate1.is_none()];
        poll_until(
            || hk_catalog.load_full().is_some(),
            5_000,
        );
        for (name, catalog, expect) in [
            ("hk", &hk_catalog, true),
            ("w0", &cat0, gates[0]),
            ("w1", &cat1, gates[1]),
        ] {
            if !expect {
                continue;
            }
            poll_until(
                || catalog.load_full().is_some(),
                5_000,
            );
            let placement = catalog
                .load()
                .as_ref()
                .unwrap_or_else(|| panic!("{name} never received the boot publication"))
                .resolve(None);
            assert_eq!(
                placement,
                Placement {
                    affinity: NumaAffinity::Single,
                    bound_numa: Some(0)
                },
                "{name}'s initial snapshot is the cluster default"
            );
        }
        let guard = FixtureGuard {
            done,
            hk: Some(hk),
            workers: vec![join0, join1],
        };
        (hk_catalog, cat0, cat1, guard)
    }

    /// FR-29/FR-30's happy path across three real reactors: both
    /// workers `submit_change` through their write channels, the
    /// serialized writer applies each, and after `Applied` acks every
    /// catalog — both workers' and the housekeeper's — resolves the new
    /// rows, scoped to their databases (other names inherit the default,
    /// FR-10).
    #[test]
    fn writer_change_visible_on_all_reactors_and_acked_applied() {
        let _serial = writer_serial();
        let (w0_core, w1_core) = cores();
        let (ack_tx0, ack_rx0) = std::sync::mpsc::channel();
        let (ack_tx1, ack_rx1) = std::sync::mpsc::channel();
        let (hk_catalog, cat0, cat1, _guard) = writer_fixture(
            w0_core,
            w1_core,
            None,
            None,
            change_body(w0_core as u32, "dev", ack_tx0),
            change_body(w1_core as u32, "beta", ack_tx1),
        );

        let (ack0, dur0) = ack_rx0
            .recv_timeout(Duration::from_secs(5))
            .expect("w0's change acks");
        let ack0 = ack0.expect("w0's change round-trips the writer");
        let (ack1, dur1) = ack_rx1
            .recv_timeout(Duration::from_secs(5))
            .expect("w1's change acks");
        let ack1 = ack1.expect("w1's change round-trips the writer");
        assert_eq!(
            ack0,
            TierCAck::Applied,
            "unstalled fan-out acks Applied ({dur0:?})"
        );
        assert_eq!(
            ack1,
            TierCAck::Applied,
            "unstalled fan-out acks Applied ({dur1:?})"
        );
        for (name, catalog) in [("hk", &hk_catalog), ("w0", &cat0), ("w1", &cat1)] {
            let guard = catalog.load();
            let snap = guard.as_ref().expect("published");
            assert_eq!(
                snap.resolve(Some("dev")).affinity,
                NumaAffinity::Cross,
                "{name} sees the applied row"
            );
            assert_eq!(
                snap.resolve(Some("beta")).affinity,
                NumaAffinity::Cross,
                "{name} sees the second applied row"
            );
            assert_eq!(
                snap.resolve(Some("other")),
                Placement {
                    affinity: NumaAffinity::Single,
                    bound_numa: Some(0)
                },
                "{name}: rows are scoped to their database; others inherit the default"
            );
        }
    }

    /// R11's bounded wait against a genuinely stalled reactor — the
    /// stall is instrumented (a gated publication loop, not a sleep
    /// guess): the issuer's store confirms (it is awaited first), the
    /// laggard's queued store costs the writer the full budget, the ack
    /// is `Pending(1)` with a lower-bound wait, the gate's opening is
    /// the lazy adoption, and a post-stall change acks `Applied`.
    #[test]
    fn stalled_reactor_acks_pending_and_adopts_lazily() {
        let _serial = writer_serial();
        let (w0_core, w1_core) = cores();
        let gate1 = Arc::new(AtomicBool::new(false));
        let (ack_tx0, ack_rx0) = std::sync::mpsc::channel();
        let (ack_tx1, ack_rx1) = std::sync::mpsc::channel();
        // w0 (unstalled) issues "dev"; w1's publication loop is gated
        // shut — its body (and so its "beta" submission) runs only after
        // the gate opens.
        let (hk_catalog, cat0, cat1, _guard) = writer_fixture(
            w0_core,
            w1_core,
            None,
            Some(Arc::clone(&gate1)),
            change_body(w0_core as u32, "dev", ack_tx0),
            change_body(w1_core as u32, "beta", ack_tx1),
        );

        // The stalled fan-out: Pending(1), and the wait really cost the
        // budget (lower bound, generous margin — no upper-bound flake).
        let (ack0, dur0) = ack_rx0
            .recv_timeout(Duration::from_secs(5))
            .expect("the change acks");
        let ack0 = ack0.expect("the change round-trips the writer");
        assert_eq!(
            ack0,
            TierCAck::Pending(1),
            "one laggard ⇒ pending one, even though the boot store queued too"
        );
        assert!(
            dur0 >= Duration::from_millis(350),
            "the bounded wait was actually bounded by the budget, not lucky timing ({dur0:?})"
        );
        // Issuer-first: the issuer's own store confirmed inside the
        // budget and its catalog serves the change at ack time.
        assert_eq!(
            cat0.load().as_ref().expect("published").resolve(Some("dev")).affinity,
            NumaAffinity::Cross,
            "the issuing reactor stores before the ack"
        );
        assert_eq!(
            hk_catalog.load().as_ref().expect("published").resolve(Some("dev")).affinity,
            NumaAffinity::Cross,
            "the writer's own catalog stored before the fan-out"
        );
        // The laggard has NOTHING yet: its initial store AND the change
        // store sit queued in its gated publication channel.
        assert!(
            cat1.load_full().is_none(),
            "the stalled reactor's publications are still queued, not applied"
        );

        // The gate's opening IS the lazy adoption: the queued stores
        // drain in order and the change becomes visible.
        gate1.store(true, Ordering::SeqCst);
        poll_until(
            || {
                cat1.load()
                    .as_ref()
                    .map(|snap| snap.resolve(Some("dev")).affinity)
                    == Some(NumaAffinity::Cross)
            },
            2_000,
        );

        // And the writer is alive after a pending fan-out: the
        // post-stall change acks Applied and lands everywhere.
        let (ack1, _dur1) = ack_rx1
            .recv_timeout(Duration::from_secs(5))
            .expect("the post-stall change acks");
        let ack1 = ack1.expect("the post-stall change round-trips");
        assert_eq!(
            ack1,
            TierCAck::Applied,
            "the writer serves after a pending fan-out"
        );
        for (name, catalog) in [("hk", &hk_catalog), ("w0", &cat0), ("w1", &cat1)] {
            assert_eq!(
                catalog
                    .load()
                    .as_ref()
                    .expect("published")
                    .resolve(Some("beta"))
                    .affinity,
                NumaAffinity::Cross,
                "{name} adopted the post-stall change"
            );
        }
    }

    /// R11's issuer-first ordering, discriminated: the ISSUER is the
    /// second worker while the first is gated. Awaited in natural
    /// order, the writer would burn the whole budget on the stalled
    /// non-issuer and the ack would be `Pending(2)`. Awaited issuer-
    /// first, the issuing store confirms first: `Pending(1)`, and the
    /// issuer's catalog serves the change at ack time.
    #[test]
    fn issuers_store_is_awaited_first_and_confirmed_within_the_budget() {
        let _serial = writer_serial();
        let (w0_core, w1_core) = cores();
        let gate0 = Arc::new(AtomicBool::new(false));
        let (ack_tx1, ack_rx1) = std::sync::mpsc::channel();
        // w0: gated and issueless — it only participates in fan-outs.
        let (_hk, cat0, cat1, _guard) = writer_fixture(
            w0_core,
            w1_core,
            Some(Arc::clone(&gate0)),
            None,
            |_: SubmitHandle| async move {},
            change_body(w1_core as u32, "dev", ack_tx1),
        );

        let (ack, dur) = ack_rx1
            .recv_timeout(Duration::from_secs(5))
            .expect("the change acks");
        let ack = ack.expect("the change round-trips the writer");
        assert_eq!(
            ack,
            TierCAck::Pending(1),
            "the issuer confirmed (1 laggard), not Pending(2) — issuer-first ordering"
        );
        assert!(
            dur >= Duration::from_millis(350),
            "the budget was spent waiting on the laggard ({dur:?})"
        );
        assert_eq!(
            cat1.load().as_ref().expect("published").resolve(Some("dev")).affinity,
            NumaAffinity::Cross,
            "the issuing reactor stores before the ack"
        );
        assert!(
            cat0.load_full().is_none(),
            "the gated non-issuer is still queued"
        );

        gate0.store(true, Ordering::SeqCst);
        poll_until(
            || {
                cat0.load()
                    .as_ref()
                    .map(|snap| snap.resolve(Some("dev")).affinity)
                    == Some(NumaAffinity::Cross)
            },
            2_000,
        );
    }

    /// FR-6's gate, both sides, over a real TCP pair: an unpublished
    /// catalog refuses the handoff with a wire `57P03` (cannot_connect_
    /// now) and closes; a published catalog passes the handoff through
    /// untouched.
    #[test]
    fn unpublished_catalog_refuses_handoff_with_57p03_and_published_passes() {
        // The refusing side.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let empty = Catalog::empty();
        let handoff = Handoff {
            fd: server.into_raw_fd(),
            peer: addr,
            user: "probe".to_string(),
            database: Some("dev".to_string()),
        };
        assert!(
            gate_handoff(handoff, &empty).is_err(),
            "an unpublished catalog refuses traffic"
        );
        let mut buf = [0u8; 256];
        let n = (&client).read(&mut buf).expect("the refusal reaches the wire");
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        assert!(
            text.contains("57P03"),
            "the refusal carries cannot_connect_now, got: {text}"
        );
        drop(client);

        // The passing side: published catalog, handoff through intact.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let published = Catalog::new(ClusterSnapshot::build(NumaAffinity::Single, &[0, 0]));
        let handoff = Handoff {
            fd: server.into_raw_fd(),
            peer: addr,
            user: "probe".to_string(),
            database: Some("dev".to_string()),
        };
        let passed = gate_handoff(handoff, &published).expect("a published catalog serves");
        // The Handoff owns the fd now (it has no Drop; the test stands
        // in for session::run and closes it).
        drop(unsafe { TcpStream::from_raw_fd(passed.fd) });
        drop(client);
    }
}

