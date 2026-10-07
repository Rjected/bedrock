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

/// Boundaries of a straight-line sequence, stopped before its endpoint.
/// Memory-accessing batches protect code against writes. Store batches also
/// protect translations, or validate the destination ranges before entry.
#[derive(Clone, Copy, Debug)]
pub struct InstructionBatch {
    pub start: u64,
    pub offsets: [u16; 65],
    pub count: usize,
    pub repeat: Option<RepeatBatch>,
    pub pages: [u64; 5],
    pub page_count: usize,
    pub accesses_memory: bool,
    pub writes_memory: bool,
    pub validated_stores: bool,
    pub looping: bool,
    pub loop_start: usize,
    pub instruction_budget: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct RepeatBatch {
    pub original_count: u64,
    pub iterations: u64,
}

impl InstructionBatch {
    // Interrupt latency is not precise on AMD. Leave a conservative margin
    // for stepping, then fail closed if an interrupt still arrives too late.
    pub const LOOP_DEADLINE_MARGIN: u64 = 65536;

    pub fn loop_period(&self) -> u64 {
        let length = (self.count - self.loop_start) as u64;
        (self
            .instruction_budget
            .saturating_sub(Self::LOOP_DEADLINE_MARGIN)
            .saturating_sub(self.loop_start as u64)
            / length)
            .clamp(1, 1 << 30)
    }

    pub fn completed_at(&self, rip: u64) -> Option<u64> {
        let offset = rip.checked_sub(self.start)?;
        self.offsets[..=self.count]
            .iter()
            .position(|&value| u64::from(value) == offset)
            .map(|index| index as u64)
    }

    pub fn endpoint(&self) -> u64 {
        self.start + u64::from(self.offsets[self.count])
    }

    /// A verified region has one backward conditional branch and no other
    /// transfers. Its branch counter and exit boundary determine exact work.
    pub fn loop_instructions(&self, branches: u64, rip: u64) -> Option<u64> {
        let index = self.completed_at(rip)?;
        let length = (self.count - self.loop_start) as u64;
        let partial = if index == self.count as u64 {
            if branches == 0 {
                return None;
            }
            self.loop_start as u64
        } else {
            index
        };
        branches.checked_mul(length)?.checked_add(partial)
    }
}

/// Low-level VM entry (assembly), separated from the run loop for testability.
pub trait VmRunner {
    type Vmcs: VirtualMachineControlStructure;

    fn set_instruction_batch(&mut self, _batch: Option<InstructionBatch>) {}

    fn can_count_instructions(&self) -> bool {
        false
    }

    /// Exact count captured by a software-counted backend, including exits
    /// before a batch's endpoint. None for hardware PMU accounting.
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
