// SPDX-License-Identifier: GPL-2.0

//! VMX assembly support for guest entry/exit.

// VmxContext is defined in the vmx crate for testability.
pub(crate) use super::vmx::VmxContext;

use super::vmcs::RealVmcs;
use super::vmx::{VmEntryError, VmRunner};

extern "C" {
    /// Save host GPRs, load guest GPRs from `ctx`, and VMLAUNCH/VMRESUME; on
    /// exit, save guest GPRs and restore host. Returns 0 on VM exit, -1 on
    /// VM entry failure (check VMCS error fields).
    ///
    /// # Safety
    /// - VMCS must be loaded and properly configured before calling
    /// - HOST_RSP must point to `ctx`
    /// - HOST_RIP must point to `vmx_exit_handler`
    fn vmx_run_guest(ctx: *mut VmxContext) -> i32;

    /// HOST_RIP landing point for all VM exits; returns to vmx_run_guest's
    /// caller. Never call directly from Rust.
    fn vmx_exit_handler();
}

/// Kernel-specific VmxContext methods backed by vmx_support.S.
pub(crate) trait VmxContextExt {
    /// Get the address of vmx_exit_handler for use as HOST_RIP.
    fn exit_handler_addr() -> u64;

    /// Run the guest until a VM exit occurs.
    ///
    /// # Safety
    /// - VMCS must be loaded and properly configured
    /// - HOST_RSP in VMCS must point to this VmxContext
    /// - HOST_RIP in VMCS must point to vmx_exit_handler
    /// - Must be called with interrupts disabled
    unsafe fn run(&mut self) -> i32;
}

impl VmxContextExt for VmxContext {
    fn exit_handler_addr() -> u64 {
        vmx_exit_handler as *const () as u64
    }

    unsafe fn run(&mut self) -> i32 {
        // SAFETY: Caller guarantees the VMCS is configured with HOST_RSP = self
        // and HOST_RIP = vmx_exit_handler. RSP stays on the kernel stack during setup.
        unsafe { vmx_run_guest(self) }
    }
}

/// Kernel `VmRunner` backed by the assembly in vmx_support.S.
pub(crate) struct RealVmRunner {
    batch: Option<super::vmx::InstructionBatch>,
    completed: Option<u64>,
}

impl RealVmRunner {
    pub(crate) fn new() -> Self {
        Self {
            batch: None,
            completed: None,
        }
    }
}

impl VmRunner for RealVmRunner {
    type Vmcs = RealVmcs;

    fn set_instruction_batch(&mut self, batch: Option<super::vmx::InstructionBatch>) {
        self.batch = batch;
    }

    fn completed_instructions(&self) -> Option<u64> {
        self.completed
    }

    fn can_count_instructions(&self) -> bool {
        super::svm::supported() && unsafe { super::c_helpers::bedrock_svm_pmu_mask() } != 0
    }

    fn saved_guest_msr(&self, vmcs: &Self::Vmcs, index: u32) -> Option<u64> {
        if !super::svm::supported() {
            return None;
        }
        use super::svm_core::vmcb::{offset as o, Vmcb};
        use super::vmx::VirtualMachineControlStructure;
        let offset = match index {
            0xc0000081 => o::STAR,
            0xc0000082 => o::LSTAR,
            0xc0000083 => o::CSTAR,
            0xc0000084 => o::SFMASK,
            0xc0000102 => o::KERNEL_GS_BASE,
            _ => return None,
        };
        // Called after VMRUN with the VM lock held.
        let v = unsafe { &*(vmcs.vmcs_region_ptr().cast::<Vmcb>()) };
        Some(v.read(offset, 8))
    }

    unsafe fn run(&mut self, ctx: &mut VmxContext, vmcs: &Self::Vmcs) -> Result<(), VmEntryError> {
        self.completed = None;
        if super::svm::supported() {
            use super::vmx::VirtualMachineControlStructure;
            let v = unsafe { &mut *(vmcs.vmcs_region_ptr().cast::<super::svm_core::vmcb::Vmcb>()) };
            let completed =
                unsafe { super::svm::run(ctx, v, vmcs.svm_phys_addr(), self.batch.as_ref()) }?;
            self.completed = Some(completed);
            return Ok(());
        }
        // SAFETY: Caller guarantees the VMCS is loaded and configured
        // (HOST_RSP = ctx, HOST_RIP = vmx_exit_handler) with interrupts in the
        // appropriate state.
        let result = unsafe { ctx.run() };

        if result == 0 {
            Ok(())
        } else {
            Err(VmEntryError::VmEntryFailed)
        }
    }
}
