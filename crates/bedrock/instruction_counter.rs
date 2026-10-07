// SPDX-License-Identifier: GPL-2.0

//! Direct MSR-based instruction counter using `IA32_PMC0`.
//!
//! Counts guest `INST_RETIRED.ANY_P` (event 0xC0) on GP counter 0. The counter
//! MSR is in both the VM-exit MSR-store and VM-entry MSR-load lists, pointing
//! at the same entry: exit saves `IA32_PMC0` before any host code runs, and the
//! next entry reloads it, wiping host-side ticks. A GP counter is used because
//! PEBS needs `IA32_FIXED_CTR0` (see `exits/pebs.rs`).
//!
//! Userspace must pin the thread before creating the VM; on hybrid CPUs, to a
//! P-core (where GP counter 0 supports `INST_RETIRED.ANY_P`).

use super::page::{alloc_zeroed_page, KernelPage};
use crate::c_helpers;
use crate::vmx::traits::{InstructionCounter, InstructionCounterError};

/// Full-width-write alias for `IA32_PMC0`. `WRMSR` to `IA32_PMC0` (0xC1)
/// truncates to 32 bits and sign-extends bit 31, garbling values past ~2.1B;
/// the alias writes all 48 bits. Requires `IA32_PERF_CAPABILITIES.FULL_WRITE`
/// (bit 13). SDM Vol 3B 21.2.8.
const IA32_A_PMC0: u32 = 0x4C1;
/// Performance event-select register for `IA32_PMC0`.
const IA32_PERFEVTSEL0: u32 = 0x186;
/// Global enable for performance counters (SDM Vol 4 Table 2-2).
const IA32_PERF_GLOBAL_CTRL: u32 = 0x38F;

/// `IA32_PERFEVTSEL0` for `INST_RETIRED.ANY_P`: event 0xC0, umask 0, USR (16),
/// OS (17), EN (22).
const PERFEVTSEL0_INST_RETIRED_ANY_P: u64 = (1u64 << 16) | (1u64 << 17) | (1u64 << 22) | 0xC0;
/// Bit 0 in `IA32_PERF_GLOBAL_CTRL` enables `IA32_PMC0`.
const PERF_GLOBAL_CTRL_PMC0: u64 = 1;

/// VMCS MSR-list entry layout (SDM Vol 3C Table 26-16).
#[repr(C)]
struct MsrListEntry {
    msr_index: u32,
    reserved: u32,
    msr_data: u64,
}

#[inline]
fn rdmsr(addr: u32) -> Result<u64, InstructionCounterError> {
    let mut value = 0;
    // SAFETY: `value` is a valid output pointer. The kernel helper catches
    // the #GP raised when the MSR is unavailable.
    let ret = unsafe { c_helpers::bedrock_rdmsr_safe(addr, &mut value) };
    if ret != 0 {
        return Err(InstructionCounterError::Unavailable);
    }
    Ok(value)
}

#[inline]
fn wrmsr(addr: u32, value: u64) -> Result<(), InstructionCounterError> {
    // SAFETY: The kernel helper catches the #GP raised when the MSR or value
    // is unavailable.
    let ret = unsafe { c_helpers::bedrock_wrmsr_safe(addr, value) };
    if ret != 0 {
        return Err(InstructionCounterError::ProgramFailed);
    }
    Ok(())
}

/// Direct MSR-based instruction counter for general-purpose counter 0.
pub(crate) struct LinuxInstructionCounter {
    svm: bool,
    svm_count: u64,
    /// Backing page; the first 16 bytes are the MSR-list entry. None on null
    /// counters.
    msr_entry_page: Option<KernelPage>,
    /// Saved `IA32_PERFEVTSEL0`, captured in `prepare`, restored in `finish`.
    saved_perfevtsel0: u64,
    /// Value the CPU loads into `IA32_PERF_GLOBAL_CTRL` on VM entry.
    guest_perf_global_ctrl: u64,
    /// Value the CPU loads into `IA32_PERF_GLOBAL_CTRL` on VM exit.
    host_perf_global_ctrl: u64,
    /// Whether `prepare` has run since the last `finish`.
    armed: bool,
}

// SAFETY: KernelPage is Send. The MSR list entry is only accessed with
// preemption disabled in the run loop on the CPU owning the VMCS, so there is
// no concurrent access.
unsafe impl Send for LinuxInstructionCounter {}

impl LinuxInstructionCounter {
    pub(crate) fn new() -> Self {
        let svm = super::svm::supported();
        let msr_entry_page = if svm { None } else { alloc_zeroed_page() }.inspect(|page| {
            // SAFETY: the page is freshly allocated, zeroed, and not aliased;
            // write a single MSR list entry for IA32_A_PMC0.
            unsafe {
                let entry = page.virt.as_u64() as *mut MsrListEntry;
                core::ptr::write(
                    entry,
                    MsrListEntry {
                        msr_index: IA32_A_PMC0,
                        reserved: 0,
                        msr_data: 0,
                    },
                );
            }
        });

        Self {
            svm,
            svm_count: 0,
            msr_entry_page,
            saved_perfevtsel0: 0,
            guest_perf_global_ctrl: 0,
            host_perf_global_ctrl: 0,
            armed: false,
        }
    }

    /// Counter value at the last VM exit (written by the CPU's MSR-store).
    #[inline]
    fn entry_msr_data(&self) -> u64 {
        match self.msr_entry_page.as_ref() {
            Some(page) => {
                // SAFETY: the entry was initialized in `new` and lives as long
                // as `self`. The CPU writes it only on VM exit; no concurrent
                // access while preemption is disabled.
                unsafe {
                    let entry = page.virt.as_u64() as *const MsrListEntry;
                    core::ptr::read_volatile(&(*entry).msr_data)
                }
            }
            None => 0,
        }
    }
}

impl InstructionCounter for LinuxInstructionCounter {
    fn record_instructions(&mut self, instructions: u64) {
        if self.svm { self.svm_count += instructions; }
    }
    fn record_exit(&mut self, reason: u32) {
        if self.svm && reason == 37 { self.svm_count += 1; }
    }
    fn prepare(&mut self) -> Result<(), InstructionCounterError> {
        if self.msr_entry_page.is_none() {
            return Ok(());
        }

        // Host value clears PMC0's enable bit as a first-line gate. NMI handlers
        // can still flip it, but host ticks are overwritten on the next entry.
        let current_global = rdmsr(IA32_PERF_GLOBAL_CTRL)?;
        self.host_perf_global_ctrl = current_global & !PERF_GLOBAL_CTRL_PMC0;
        self.guest_perf_global_ctrl = self.host_perf_global_ctrl | PERF_GLOBAL_CTRL_PMC0;

        let saved = rdmsr(IA32_PERFEVTSEL0)?;
        self.saved_perfevtsel0 = saved;
        wrmsr(IA32_PERFEVTSEL0, PERFEVTSEL0_INST_RETIRED_ANY_P)?;

        self.armed = true;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), InstructionCounterError> {
        if !self.armed {
            return Ok(());
        }
        // PERF_GLOBAL_CTRL was already loaded by hardware on the last VM exit.
        if wrmsr(IA32_PERFEVTSEL0, self.saved_perfevtsel0).is_err() {
            return Err(InstructionCounterError::RestoreFailed);
        }
        self.armed = false;
        Ok(())
    }

    fn read(&self) -> u64 {
        if self.svm { return self.svm_count; }
        // Monotonic across run loops: each entry reloads `IA32_PMC0` from this
        // entry and each exit saves it back.
        self.entry_msr_data()
    }

    fn is_configured(&self) -> bool {
        self.svm || self.msr_entry_page.is_some()
    }

    fn perf_global_ctrl_values(&self) -> Option<(u64, u64)> {
        if self.armed {
            Some((self.guest_perf_global_ctrl, self.host_perf_global_ctrl))
        } else {
            None
        }
    }

    fn msr_save_load_entry_phys(&self) -> Option<u64> {
        self.msr_entry_page.as_ref().map(|p| p.phys.as_u64())
    }
}
