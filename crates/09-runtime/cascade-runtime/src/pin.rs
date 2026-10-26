//! Thread pinning helpers (spec §2.2, §3.1; FR-2).
//!
//! Pinning is only real if it is *verified*: every path that sets affinity
//! reads `sched_getaffinity` back and refuses to report success otherwise —
//! a pin that silently didn't stick is exactly the failure mode FR-2 exists
//! to catch.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum PinError {
    #[error("sched_setaffinity to core {core} failed: {source}")]
    SetFailed {
        core: u32,
        #[source]
        source: io::Error,
    },
    #[error("pin to core {core} did not stick; kernel reports affinity = {seen:?}")]
    VerifyFailed { core: u32, seen: Vec<u32> },
}

/// CPUs the calling thread may run on, straight from the kernel (pid 0 =
/// calling thread).
pub fn current_affinity() -> Vec<u32> {
    unsafe {
        let mut mask: libc::cpu_set_t = std::mem::zeroed();
        let got = libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut mask);
        assert_eq!(got, 0, "sched_getaffinity failed");
        (0..(std::mem::size_of::<libc::cpu_set_t>() * 8))
            .filter(|&cpu| libc::CPU_ISSET(cpu, &mask))
            .map(|cpu| cpu as u32)
            .collect()
    }
}

/// The calling thread must currently run on exactly `core` — the read-back
/// half of FR-2. Success carries the kernel's read-back (a single-element
/// set, or the mismatch would be `Err`) so boot can enforce one-thread-per-
/// core over the *verified* affinities rather than the config's intent
/// (T2.1): what was measured, not what was asked for.
pub fn verify_core(core: u32) -> Result<Vec<u32>, PinError> {
    let seen = current_affinity();
    if seen == vec![core] {
        Ok(seen)
    } else {
        Err(PinError::VerifyFailed { core, seen })
    }
}

/// Pin the calling thread to exactly one CPU and verify it took (housekeeper
/// threads, R5 — plain `std::thread` pins itself; workers let glommio's
/// `Placement::Fixed` set affinity and are verified with [`verify_core`]).
/// Success carries the verified affinity, as with [`verify_core`].
pub fn pin_current(core: u32) -> Result<Vec<u32>, PinError> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core as usize, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(PinError::SetFailed {
                core,
                source: io::Error::last_os_error(),
            });
        }
    }
    verify_core(core)
}
