// SPDX-License-Identifier: GPL-2.0

//! `VmContext`: abstraction over VM state so exit handlers can be tested in userland.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::{
    CowAllocator, InstructionCounter, Kernel, Machine, Page, VirtualMachineControlStructure,
    VmGetRegistersError, VmRunError, VmRunner, VmSetRegistersError, Vmx,
};

use super::registers::{get_registers, set_registers};
use super::vm_run::{run, sync_gprs_from_vmx_ctx, sync_gprs_to_vmx_ctx};

/// Abstraction over VM state for testability.
///
/// Most state lives in `VmState` (via `state()`/`state_mut()`); memory access
/// is separate because it differs between root and forked VMs.
pub trait VmContext {
    type Vmcs: VirtualMachineControlStructure;
    type V: Vmx;
    type I: InstructionCounter;
    /// Page type for copy-on-write allocations (ForkedVm only).
    type CowPage: Page;

    fn state(&self) -> &VmState<Self::Vmcs, Self::I>;

    fn state_mut(&mut self) -> &mut VmState<Self::Vmcs, Self::I>;

    fn read_guest_memory(&self, gpa: GuestPhysAddr, buf: &mut [u8]) -> Result<(), MemoryError>;

    /// Compare guest bytes without requiring a full-page temporary buffer.
    /// Backends with direct guest page pointers can override the default.
    fn guest_memory_matches(
        &self,
        gpa: GuestPhysAddr,
        expected: &[u8],
    ) -> Result<bool, MemoryError> {
        let mut bytes = [0u8; 512];
        for (offset, chunk) in expected.chunks(512).enumerate() {
            let address = gpa
                .as_u64()
                .checked_add((offset * 512) as u64)
                .ok_or(MemoryError::OutOfRange)?;
            self.read_guest_memory(GuestPhysAddr::new(address), &mut bytes[..chunk.len()])?;
            if bytes[..chunk.len()] != *chunk {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn write_guest_memory(&mut self, gpa: GuestPhysAddr, buf: &[u8]) -> Result<(), MemoryError>;

    /// Fill in the pending log entry's memory_hash by hashing dirty pages found
    /// via the EPT. No-op without a pending entry.
    fn finalize_exit_record<K: Kernel>(&mut self, kernel: &K);

    // ========== Copy-on-Write Methods ==========

    /// Handle a write EPT violation on an R+X page. Forked VMs copy the parent
    /// page into a new one, remap it RWX and return `Some(Continue)` to retry.
    /// Returns `None` if COW is unsupported (root VMs) or the fault can't be handled.
    fn handle_cow_fault<A: CowAllocator<Self::CowPage>>(
        &mut self,
        _gpa: GuestPhysAddr,
        _allocator: &mut A,
    ) -> Option<ExitHandlerResult> {
        None
    }

    /// Whether this is a forked (copy-on-write) VM.
    fn is_forked(&self) -> bool {
        false
    }

    /// COW every page of feedback buffer `index` so a host mmap stays coherent.
    ///
    /// The mmap maps the buffer's current frames. A page still shared with the
    /// parent would be mapped at the parent's frame, and the next guest write
    /// would COW it elsewhere, leaving the mapping stale. Pre-COWing makes the
    /// mapped frame the one the guest writes to ("map once, keep running,
    /// re-read"). Already-COW'd pages are skipped so earlier guest writes are
    /// kept. Called from the mmap handler; no-op for root VMs.
    fn cow_feedback_buffer_for_mapping<A: CowAllocator<Self::CowPage>>(
        &mut self,
        _index: usize,
        _allocator: &mut A,
    ) {
    }

    /// Pre-COW the registered I/O channel page. Hypervisor writes from VMCALL
    /// handlers raise no EPT violation, so lazy COW never fires and
    /// [`VmContext::write_guest_memory`] would hit its "not COW'd yet" error.
    /// No-op for root VMs.
    fn pre_cow_io_channel_page<A: CowAllocator<Self::CowPage>>(&mut self, _allocator: &mut A) {}

    // ========== Register Methods ==========

    /// Requires the VMCS to be loaded.
    fn set_registers(&mut self, regs: &GuestRegisters) -> Result<(), VmSetRegistersError> {
        set_registers(self.state_mut(), regs)
    }

    /// Set guest registers with VMCS guarded load/clear.
    fn set_registers_guarded(&mut self, regs: &GuestRegisters) -> Result<(), VmSetRegistersError> {
        self.state()
            .vmcs
            .load()
            .map_err(VmSetRegistersError::VmcsGuard)?;

        let result = self.set_registers(regs);

        self.state()
            .vmcs
            .clear()
            .map_err(VmSetRegistersError::VmcsGuard)?;

        result
    }

    /// Requires the VMCS to be loaded.
    fn get_registers(&self) -> Result<GuestRegisters, VmGetRegistersError> {
        get_registers(self.state())
    }

    /// Get all guest registers with VMCS guarded load/clear.
    fn get_registers_guarded(&self) -> Result<GuestRegisters, VmGetRegistersError> {
        self.state()
            .vmcs
            .load()
            .map_err(VmGetRegistersError::VmcsGuard)?;

        let result = self.get_registers();

        self.state()
            .vmcs
            .clear()
            .map_err(VmGetRegistersError::VmcsGuard)?;

        result
    }

    // ========== GPR Sync Methods ==========

    /// Copy GPRs into the VmxContext; also sets up XSAVE area pointers.
    fn sync_gprs_to_vmx_ctx(&mut self) {
        sync_gprs_to_vmx_ctx(self.state_mut())
    }

    fn sync_gprs_from_vmx_ctx(&mut self) {
        sync_gprs_from_vmx_ctx(self.state_mut())
    }

    // ========== VM Run Methods ==========

    /// Run the VM until an exit requiring userspace handling. Swaps host/guest
    /// MSRs that lack VMCS fields around the run loop.
    ///
    /// # Safety
    ///
    /// VMCS (incl. HOST_RIP) must be configured, interrupts in an appropriate
    /// state, and preemption disabled so the CPU can't migrate across entry/exit.
    unsafe fn run<R: VmRunner<Vmcs = Self::Vmcs>, M: Machine, A: CowAllocator<Self::CowPage>>(
        &mut self,
        runner: &mut R,
        machine: &M,
        allocator: &mut A,
    ) -> Result<ExitReason, VmRunError>
    where
        Self: Sized,
    {
        // SAFETY: Caller upholds this method's safety contract.
        unsafe { run(self, runner, machine, allocator) }
    }
}
