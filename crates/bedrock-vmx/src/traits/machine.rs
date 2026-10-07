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
    pub pages: [u64; 5],
    pub page_count: usize,
    pub accesses_memory: bool,
    pub writes_memory: bool,
    pub validated_stores: bool,
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

impl InstructionBatch {
    // Interrupt latency is not precise on AMD. Leave a conservative margin
    // for stepping, then fail closed if an interrupt still arrives too late.
    pub const COUNTER_DEADLINE_MARGIN: u64 = 65536;

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
        let offset = rip.checked_sub(self.start)?;
        self.offsets[..=self.count]
            .iter()
            .position(|&value| u64::from(value) == offset)
            .map(|index| index as u64)
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
