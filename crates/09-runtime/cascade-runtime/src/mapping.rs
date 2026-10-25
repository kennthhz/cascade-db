//! Private anonymous mappings backing all *placed* memory (spec §8.1,
//! §10.1 steps 5 and 6b; FR-22, FR-37).
//!
//! Both boot-time allocations the runtime makes — the per-NUMA BPM pools
//! ([`crate::pools`], §10.1 step 5) and the per-reactor working arenas
//! ([`crate::arena`], step 6b) — are built on this one type, and each
//! allocation is exactly one VMA, owned from `mmap` to `munmap`. That
//! single-VMA ownership is what makes placement real and checkable:
//! `mbind` can address exactly this region (pools bind their node here, so
//! the faulting thread's own node is irrelevant), and the bare-metal
//! locality checks attribute pages to nodes in `/proc/self/numa_maps`
//! (pools and arenas alike). Arenas reserve in-task on the verified core;
//! pools mbind explicitly at boot — two mechanisms, one substrate.
//!
//! Not `Clone`/`Send` by discipline: a mapping is created, used, and
//! dropped on one thread (boot for pools, the reactor for arenas).

use std::io;

/// One owned anonymous mapping. Not `Clone`/`Send` by discipline: it is
/// created, used, and dropped on one thread (boot for pools, the reactor
/// for arenas).
pub(crate) struct Mapping {
    /// The address `mmap` returned — `munmap` needs exactly this.
    raw: *mut libc::c_void,
    base: *mut u8,
    len: usize,
    total: usize,
}

impl Mapping {
    /// Maps `len` bytes, base aligned up to `align`. `mmap` only promises
    /// page (4 KiB) alignment; the pool's frames want `PAGE_SIZE`-aligned
    /// addresses so O_DIRECT I/O can target them when the BPM adopts this
    /// memory.
    pub(crate) fn map(len: usize, align: usize) -> io::Result<Self> {
        let total = len
            .checked_add(align)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mapping size overflow"))?;
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = ((raw as usize) + align - 1) & !(align - 1);
        Ok(Mapping {
            raw,
            base: base as *mut u8,
            len,
            total,
        })
    }

    pub(crate) fn addr(&self) -> usize {
        self.base as usize
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The `i`th `PAGE_SIZE` frame of the mapping.
    pub(crate) fn frame(&self, i: usize, frame_len: usize) -> &[u8] {
        assert!(
            (i + 1) * frame_len <= self.len,
            "frame {i} out of range (len {})",
            self.len
        );
        // SAFETY: `base + i*frame_len .. +frame_len` is inside the owned
        // mapping for any `i` the assert allows; the slice is read-only.
        unsafe { std::slice::from_raw_parts(self.base.add(i * frame_len), frame_len) }
    }

    /// Faults every kernel page of the mapping in (one volatile write per
    /// page). Never used on a hot path — tests and the bare-metal locality
    /// check, which needs resident pages to see placement in numa_maps.
    pub(crate) fn touch(&self) {
        let page = kernel_page_size();
        let mut off = 0;
        while off < self.len {
            // SAFETY: `off < len`, so the address is inside the mapping.
            unsafe { self.base.add(off).write_volatile(0u8) };
            off += page;
        }
    }

    /// Binds this mapping's *future* page faults to NUMA node `numa`
    /// (`mbind(MPOL_BIND | MPOL_F_STATIC_NODES)`). This — not first-touch —
    /// is the placement mechanism (§10.1 step 5: pools are "backed by
    /// NUMA-local memory"): from a successful call on, every page of the
    /// pool is node-local no matter which reactor faults it in, so the
    /// booting thread's own node is irrelevant (the T2.3 code-review
    /// failure mode). `false` means the kernel refused (node absent — the
    /// common case on hosts with no NUMA information, e.g. WSL2 — or
    /// memoryless); the caller degrades to first-touch with a warning.
    pub(crate) fn mbind_node(&self, numa: u32) -> bool {
        if numa >= 64 {
            // The nodemask is one word; no host this targets has 64+ nodes.
            return false;
        }
        let mask: libc::c_ulong = 1 << numa;
        // The libc crate does not wrap `mbind` on all targets; the syscall
        // is stable (Linux ≥ 2.6) and takes exactly these six arguments.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_mbind,
                self.base as *mut libc::c_void,
                self.len as libc::c_ulong,
                libc::MPOL_BIND,
                &mask as *const libc::c_ulong,
                64usize,
                libc::MPOL_F_STATIC_NODES,
            )
        };
        rc == 0
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `raw`/`total` are exactly what `mmap` handed out; this is
        // the only drop.
        unsafe { libc::munmap(self.raw, self.total) };
    }
}

/// Kernel page size — the fault granularity `touch` must step over.
pub(crate) fn kernel_page_size() -> usize {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 {
        ps as usize
    } else {
        4096
    }
}
