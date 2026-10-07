// SPDX-License-Identifier: GPL-2.0

//! Machine abstraction traits: hardware access for VMX, mockable for tests.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::{Kernel, Page, VirtualMachineControlStructure, Vmx, VmxCpu};

/// Groups all hardware access traits so the run loop can be tested without hardware.
pub trait Machine: Send + Sync {
    type P: Page;
    type K: Kernel<P = Self::P>;
    type M: MsrAccess;
    type C: CrAccess;
    type D: DescriptorTableAccess;
    type V: Vmx<M = Self>;
    /// Per-CPU VMX state.
    type Vcpu: VmxCpu<M = Self> + 'static;

    fn kernel(&self) -> &Self::K;
    fn msr_access(&self) -> &Self::M;
    fn cr_access(&self) -> &Self::C;
    fn descriptor_table_access(&self) -> &Self::D;
}

/// Error from VM entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmEntryError {
    /// VMLAUNCH or VMRESUME failed.
    VmEntryFailed,
}

/// Boundaries of a verified instruction region, stopped before its endpoint.
/// Memory-accessing batches protect code against writes. Store batches also
/// protect translations, or validate the destination ranges before entry.
#[derive(Clone, Copy, Debug)]
pub struct InstructionBatch {
    pub start: u64,
    pub offsets: [u16; 65],
    pub count: usize,
    pub repeat: Option<RepeatBatch>,
    pub counted_loop: Option<CountedLoopBatch>,
    pub pages: [u64; 68],
    pub code_page_count: usize,
    /// Execution traps for otherwise unsafe entry points in permitted pages.
    pub page_breakpoints: [u64; 4],
    pub page_breakpoint_count: usize,
    /// Direct transfers leaving the decoded region stop before target execution.
    pub branch_exits: [u64; 3],
    pub branch_exit_count: usize,
    /// Exact retirements for straight-line paths stopped at outgoing targets.
    pub branch_exit_counts: [u16; 3],
    pub page_count: usize,
    pub accesses_memory: bool,
    pub writes_memory: bool,
    pub validated_stores: bool,
    /// Every reachable translation frame is protected against guest writes.
    pub guarded_stores: bool,
    pub uses_counter: bool,
    /// Every transfer moves forward, bounding retirements by `count`.
    pub counter_bounded: bool,
    /// Run freely within an immutable code page guarded by nested paging.
    pub page_execution: bool,
    pub endpoint_intercepted: bool,
    pub instruction_budget: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct RepeatBatch {
    pub original_count: u64,
    pub iterations: u64,
}

/// A MOV/LEA/DEC/JNZ loop with no other counter or flags consumers.
#[derive(Clone, Copy, Debug)]
pub struct CountedLoopBatch {
    /// Architectural GPR index: RCX (1) or RDX (2).
    pub register: u8,
    /// Width of the decrement, either 32 or 64 bits.
    pub width: u8,
    pub original_count: u64,
    pub iterations: u64,
    pub decrement_index: u64,
}

impl CountedLoopBatch {
    /// Recover the architectural counter, flags, and retirements at any body boundary.
    /// The counter is temporarily clamped on entry; only the final JNZ can observe
    /// different flags, and its artificial fall-through is rewound by the caller.
    pub fn account(
        &self,
        instruction: u64,
        body_length: u64,
        counter: u64,
        flags: u64,
    ) -> Option<(u64, u64, u64)> {
        if instruction > body_length
            || self.decrement_index >= body_length
            || !matches!(self.width, 32 | 64)
            || !matches!(self.register, 1 | 2)
        {
            return None;
        }
        let decrements = self.iterations.checked_sub(counter)?;
        let original = if self.width == 32 {
            self.original_count & u32::MAX as u64
        } else {
            self.original_count
        };
        let result = original.checked_sub(decrements)?;
        // A 32-bit write clears the upper half only after DEC has executed.
        let remaining = if decrements == 0 {
            self.original_count
        } else {
            result
        };
        let completed = if instruction == body_length {
            if counter != 0 {
                return None;
            }
            decrements.checked_mul(body_length)?
        } else {
            let loops = decrements.checked_sub(u64::from(instruction > self.decrement_index))?;
            if loops >= self.iterations {
                return None;
            }
            loops.checked_mul(body_length)?.checked_add(instruction)?
        };
        let flags = if decrements == 0 {
            flags
        } else {
            // DEC preserves CF and sets PF/AF/ZF/SF/OF from the real result.
            let arithmetic = (u64::from((remaining as u8).count_ones() % 2 == 0) << 2)
                | (u64::from(remaining & 15 == 15) << 4)
                | (u64::from(result == 0) << 6)
                | ((result >> (self.width - 1)) & 1) << 7
                | (u64::from(result == (1u64 << (self.width - 1)) - 1) << 11);
            (flags & !0x8d4) | arithmetic
        };
        Some((completed, remaining, flags))
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod counted_loop_tests {
    use super::CountedLoopBatch;

    fn native_dec_flags(value: u64) -> u64 {
        let flags: u64;
        unsafe {
            core::arch::asm!(
                "stc", "dec {value}", "pushfq", "pop {flags}",
                value = inout(reg) value => _, flags = lateout(reg) flags,
            );
        }
        flags
    }

    #[test]
    fn narrow_counters_match_native_zero_extension_and_flags() {
        for register in [1, 2] {
            for original_count in [0x100000003u64, 0x180000000, 0x180000001, u64::MAX] {
                for decrement_index in 0..3 {
                    let batch = CountedLoopBatch {
                        register,
                        width: 32,
                        original_count,
                        iterations: 3,
                        decrement_index,
                    };
                    for loops in 0..3 {
                        for instruction in 0..4 {
                            let decrements = loops + u64::from(instruction > decrement_index);
                            let (completed, remaining, flags) = batch
                                .account(instruction, 4, 3 - decrements, 0x203)
                                .unwrap();
                            let mut expected = original_count;
                            let mut expected_flags = 0x203u64;
                            for _ in 0..decrements {
                                unsafe {
                                    core::arch::asm!("stc", "dec {value:e}", "pushfq", "pop {flags}",
                                    value = inout(reg) expected, flags = lateout(reg) expected_flags);
                                }
                            }
                            assert_eq!(completed, loops * 4 + instruction);
                            assert_eq!(remaining, expected);
                            assert_eq!(flags & 0x8d5, expected_flags & 0x8d5);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_partial_loop_boundary_matches_scalar_retirements_and_native_flags() {
        for original_count in [3, 4, 16, 256, 1 << 63, (1 << 63) + 1, u64::MAX] {
            for decrement_index in 0..3 {
                let batch = CountedLoopBatch {
                    register: 1,
                    width: 64,
                    original_count,
                    iterations: 3,
                    decrement_index,
                };
                for loops in 0..3 {
                    for instruction in 0..4 {
                        let decrements = loops + u64::from(instruction > decrement_index);
                        let temporary = 3 - decrements;
                        let flags = if decrements == 0 {
                            0x203
                        } else {
                            native_dec_flags(temporary + 1)
                        };
                        let (completed, remaining, actual_flags) =
                            batch.account(instruction, 4, temporary, flags).unwrap();
                        assert_eq!(completed, loops * 4 + instruction);
                        assert_eq!(remaining, original_count - decrements);
                        let expected_flags = if decrements == 0 {
                            flags
                        } else {
                            native_dec_flags(remaining + 1)
                        };
                        assert_eq!(actual_flags & 0x8d5, expected_flags & 0x8d5);
                        assert_eq!(actual_flags & !0x8d4, flags & !0x8d4);
                    }
                }
                let (completed, remaining, flags) =
                    batch.account(4, 4, 0, native_dec_flags(1)).unwrap();
                assert_eq!(completed, 12);
                assert_eq!(remaining, original_count - 3);
                assert_eq!(flags & 0x8d5, native_dec_flags(remaining + 1) & 0x8d5);
                assert!(batch.account(4, 4, 1, 2).is_none());
                assert!(batch.account(0, 4, 0, 2).is_none());
                assert!(batch.account(0, 4, 4, 2).is_none());
            }
        }
    }
}

impl InstructionBatch {
    /// Private qualification on an AMD synthetic MTF exit: an NPT fetch
    /// guard stopped execution before entering another immutable code page.
    pub(crate) const PAGE_FETCH_BOUNDARY: u64 = 1 << 63;
    /// Synthetic page-execution stops can depend on host interrupts or NPT
    /// state. They are distinct from an exact hardware single-step.
    pub(crate) const PAGE_EXECUTION_BOUNDARY: u64 = 1 << 62;

    // AMD PMC overflow interrupts are asynchronous. Leave room for their
    // delivery before an exact deadline, then fail closed if one arrives late.
    // The validation host's largest observed lag was 135 retired instructions
    // over a 200M-TSC checkpoint and a complete Linux boot.
    pub const COUNTER_DEADLINE_MARGIN: u64 = 512;

    pub fn counter_period(&self) -> u64 {
        if self.counter_bounded {
            // The endpoint stops execution before the deadline. Count without
            // requesting an overflow interrupt in this short region.
            return 1 << 30;
        }
        self.instruction_budget
            .saturating_sub(Self::COUNTER_DEADLINE_MARGIN)
            .clamp(1, 1 << 30)
    }

    pub fn completed_at(&self, rip: u64) -> Option<u64> {
        if let Some(index) = self.branch_exits[..self.branch_exit_count]
            .iter()
            .position(|&target| target == rip)
        {
            return Some(u64::from(self.branch_exit_counts[index]));
        }
        let offset = rip.checked_sub(self.start)?;
        self.offsets[..=self.count]
            .iter()
            .position(|&value| u64::from(value) == offset)
            .map(|index| index as u64)
    }

    pub fn is_boundary(&self, rip: u64) -> bool {
        self.completed_at(rip).is_some()
            || self.branch_exits[..self.branch_exit_count].contains(&rip)
    }

    pub fn is_execution_stop(&self, rip: u64) -> bool {
        rip == self.endpoint() || self.branch_exits[..self.branch_exit_count].contains(&rip)
    }

    pub fn endpoint(&self) -> u64 {
        self.start + u64::from(self.offsets[self.count])
    }
}

/// Low-level VM entry (assembly), separated from the run loop for testability.
pub trait VmRunner {
    type Vmcs: VirtualMachineControlStructure;

    fn set_instruction_batch(&mut self, _batch: Option<InstructionBatch>) {}

    fn can_count_instructions(&self) -> bool {
        false
    }

    /// Guest page-table walks can read nested pages protected against writes.
    fn can_guard_page_tables(&self) -> bool {
        false
    }

    /// Exact count captured by a software-counted backend, including exits
    /// before a batch's endpoint. None when the instruction-counter object
    /// already reads its hardware accounting directly.
    fn completed_instructions(&self) -> Option<u64> {
        None
    }

    /// Guest MSRs saved by the backend rather than left in host registers.
    fn saved_guest_msr(&self, _vmcs: &Self::Vmcs, _index: u32) -> Option<u64> {
        None
    }

    /// One VM entry/exit cycle: load guest GPRs from `ctx`, VMLAUNCH/VMRESUME,
    /// save guest GPRs back on exit. `Ok` means a normal VM exit.
    ///
    /// # Safety
    ///
    /// VMCS must be loaded and configured, HOST_RSP must point to `ctx`,
    /// HOST_RIP to the exit handler, and interrupts must be in an appropriate state.
    unsafe fn run(
        &mut self,
        ctx: &mut super::VmxContext,
        vmcs: &Self::Vmcs,
    ) -> Result<(), VmEntryError>;
}
