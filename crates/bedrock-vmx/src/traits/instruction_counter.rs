// SPDX-License-Identifier: GPL-2.0

//! Guest instruction counter. In kernel builds it is a PMU counter whose
//! `IA32_PERF_GLOBAL_CTRL` is swapped by VMCS controls and whose value is
//! saved/restored via VMCS MSR lists, so it counts only guest execution.

/// Error while preparing or restoring an instruction counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionCounterError {
    /// Host lacks the required PMU MSRs.
    Unavailable,
    ProgramFailed,
    RestoreFailed,
}

/// Counts guest instructions retired.
///
/// `prepare` runs once before the run loop (preemption disabled, on the loop's
/// CPU), `finish` once after it; `read` may be called after each VM exit.
pub trait InstructionCounter {
    /// Software counting backends account for completed hardware steps here.
    /// Hardware PMU backends already captured their count at VM exit.
    fn record_exit(&mut self, _reason: u32) {}
    fn record_instructions(&mut self, _instructions: u64) {}
    /// Program the host PMU (e.g. `IA32_PERFEVTSEL0`) and reset the counter.
    /// Preemption must be disabled.
    #[inline]
    fn prepare(&mut self) -> Result<(), InstructionCounterError> {
        Ok(())
    }

    /// Restore host PMU state, on the same CPU as `prepare`.
    #[inline]
    fn finish(&mut self) -> Result<(), InstructionCounterError> {
        Ok(())
    }

    fn read(&self) -> u64;

    /// Whether this counter is hardware-backed (`false` for the null impl).
    fn is_configured(&self) -> bool;

    /// `(guest, host)` `IA32_PERF_GLOBAL_CTRL` values for the CPU to swap on VM
    /// entry/exit; `None` for null counters. Valid only after `prepare`.
    fn perf_global_ctrl_values(&self) -> Option<(u64, u64)>;

    /// Physical address of a 16-byte VMCS MSR list entry used as both VM-exit
    /// MSR-store and VM-entry MSR-load; its data field is what `read` returns.
    ///
    /// Reloading the saved value on every entry wipes whatever the host (NMI
    /// handlers, perf, ...) did to the live MSR between exits, keeping the count
    /// deterministic without registering a perf event. `None` for null/mock counters.
    #[inline]
    fn msr_save_load_entry_phys(&self) -> Option<u64> {
        None
    }
}

/// Null implementation for VMs without instruction counting.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullInstructionCounter;

impl InstructionCounter for NullInstructionCounter {
    #[inline]
    fn read(&self) -> u64 {
        0
    }

    #[inline]
    fn is_configured(&self) -> bool {
        false
    }

    #[inline]
    fn perf_global_ctrl_values(&self) -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
#[path = "instruction_counter_tests.rs"]
mod tests;
