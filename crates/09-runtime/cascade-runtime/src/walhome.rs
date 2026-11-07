//! WAL home-NUMA placement (T2.6; §2.3 rows, §8.2; A14, FR-27).
//!
//! Per-DB writers are homed per affinity: a `single` database's writer
//! lives on the Tier C bound NUMA (the A13 pick — the writers sit with the
//! only workers allowed to touch the DB, I13); a `cross` database's writer
//! lives on a *designated* NUMA chosen at creation and **persisted** (A14
//! — `pg_database.datnumanode`): reboots re-read the choice, they never
//! re-derive it, or a config change would silently migrate a database's
//! WAL stream across nodes.
//!
//! This slice stands in for the catalog: on first boot the dev database's
//! home is derived deterministically (`db_hash(DatabaseId) mod
//! num_numas` — same frozen-FNV discipline as the T2.5 page placement),
//! then written to `wal_homes.toml` under the WAL directory; every later
//! boot re-reads that file and honors it even where re-derivation would
//! now pick differently. The home writer itself is constructed on its
//! home NUMA (first-touch placement of its state), and every reactor's
//! sessions append through that one shared writer — FR-27's
//! durable-before-ack slice from T1.11, now placed.
//!
//! Honest scope: one database (dev, [`DatabaseId`](cascade_types::DatabaseId)
//! 0) and one writer exist today; the per-DB writer fan-out arrives with
//! the real engine (the table, persistence format, and derivation are
//! already per-DB so the fan-out is a boot-loop change, not a redesign).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cascade_types::DatabaseId;

use crate::pin;
use crate::pools::PoolRegistry;

/// Frozen placement hash for a database id: FNV-1a 64 over the
/// little-endian `DatabaseId`. The algorithm is frozen like the T2.5 page
/// hash — a database's derived home must not move across builds or
/// restarts (golden literals in the unit tests pin the contract).
pub fn db_hash(db: DatabaseId) -> u64 {
    fn mix(h: &mut u64, bytes: &[u8]) {
        for b in bytes {
            *h ^= u64::from(*b);
            *h = h.wrapping_mul(0x0100_0000_01b3); // 0x100000001b3, the FNV-1a 64 prime
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    mix(&mut h, &db.0.to_le_bytes());
    h
}

/// One database's WAL home: the NUMA whose writer owns its WAL stream
/// (A14), plus the first-touch probe of where that writer was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalHome {
    /// The home NUMA.
    pub numa: u32,
    /// Core the home writer's constructor ran on. `None` = boot-thread
    /// fallback (the home NUMA had no pinnable core): placement is then
    /// first-touch best-effort, honestly reported — same discipline as the
    /// pools' `node_bound()`.
    pub constructed_on: Option<u32>,
}

/// The homing table: `DatabaseId → WalHome` (§2.3's per-DB row set, as of
/// boot). Built by boot before workers spawn; immutable after.
#[derive(Debug, Default)]
pub struct WalHomeTable {
    homes: HashMap<u32, WalHome>,
}

impl WalHomeTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a database's home (boot-time construction).
    pub fn set(&mut self, db: DatabaseId, home: WalHome) {
        self.homes.insert(db.0, home);
    }

    /// A database's home, when one exists.
    pub fn home(&self, db: DatabaseId) -> Option<WalHome> {
        self.homes.get(&db.0).copied()
    }
}

/// A14's first-creation pick for a `cross` database: `db_hash(DatabaseId)
/// mod num_numas` over the pool registry (config order), mapped to the
/// chosen pool's NUMA. Deterministic — the same database id derives the
/// same home on any boot with the same layout; persistence (A14) is what
/// keeps it stable across *changed* layouts. `None` for an empty registry
/// (validated configs never produce one).
///
/// Caveat (M4, post-M5 review): the registry holds every configured
/// pool, including worker-less ones — a pick can name a numa with no
/// reactor. Boot derives over the DESIGNATED set instead
/// ([`derive_wal_home_from_numas`]); this form stays for registry-side
/// callers that already know their numas are reactor-backed.
pub fn derive_wal_home(db: DatabaseId, pools: &PoolRegistry) -> Option<u32> {
    derive_wal_home_from_numas(db, &pools.iter().map(|p| p.numa()).collect::<Vec<_>>())
}

/// The registry-free form of [`derive_wal_home`]: same hash, same order,
/// over bare NUMA numbers. Boot needs this BEFORE the pool registry
/// exists — the T4.3 recycler's target (`db` → home NUMA) must be
/// resolved before the housekeepers spawn, because resolution reads the
/// persisted-homes file and the housekeeper must never touch I/O at tick
/// time (the syscall-layer no-I/O tests would catch it).
///
/// The list must be the DESIGNATED numas — one entry per numa that has a
/// worker (and so a reactor that can flush the tail and stamp the clean
/// marker), not every pool numa (M4, post-M5 review): a home derived
/// onto a worker-less pool numa has no designated reactor, the drain
/// finds no flagged sender, and the marker is refused every shutdown —
/// every restart would replay as a crash. Boot passes the designated
/// set; the registry form above cannot know it, so it documents the
/// burden.
pub fn derive_wal_home_from_numas(db: DatabaseId, designated_numas: &[u32]) -> Option<u32> {
    if designated_numas.is_empty() {
        return None;
    }
    let idx = (db_hash(db) % designated_numas.len() as u64) as usize;
    designated_numas.get(idx).copied()
}

/// The per-DB WAL home decision (T2.6, A14) in ONE place: `single`
/// follows the Tier C bound NUMA; `cross` honors the persisted choice
/// when it is still a DESIGNATED numa — one a reactor lives on, the only
/// place the tail can flush and the marker can stamp (else warns and
/// re-derives) — falling back to derivation on first boot. Boot resolves
/// each DB's home once, early — the recycler's target and the writer
/// construction both consume the same value, so the two can never drift.
pub fn resolve_wal_home(
    db: DatabaseId,
    placement: &crate::tierc::Placement,
    persisted: &HashMap<u32, u32>,
    designated_numas: &[u32],
) -> u32 {
    let derived = derive_wal_home_from_numas(db, designated_numas)
        .expect("a routed boot always has designated reactors (worker_cores is validated non-empty)");
    match (placement.affinity, placement.bound_numa) {
        (crate::config::NumaAffinity::Single, Some(numa)) => numa,
        // `single` with no bound NUMA means no workers (config validation
        // forbids it) — fall back to derivation rather than panic.
        (crate::config::NumaAffinity::Single, None) => derived,
        (crate::config::NumaAffinity::Cross, _) => match persisted.get(&db.0).copied() {
            Some(numa) if designated_numas.contains(&numa) => numa,
            Some(numa) => {
                cascade_telemetry::log(
                    cascade_telemetry::Level::Warn,
                    "wal_home_persisted_invalid",
                    &[
                        ("db", db.0.to_string().as_str()),
                        ("numa", numa.to_string().as_str()),
                        ("policy", "re-deriving (NUMA no longer designated)"),
                    ],
                );
                derived
            }
            None => derived,
        },
    }
}

/// Constructs the home writer on its home NUMA: a pinned constructor
/// thread first-touches the writer's state on a core of that node (a
/// worker core of the NUMA in config order, else any online core the
/// topology places there). `start` seeds the writer's watermarks — the
/// replayed log end on a recovery boot (T3.3), 0 otherwise. Returns the
/// writer and the core it was constructed on (`None` = every fallback
/// fired and the writer was built on the boot thread — reported, not
/// hidden).
pub fn construct_writer_on(
    cfg: &crate::config::RuntimeConfig,
    online: &[u32],
    topology: &dyn crate::topology::TopologyProvider,
    numa: u32,
    start: cascade_types::Lsn,
) -> (std::sync::Arc<cascade_wal::WalWriter>, Option<u32>) {
    let core = cfg
        .worker_cores
        .iter()
        .find(|w| w.numa == numa)
        .and_then(|w| w.cores.first().copied())
        .or_else(|| {
            online
                .iter()
                .copied()
                .find(|&c| topology.numa_node(c) == Some(numa))
        });
    let Some(core) = core else {
        cascade_telemetry::log(
            cascade_telemetry::Level::Warn,
            "wal_home_first_touch_fallback",
            &[
                ("numa", numa.to_string().as_str()),
                ("reason", "no pinnable core on the home NUMA"),
            ],
        );
        return (std::sync::Arc::new(cascade_wal::WalWriter::new_at(start)), None);
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("cascade-walhome-{numa}"))
        .spawn(move || {
            let pinned = pin::pin_current(core)
                .ok()
                .and_then(|seen| seen.first().copied());
            // The writer is built *inside* the pinned thread: its (small)
            // state first-touches on the home NUMA by construction.
            let writer = std::sync::Arc::new(cascade_wal::WalWriter::new_at(start));
            let _ = tx.send((pinned, writer));
        });
    match spawned {
        Ok(_) => match rx.recv() {
            Ok((Some(core), writer)) => (writer, Some(core)),
            Ok((None, writer)) => {
                cascade_telemetry::log(
                    cascade_telemetry::Level::Warn,
                    "wal_home_first_touch_fallback",
                    &[
                        ("numa", numa.to_string().as_str()),
                        ("reason", "pin verification failed; placement unverified"),
                    ],
                );
                (writer, None)
            }
            Err(_) => {
                // Constructor thread died before reporting — build on the
                // boot thread rather than refuse boot over placement.
                cascade_telemetry::log(
                    cascade_telemetry::Level::Warn,
                    "wal_home_first_touch_fallback",
                    &[
                        ("numa", numa.to_string().as_str()),
                        ("reason", "constructor thread died; boot-thread placement"),
                    ],
                );
                (std::sync::Arc::new(cascade_wal::WalWriter::new_at(start)), None)
            }
        },
        Err(e) => {
            cascade_telemetry::log(
                cascade_telemetry::Level::Warn,
                "wal_home_first_touch_fallback",
                &[
                    ("numa", numa.to_string().as_str()),
                    ("reason", e.to_string().as_str()),
                ],
            );
            (std::sync::Arc::new(cascade_wal::WalWriter::new_at(start)), None)
        }
    }
}

// ---- A14 persistence (the debug catalog stand-in) ----

/// Where the home choices persist under the WAL directory. TOML, one row
/// per database — the stand-in for `pg_database.datnumanode`.
pub fn homes_path(wal_dir: &Path) -> PathBuf {
    wal_dir.join("wal_homes.toml")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct HomesDoc {
    #[serde(default)]
    db: Vec<DbHome>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DbHome {
    id: u32,
    numa: u32,
}

/// Reads the persisted home choices. A missing file (first boot) is the
/// empty map; a malformed one warns and reads as empty — boot never
/// crashes on its debug catalog, the homes are re-derived and rewritten.
pub fn read_homes(wal_dir: &Path) -> HashMap<u32, u32> {
    let path = homes_path(wal_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            cascade_telemetry::log(
                cascade_telemetry::Level::Warn,
                "wal_homes_catalog_unreadable",
                &[("path", path.to_string_lossy().as_ref()), ("reason", e.to_string().as_str())],
            );
            return HashMap::new();
        }
    };
    match toml::from_str::<HomesDoc>(&text) {
        // A duplicated id row: the last one wins — the same rule the kernel
        // applies to repeated cmdline parameters.
        Ok(doc) => doc.db.into_iter().map(|row| (row.id, row.numa)).collect(),
        Err(e) => {
            cascade_telemetry::log(
                cascade_telemetry::Level::Warn,
                "wal_homes_catalog_unreadable",
                &[("path", path.to_string_lossy().as_ref()), ("reason", e.to_string().as_str())],
            );
            HashMap::new()
        }
    }
}

/// Persists the home choices (A14: chosen at creation, re-read at boot).
/// Every decided row is written — existing rows are preserved so future
/// databases' homes are never dropped by a reboot. The write is ATOMIC
/// (post-M5 review low #8): serialize to a sibling temp file, fsync it,
/// rename over the catalog, and fsync the directory so the rename itself
/// survives power loss. The old in-place `fs::write` could be torn by a
/// crash mid-write, and a torn catalog reads as empty — boot would
/// silently re-derive and potentially MIGRATE a home across a layout
/// change, the exact thing A14 exists to prevent. Failure warns only:
/// the next boot re-derives rather than refusing to start.
pub fn write_homes(wal_dir: &Path, homes: &HashMap<u32, u32>) {
    let doc = HomesDoc {
        db: {
            let mut rows: Vec<DbHome> = homes
                .iter()
                .map(|(&id, &numa)| DbHome { id, numa })
                .collect();
            rows.sort_by_key(|row| row.id);
            rows
        },
    };
    let path = homes_path(wal_dir);
    let result = toml::to_string(&doc)
        .map_err(std::io::Error::other)
        .and_then(|text| write_atomically(&path, &text));
    if let Err(e) = result {
        cascade_telemetry::log(
            cascade_telemetry::Level::Warn,
            "wal_homes_catalog_unwritable",
            &[("path", path.to_string_lossy().as_ref()), ("reason", e.to_string().as_str())],
        );
    }
}

/// temp file + fsync + rename, in the catalog's own directory (a same-
/// directory rename is atomic on POSIX). Any error propagates — `fdatasync`
/// on the WAL commit path has the same discipline, and the caller warns.
fn write_atomically(path: &Path, text: &str) -> Result<(), std::io::Error> {
    use std::io::Write as _;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Pin the rename: without a directory fsync, a power cut can leave the
    // old catalog (or none) visible after the crash that the rename
    // already reported surviving.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden literals for the frozen `db_hash` layout (FNV-1a 64 over the
    /// little-endian id), computed independently of the crate — a refactored
    /// hash breaks these, which is the point: derived homes may not drift.
    #[test]
    fn db_hash_is_frozen() {
        assert_eq!(db_hash(DatabaseId(0)), 0x4d25_767f_9dce_13f5);
        assert_eq!(db_hash(DatabaseId(7)), 0x6d35_7266_9b2c_de42);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cascade-walhome-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn homes_catalog_roundtrips() {
        let dir = temp_dir("roundtrip");
        let mut homes = HashMap::new();
        homes.insert(0u32, 1u32);
        homes.insert(3u32, 2u32);
        write_homes(&dir, &homes);
        assert_eq!(read_homes(&dir), homes, "boot re-reads what boot wrote");
        assert!(homes_path(&dir).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Low #8 (post-M5 review): the write is temp+fsync+rename, so a
    /// completed write replaces the previous catalog wholesale and leaves
    /// no temp sibling behind — the litter is the observable that the
    /// atomic path ran (the crash-mid-write half of atomicity is not unit
    /// testable; the mechanism is the temp+rename, not a test double).
    #[test]
    fn homes_write_replaces_atomically_and_leaves_no_temp_litter() {
        let dir = temp_dir("atomic");
        let mut homes = HashMap::new();
        homes.insert(0u32, 1u32);
        write_homes(&dir, &homes);
        // Overwrite in place — the rename path replaces an existing
        // catalog, and the extra row survives as a whole.
        homes.insert(3u32, 2u32);
        write_homes(&dir, &homes);
        assert_eq!(read_homes(&dir), homes, "the second write replaced the first");
        let mut files: Vec<String> = std::fs::read_dir(&dir)
            .expect("catalog dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec!["wal_homes.toml".to_owned()],
            "no temp sibling survives a completed write"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_catalog_reads_as_empty() {
        let dir = temp_dir("missing");
        assert!(read_homes(&dir).is_empty(), "first boot has no rows");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_catalog_reads_as_empty() {
        let dir = temp_dir("malformed");
        std::fs::write(homes_path(&dir), "not = [valid toml").expect("write junk");
        assert!(
            read_homes(&dir).is_empty(),
            "a broken catalog degrades to re-derivation, not a boot crash"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
