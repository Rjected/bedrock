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

/// Low-level VM entry (assembly), separated from the run loop for testability.
pub trait VmRunner {
    type Vmcs: VirtualMachineControlStructure;

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
