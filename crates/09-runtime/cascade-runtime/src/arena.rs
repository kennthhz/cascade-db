//! Per-reactor working-memory arena (spec §2.3, §10.1 step 6b; FR-37).
//!
//! The *allocator mechanism* is the runtime ADR (the spec names
//! mimalloc-style thread-local heaps as the example); this module lands the
//! placement contract those heaps will adopt. Each reactor reserves its
//! arena in-task, on its verified core, so every page of it first-touches
//! NUMA-local by construction — no cross-thread handoff exists that could
//! place it wrong. The arena is one private mapping, which is what makes
//! FR-37's "verified on bare metal" checkable: its pages are attributable
//! in `/proc/self/numa_maps`.
//!
//! Reservation, not commitment: pages are faulted only when used, so the
//! reservation costs virtual address space until the allocator (or a
//! test's `touch`) backs it.

use std::io;

use crate::mapping::Mapping;

/// Reserved per reactor at boot. Sizing policy is the allocator ADR's to
/// revisit; this is a working-set-sized reservation, virtual-only until
/// touched.
pub const ARENA_RESERVE_BYTES: u64 = 64 * 1024 * 1024;

/// One reactor's arena. Created, used, and dropped on the owning reactor's
/// thread (deliberately not `Send`).
pub struct ReactorArena {
    mapping: Mapping,
}

impl ReactorArena {
    /// Reserves `bytes` of address space for this reactor's working
    /// memory. Fails only if the kernel refuses the mapping.
    pub fn reserve(bytes: u64) -> io::Result<Self> {
        let len = usize::try_from(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena size overflow"))?;
        Ok(ReactorArena {
            mapping: Mapping::map(len, 4096)?,
        })
    }

    /// Reserved size in bytes.
    pub fn len(&self) -> usize {
        self.mapping.len()
    }

    /// The arena is always non-empty (`reserve(0)` is rejected upstream by
    /// the config's positivity rules); kept for clippy completeness.
    pub fn is_empty(&self) -> bool {
        self.mapping.len() == 0
    }

    /// Base address — the bare-metal locality check finds this range in
    /// `/proc/self/numa_maps`.
    pub fn addr(&self) -> usize {
        self.mapping.addr()
    }

    /// Faults every page in (one volatile write per kernel page). Not a
    /// production path — the bare-metal check needs resident pages to see
    /// placement; production faults pages lazily at first use, which is
    /// the point of reserving in-task.
    pub fn touch(&self) {
        self.mapping.touch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_and_touches_a_page_aligned_region() {
        let arena = ReactorArena::reserve(ARENA_RESERVE_BYTES).expect("arena reserves");
        assert_eq!(arena.len() as u64, ARENA_RESERVE_BYTES);
        assert_ne!(arena.addr(), 0);
        arena.touch(); // faults ~16k pages of a 64 MiB region — fast enough
    }

    #[test]
    fn rejects_sizes_that_do_not_fit_usize() {
        assert!(ReactorArena::reserve(u64::MAX).is_err());
    }
}
