//! Per-NUMA BPM pool registry (spec §8.1, §10.1 step 5; FR-22).
//!
//! Boot allocates one pool per `runtime.numa_pools` entry — the pool's page
//! RAM (`size_mb` → `frames × PAGE_SIZE` bytes, sized per NUMA per §8.1,
//! never one total split uniformly) — before any worker exists (§10.1
//! orders pools at step 5, workers at step 6). Placement is real, not
//! aspirational: each pool is one private mapping bound to its node with
//! `mbind(MPOL_BIND)`, so every page the pool ever faults is node-local
//! regardless of which reactor touches it first. Where the kernel has no
//! such node (hosts with no NUMA information — WSL2, some VMs — or a
//! memoryless node), boot degrades with a warning to first-touch placement
//! rather than claim a locality it cannot deliver.
//!
//! The M1 BPM (`cascade-bpm::Bpm`) is still the single-reactor slice and
//! allocates its own frames; this registry is the §10.1 step-5 allocation
//! the §8.1 concurrent BPM (M3) will adopt. Until then the registry is the
//! placement + sizing contract: which NUMA gets how many frames
//! ([`NumaPoolHandle::frames`]), and which pool serves which worker —
//! §10.1 step 6c binds each reactor to [`PoolRegistry::index_for_numa`].
//! A pool may exist with no workers on its node (plan A18): legal, and it
//! serves `cross`-affinity placement only.

use std::io;

use cascade_types::{PageId, PAGE_SIZE};

use crate::{config::RuntimeConfig, mapping::Mapping};

const MIB: u64 = 1 << 20;

/// FR-22's cross-affinity placement hash: FNV-1a over the three `PageId`
/// fields (I3 — the key is `(DatabaseId, SegmentId, PageNo)`, never a file
/// path), each fixed-width little-endian in a frozen order. The algorithm
/// and layout are *frozen*: a page's pool must not move across builds,
/// processes, or restarts, so this is deliberately not `std`'s
/// `RandomState`-keyed `Hash` (whose seed differs per process). Golden
/// literals in `tests/pools.rs` pin the contract.
fn page_hash(page_id: &PageId) -> u64 {
    fn mix(h: &mut u64, bytes: &[u8]) {
        for b in bytes {
            *h ^= u64::from(*b);
            *h = h.wrapping_mul(0x0100_0000_01b3); // 0x100000001b3, the FNV-1a 64 prime
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    mix(&mut h, &page_id.db.0.to_le_bytes());
    mix(&mut h, &page_id.segment.0.to_le_bytes());
    mix(&mut h, &page_id.page_no.to_le_bytes());
    h
}

/// One pool's page RAM: `frames × PAGE_SIZE` bytes in a private mapping
/// with a `PAGE_SIZE`-aligned base, so frames are directly usable as page
/// images (O_DIRECT-ready) when the BPM adopts this memory.
pub struct PoolMemory {
    mapping: Mapping,
}

impl PoolMemory {
    fn reserve(frames: u32) -> io::Result<Self> {
        Ok(PoolMemory {
            mapping: Mapping::map(frames as usize * PAGE_SIZE, PAGE_SIZE)?,
        })
    }

    /// Faults every page in — tests and the bare-metal locality check only
    /// (production never pre-faults; a large pool would commit its whole
    /// RSS at boot for no benefit over `mbind`-guided lazy faulting).
    pub fn touch(&self) {
        self.mapping.touch()
    }

    /// Base address — the bare-metal check finds this range in
    /// `/proc/self/numa_maps`.
    pub fn addr(&self) -> usize {
        self.mapping.addr()
    }

    /// The `i`th 8-KiB frame.
    pub fn frame(&self, i: u32) -> &[u8] {
        self.mapping.frame(i as usize, PAGE_SIZE)
    }

    /// Overwrites the `i`th frame with `image` (boot-time population —
    /// the demo heap is the only writer, and `&mut self` proves it runs
    /// while the registry is still exclusively held by boot).
    pub fn write_frame(&mut self, i: u32, image: &[u8]) {
        assert!(
            image.len() == PAGE_SIZE,
            "a frame image is exactly PAGE_SIZE bytes"
        );
        let dst = self.mapping.frame_mut(i as usize, PAGE_SIZE);
        dst.copy_from_slice(image);
    }
}

/// One registry entry: a pool's placement + sizing contract (FR-22) and
/// its RAM.
pub struct NumaPoolHandle {
    numa: u32,
    frames: u32,
    memory: PoolMemory,
    /// The kernel accepted this pool's `mbind` — every page of it is
    /// node-local no matter who faults it in.
    node_bound: bool,
}

impl NumaPoolHandle {
    /// The NUMA node this pool belongs to.
    pub fn numa(&self) -> u32 {
        self.numa
    }

    /// Frame count derived from the config's `size_mb` (§8.1 per-NUMA
    /// sizing).
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// True when the kernel accepted the node binding (placement is
    /// guaranteed); false means first-touch best-effort — see the module
    /// doc.
    pub fn node_bound(&self) -> bool {
        self.node_bound
    }

    /// The pool's page RAM.
    pub fn memory(&self) -> &PoolMemory {
        &self.memory
    }

    /// Exclusive access to the pool's page RAM (boot-time population).
    pub fn memory_mut(&mut self) -> &mut PoolMemory {
        &mut self.memory
    }
}

/// The boot-time pool registry (§10.1 step 5), in config order.
pub struct PoolRegistry {
    pools: Vec<NumaPoolHandle>,
}

impl PoolRegistry {
    /// Builds one pool per `runtime.numa_pools` entry. Precondition: the
    /// config has been validated (`size_mb ≥ 1`, no duplicate NUMAs) —
    /// [`crate::boot`] validates before calling this, which is exactly the
    /// T2.3 code-review order: topology is resolved and placement rules
    /// checked *before* pools are built, never after.
    pub fn build(cfg: &RuntimeConfig) -> Result<Self, crate::boot::BootError> {
        let mut pools = Vec::with_capacity(cfg.numa_pools.len());
        for pool in &cfg.numa_pools {
            let frames = frames_for(pool.numa, pool.size_mb)?;
            let memory =
                PoolMemory::reserve(frames).map_err(|source| crate::boot::BootError::PoolReserve {
                    numa: pool.numa,
                    source,
                })?;
            let node_bound = memory.mapping.mbind_node(pool.numa);
            if !node_bound {
                cascade_telemetry::log(
                    cascade_telemetry::Level::Warn,
                    "pool_placement_fallback",
                    &[
                        ("numa", pool.numa.to_string().as_str()),
                        ("policy", "first-touch (kernel has no such NUMA node)"),
                    ],
                );
            }
            pools.push(NumaPoolHandle {
                numa: pool.numa,
                frames,
                memory,
                node_bound,
            });
        }
        Ok(PoolRegistry { pools })
    }

    /// How many pools — also `num_numas` for the FR-22 `cross`-affinity
    /// page placement (`hash(PageId) mod num_numas`, T2.5).
    pub fn num_numas(&self) -> usize {
        self.pools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &NumaPoolHandle> {
        self.pools.iter()
    }

    /// The pool on `numa`, if configured.
    pub fn pool_for_numa(&self, numa: u32) -> Option<&NumaPoolHandle> {
        self.pools.iter().find(|p| p.numa == numa)
    }

    /// Exclusive access to one pool, by NUMA (boot-time population).
    pub fn pool_for_numa_mut(&mut self, numa: u32) -> Option<&mut NumaPoolHandle> {
        self.pools
            .iter_mut()
            .find(|p| p.numa() == numa)
    }

    /// Registry index of the pool on `numa` (§10.1 step 6c's binding for
    /// reactors on that node). `None` only for a NUMA with no pool — which
    /// validated configs cannot produce for a worker node.
    pub fn index_for_numa(&self, numa: u32) -> Option<usize> {
        self.pools.iter().position(|p| p.numa == numa)
    }

    /// FR-22 placement for a `cross`-affinity page: `hash(PageId) mod
    /// num_numas` — a single direct lookup, no miss-probing (§8.1). The
    /// result depends only on the page key and the pool count, so the same
    /// `PageId` lands in the same pool from any reactor and across
    /// restarts. `None` only for an empty registry (boot never builds one).
    pub fn pool_for_page(&self, page_id: &PageId) -> Option<&NumaPoolHandle> {
        let idx = (page_hash(page_id) % self.pools.len() as u64) as usize;
        self.pools.get(idx)
    }
}

/// Frame count for one pool's `size_mb` (§8.1: per-NUMA sizing). `size_mb`
/// is validated ≥ 1, so every pool has ≥ 128 frames; a size whose frame
/// count leaves `u32` (the BPM's `max_frames` width) refuses boot instead
/// of wrapping.
fn frames_for(numa: u32, size_mb: u64) -> Result<u32, crate::boot::BootError> {
    let bytes = size_mb
        .checked_mul(MIB)
        .ok_or(crate::boot::BootError::PoolTooLarge { numa, size_mb })?;
    let frames = bytes / PAGE_SIZE as u64;
    u32::try_from(frames).map_err(|_| crate::boot::BootError::PoolTooLarge { numa, size_mb })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_mb_converts_to_frames() {
        // 1 MiB / 8 KiB = 128 — the smallest legal pool.
        assert_eq!(frames_for(0, 1).unwrap(), 128);
        assert_eq!(frames_for(1, 2).unwrap(), 256);
        assert_eq!(frames_for(0, 1024).unwrap(), 131_072); // 1 GiB / 8 KiB
    }

    #[test]
    fn frame_count_must_fit_the_bpm_width() {
        // The largest whole-MiB size whose frame count fits u32; one more
        // MiB pushes the count past u32::MAX (whole MiB can't express
        // u32::MAX frames exactly — 8 KiB frames don't divide a MiB-boundary
        // count evenly at that height) and refuses.
        let max_mb = (u32::MAX as u64 * PAGE_SIZE as u64) / MIB;
        assert_eq!(frames_for(0, max_mb).unwrap(), u32::MAX - 127);
        match frames_for(0, max_mb + 1) {
            Err(crate::boot::BootError::PoolTooLarge { numa: 0, .. }) => {}
            other => panic!("expected PoolTooLarge, got {other:?}"),
        }
        // And a size_mb whose byte count itself overflows u64.
        match frames_for(0, u64::MAX) {
            Err(crate::boot::BootError::PoolTooLarge { numa: 0, .. }) => {}
            other => panic!("expected PoolTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn pool_ram_is_page_size_aligned_and_framed() {
        let mem = PoolMemory::reserve(4).expect("pool memory reserves");
        assert_eq!(mem.addr() % PAGE_SIZE, 0, "frames must be 8-KiB aligned");
        assert_eq!(mem.frame(3).len(), PAGE_SIZE);
    }
}
