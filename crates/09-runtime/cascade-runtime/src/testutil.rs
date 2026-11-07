//! Test-support utilities. Integration-test surface only — not a
//! production API, and inert unless a test arms it.
//!
//! The suite watchdog exists because of the environmental hazard this
//! development environment carries (WSL2 5.15 + glommio 0.9 lost io_uring
//! completions): the session-level stall rule (`stall_reap_ms`) bounds a
//! wedged statement on a live ring, but a ring whose PARK wakeup is lost
//! freezes every task on it — the session, that ring's reap loop, even
//! the tick loop feeding reap signals — and nothing running on the ring
//! can contain that. Observed as a test binary that hangs forever (the
//! eternal-futex class, ~1 in 20 full suites): the client sits in a
//! drained `read`, the server threads sit in `io_uring_enter`, and
//! libtest never finishes.
//!
//! [`arm_suite_watchdog`] converts that into a fast, loud failure: a
//! thread that, at the budget, dumps the hang diagnostics ([`on_hang`]
//! providers — tick counters, reap counters, connection gauges — the
//! numbers that name the frozen ring) plus a `/proc` thread snapshot, and
//! aborts the process. Idempotent per binary; arm it in every fixture
//! that boots a runtime.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, Once,
};
use std::time::Duration;

/// Default whole-binary budget: the slowest suite runs in well under 10 s
/// even loaded, so 90 s is ~10× headroom while staying far under the
/// historical 14-minute wedge. `CASCADE_TEST_WATCHDOG_SECS` overrides.
const DEFAULT_BUDGET_SECS: u64 = 90;

type Provider = Box<dyn Fn() -> String + Send + Sync>;

static ARM: Once = Once::new();
static PROVIDERS: Mutex<Vec<(String, Provider)>> = Mutex::new(Vec::new());

/// Registers a diagnostics provider the watchdog prints when it fires.
/// A fixture boots its runtime, then hands in closures over the counters
/// that distinguish the wedge classes: `orchestrator().ticks()` advancing
/// past the hang point rules the housekeeper ring alive; `reap_stats()
/// .reaped() == 0` with a stuck session rules the worker ring alive (the
/// tick never condemned anything); per-worker connection gauges show
/// whether the session ever started. Labels are free-form.
pub fn on_hang(label: String, provider: impl Fn() -> String + Send + Sync + 'static) {
    PROVIDERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((label, Box::new(provider)));
}

/// Arms the per-binary hang watchdog (idempotent — call it in every
/// fixture that boots a runtime; only the first call spawns the thread).
/// See the module doc for why this exists.
pub fn arm_suite_watchdog() {
    ARM.call_once(|| {
        let secs = std::env::var("CASCADE_TEST_WATCHDOG_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_BUDGET_SECS);
        let budget = Duration::from_secs(secs);
        std::thread::Builder::new()
            .name("suite-watchdog".into())
            .spawn(move || {
                std::thread::sleep(budget);
                // Fire. The process is wedged; nothing on the rings can
                // recover it (that is the point). Dump everything that
                // names the wedge, then abort — a loud red beats a silent
                // hang, and cargo treats the signal-death as the failure.
                // The dump goes to a side file as well as stderr: the
                // abort must never depend on stderr reaching anyone.
                let mut dump = String::new();
                dump.push_str(&format!(
                    "=== SUITE WATCHDOG: {budget:?} budget exceeded ===\n"
                ));
                for (label, provider) in PROVIDERS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                {
                    dump.push_str(&format!("--- {label} ---\n{}\n", provider()));
                }
                let tasks = std::fs::read_dir("/proc/self/task")
                    .map(|d| d.flatten().collect::<Vec<_>>())
                    .unwrap_or_default();
                for t in tasks {
                    let tid = t.file_name().to_string_lossy().into_owned();
                    let comm = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
                        .unwrap_or_default();
                    let syscall =
                        std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall"))
                            .unwrap_or_default();
                    dump.push_str(&format!(
                        "TID {} [{}] syscall={}\n",
                        tid,
                        comm.trim(),
                        syscall.trim()
                    ));
                }
                dump.push_str("=== SUITE WATCHDOG: aborting ===\n");
                let side = std::env::temp_dir().join(format!(
                    "cascade-watchdog-{}.log",
                    std::process::id()
                ));
                let _ = std::fs::write(&side, &dump);
                eprint!("{dump}");
                std::process::abort();
            })
            .expect("suite watchdog thread spawns");
    });
}

// ---- The H2 discriminator's deterministic park ----

/// Set by a test to hold the Tier C writer: a received change parks
/// received-but-unapplied — its session's gauge is up, its ack unsent —
/// until released. The shutdown-drain test arms the hold BEFORE the DDL
/// is even sent, so the change is provably mid-bounce when shutdown
/// begins; `writer_loop` polls this once per DDL (one relaxed load —
/// DDLs are rare and the park loop runs only while a test holds).
static WRITER_PRE_APPLY_HOLD: AtomicBool = AtomicBool::new(false);

/// Arms (`true`) or releases (`false`) the writer hold. See
/// [`WRITER_PRE_APPLY_HOLD`].
pub fn hold_writer_before_apply(hold: bool) {
    WRITER_PRE_APPLY_HOLD.store(hold, Ordering::Relaxed);
}

/// The writer-side read. `pub(crate)` — production code consults it,
/// only tests set it.
pub(crate) fn writer_held_before_apply() -> bool {
    WRITER_PRE_APPLY_HOLD.load(Ordering::Relaxed)
}
