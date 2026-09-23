// SPDX-License-Identifier: GPL-2.0

//! Host TSC access for performance measurement (returns 0 in cargo builds).

/// Read the host TSC.
#[cfg(not(feature = "cargo"))]
#[inline]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: RDTSC is safe to execute and only reads the timestamp counter.
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem)
        );
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Test stub: always 0, so perf stats show 0 cycles in tests.
#[cfg(feature = "cargo")]
#[inline]
pub fn rdtsc() -> u64 {
    0
}
