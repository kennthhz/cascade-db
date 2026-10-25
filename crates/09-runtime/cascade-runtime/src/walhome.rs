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
pub fn derive_wal_home(db: DatabaseId, pools: &PoolRegistry) -> Option<u32> {
    let idx = (db_hash(db) % pools.num_numas() as u64) as usize;
    pools.iter().nth(idx).map(|p| p.numa())
}

/// Constructs the home writer on its home NUMA: a pinned constructor
/// thread first-touches the writer's state on a core of that node (a
/// worker core of the NUMA in config order, else any online core the
/// topology places there). Returns the writer and the core it was
/// constructed on (`None` = every fallback fired and the writer was built
/// on the boot thread — reported, not hidden).
pub fn construct_writer_on(
    cfg: &crate::config::RuntimeConfig,
    online: &[u32],
    topology: &dyn crate::topology::TopologyProvider,
    numa: u32,
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
        return (std::sync::Arc::new(cascade_wal::WalWriter::new()), None);
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
            let writer = std::sync::Arc::new(cascade_wal::WalWriter::new());
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
                        ("reason", "pin verification failed; boot-thread placement"),
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
                (std::sync::Arc::new(cascade_wal::WalWriter::new()), None)
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
            (std::sync::Arc::new(cascade_wal::WalWriter::new()), None)
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
/// databases' homes are never dropped by a reboot. Failure warns only:
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
    match toml::to_string(&doc) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                cascade_telemetry::log(
                    cascade_telemetry::Level::Warn,
                    "wal_homes_catalog_unwritable",
                    &[("path", path.to_string_lossy().as_ref()), ("reason", e.to_string().as_str())],
                );
            }
        }
        Err(e) => {
            cascade_telemetry::log(
                cascade_telemetry::Level::Warn,
                "wal_homes_catalog_unwritable",
                &[("path", path.to_string_lossy().as_ref()), ("reason", e.to_string().as_str())],
            );
        }
    }
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
