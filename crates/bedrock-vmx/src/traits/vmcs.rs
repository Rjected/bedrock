#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::{
    allocate_vpid, cpu_based, errors::VmcsAllocError, pin_based, secondary_exec, HostPhysAddr,
    Kernel, Machine, Page, VmcsReadResult, VmcsWriteResult, Vmx, VmxCpu,
};

/// Operations on a VMCS.
///
/// Implementors uphold the VMX instruction safety invariants internally so
/// callers need no `unsafe`: reads/writes require the VMCS to be loaded, and
/// `physical_address` must be valid and 4KB-aligned.
pub trait VirtualMachineControlStructure: Sized {
    type P: Page;
    type M: Machine<P = Self::P>;

    /// VMCLEAR this VMCS.
    fn clear(&self) -> Result<(), &'static str>;

    /// VMPTRLD this VMCS.
    fn load(&self) -> Result<(), &'static str>;

    fn read16(&self, field: VmcsField16) -> VmcsReadResult<u16>;

    fn read32(&self, field: VmcsField32) -> VmcsReadResult<u32>;

    fn read64(&self, field: VmcsField64) -> VmcsReadResult<u64>;

    /// Natural-width fields are 64-bit on Intel 64 processors.
    fn read_natural(&self, field: VmcsFieldNatural) -> VmcsReadResult<u64>;

    fn write16(&self, field: VmcsField16, value: u16) -> VmcsWriteResult;

    fn write32(&self, field: VmcsField32, value: u32) -> VmcsWriteResult;

    fn write64(&self, field: VmcsField64, value: u64) -> VmcsWriteResult;

    fn write_natural(&self, field: VmcsFieldNatural, value: u64) -> VmcsWriteResult;

    /// Pointer to the VMCS region, used to copy VMCSs when forking. The format
    /// is implementation-specific, so both regions must share revision/format.
    ///
    /// # Safety
    ///
    /// Only valid for direct access after VMCLEAR has flushed the data to memory.
    fn vmcs_region_ptr(&self) -> *mut u8;

    /// Write the host-state area (loaded on every VM exit). VMCS must be loaded.
    /// SDM Vol 3C §26.5.
    fn setup_host_state(&self, host: &HostState) -> VmcsWriteResult {
        self.write_natural(VmcsFieldNatural::HostCr0, host.cr0)?;
        self.write_natural(VmcsFieldNatural::HostCr3, host.cr3)?;
        self.write_natural(VmcsFieldNatural::HostCr4, host.cr4)?;

        self.write16(VmcsField16::HostCsSelector, host.cs_selector)?;
        self.write16(VmcsField16::HostSsSelector, host.ss_selector)?;
        self.write16(VmcsField16::HostDsSelector, host.ds_selector)?;
        self.write16(VmcsField16::HostEsSelector, host.es_selector)?;
        self.write16(VmcsField16::HostFsSelector, host.fs_selector)?;
        self.write16(VmcsField16::HostGsSelector, host.gs_selector)?;
        self.write16(VmcsField16::HostTrSelector, host.tr_selector)?;

        self.write_natural(VmcsFieldNatural::HostFsBase, host.fs_base)?;
        self.write_natural(VmcsFieldNatural::HostGsBase, host.gs_base)?;
        self.write_natural(VmcsFieldNatural::HostTrBase, host.tr_base)?;
        self.write_natural(VmcsFieldNatural::HostGdtrBase, host.gdtr_base)?;
        self.write_natural(VmcsFieldNatural::HostIdtrBase, host.idtr_base)?;

        self.write32(VmcsField32::HostIa32SysenterCs, host.sysenter_cs)?;
        self.write_natural(VmcsFieldNatural::HostIa32SysenterEsp, host.sysenter_esp)?;
        self.write_natural(VmcsFieldNatural::HostIa32SysenterEip, host.sysenter_eip)?;

        self.write64(VmcsField64::HostIa32Efer, host.efer)?;
        self.write64(VmcsField64::HostIa32Pat, host.pat)?;

        self.write_natural(VmcsFieldNatural::HostRsp, host.rsp)?;
        self.write_natural(VmcsFieldNatural::HostRip, host.rip)?;

        // VMCS link pointer (must be ~0 for non-shadow VMCS)
        self.write64(VmcsField64::VmcsLinkPointer, !0u64)?;

        Ok(())
    }

    fn from_parts(page: Self::P, revision_id: u32) -> Self
    where
        Self: Sized;

    fn new(machine: &Self::M) -> Result<Self, VmcsAllocError>
    where
        Self: Sized,
    {
        let page = machine
            .kernel()
            .alloc_zeroed_page()
            .ok_or(VmcsAllocError::MemoryAllocationFailed)?;

        let revision_id = <Self::M as Machine>::V::basic_info().vmcs_revision_id & 0x7FFFFFFF;

        let ptr = page.virtual_address().as_u64() as *mut u32;
        // SAFETY: ptr is a valid pointer to the beginning of a freshly-allocated
        // zeroed 4KB page; writing 4 bytes at this address is within bounds.
        unsafe {
            core::ptr::write_volatile(ptr, revision_id);
        }

        log_info!(
            "Allocated VMCS page at physical address {:x}",
            page.physical_address().as_u64()
        );

        Ok(Self::from_parts(page, revision_id))
    }

    /// Write VM-execution, VM-exit and VM-entry controls (SDM Vol 3C ch. 26).
    /// VMCS must be loaded. `msr_bitmap_addr` is used only if USE_MSR_BITMAPS is set.
    fn setup_controls(&self, msr_bitmap_addr: Option<HostPhysAddr>) -> Result<(), VmcsSetupError> {
        let vcpu = <Self::M as Machine>::V::current_vcpu();
        let caps = vcpu.capabilities();

        // SDM Vol 3C §26.6.9
        if caps.cpu_based_exec_ctrl & cpu_based::USE_MSR_BITMAPS != 0 {
            if let Some(addr) = msr_bitmap_addr {
                self.write64(VmcsField64::MsrBitmapAddr, addr.as_u64())
                    .map_err(VmcsSetupError::Controls)?;
            }
        }

        self.write32(
            VmcsField32::PinBasedVmExecControls,
            caps.pin_based_exec_ctrl,
        )
        .map_err(VmcsSetupError::Controls)?;

        // ~10ms on typical hardware (TSC rate / VMX_MISC[4:0] divisor; SDM §26.4,
        // §27.5.1). Not deterministic (wall-clock based): only a heartbeat so a
        // spinning guest still exits periodically.
        if caps.pin_based_exec_ctrl & pin_based::PREEMPTION_TIMER != 0 {
            self.write32(VmcsField32::VmxPreemptionTimerValue, 0x100000)
                .map_err(VmcsSetupError::Controls)?;
        }

        self.write32(
            VmcsField32::PrimaryProcBasedVmExecControls,
            caps.cpu_based_exec_ctrl,
        )
        .map_err(VmcsSetupError::Controls)?;

        log_info!(
            "CPU-based VM-exec controls = 0x{:08x}",
            caps.cpu_based_exec_ctrl
        );

        if caps.cpu_based_exec_ctrl & cpu_based::ACTIVATE_SECONDARY_CONTROLS != 0 {
            self.write32(
                VmcsField32::SecondaryProcBasedVmExecControls,
                caps.cpu_based_exec_ctrl2,
            )
            .map_err(VmcsSetupError::Controls)?;

            // Unique per-VM VPID for TLB isolation; must not be 0 (reserved for
            // VMX root, SDM Vol 3C §28.2.1.1).
            if caps.cpu_based_exec_ctrl2 & secondary_exec::ENABLE_VPID != 0 {
                let vpid = allocate_vpid();
                self.write16(VmcsField16::VirtualProcessorId, vpid)
                    .map_err(VmcsSetupError::Controls)?;

                // Flush stale entries from previous users of this VPID; text_poke
                // and similar code rely on TLB coherency after CR3 switches.
                if let Err(e) = <Self::M as Machine>::V::invvpid_single_context(vpid) {
                    log_err!("INVVPID failed for VPID {}: {:?}", vpid, e);
                    // Not fatal: INVVPID may be unsupported under nested virt.
                }

                log_info!("Allocated VPID={}", vpid);
            }
        }

        self.write32(VmcsField32::PrimaryVmExitControls, caps.vmexit_ctrl)
            .map_err(VmcsSetupError::Controls)?;

        self.write32(VmcsField32::VmEntryControls, caps.vmentry_ctrl)
            .map_err(VmcsSetupError::Controls)?;

        // Only intercept #MC (vector 18), like bhyve; the guest handles its own
        // exceptions. SDM Vol 3C §26.6.3.
        self.write32(VmcsField32::ExceptionBitmap, 1 << 18) // #MC only
            .map_err(VmcsSetupError::Controls)?;

        // Mask the VMX-constrained CR0/CR4 bits (must-be-1 | must-be-0, as in bhyve):
        // guest writes to them exit and reads return the shadow. SDM Vol 3C §26.6.6.
        let cr0_ones_mask = caps.cr0_fixed0 & caps.cr0_fixed1;
        let cr0_zeros_mask = !caps.cr0_fixed0 & !caps.cr0_fixed1;
        let cr0_mask = cr0_ones_mask | cr0_zeros_mask;

        let cr4_ones_mask = caps.cr4_fixed0 & caps.cr4_fixed1;
        let cr4_zeros_mask = !caps.cr4_fixed0 & !caps.cr4_fixed1;
        let cr4_mask = cr4_ones_mask | cr4_zeros_mask;

        self.write_natural(VmcsFieldNatural::Cr0GuestHostMask, cr0_mask)
            .map_err(VmcsSetupError::Controls)?;
        self.write_natural(VmcsFieldNatural::Cr4GuestHostMask, cr4_mask)
            .map_err(VmcsSetupError::Controls)?;

        // Updated when the guest writes CR0/CR4.
        self.write_natural(VmcsFieldNatural::Cr0ReadShadow, 0)
            .map_err(VmcsSetupError::Controls)?;
        self.write_natural(VmcsFieldNatural::Cr4ReadShadow, 0)
            .map_err(VmcsSetupError::Controls)?;

        self.write32(VmcsField32::PageFaultErrorCodeMask, 0)
            .map_err(VmcsSetupError::Controls)?;
        self.write32(VmcsField32::PageFaultErrorCodeMatch, 0)
            .map_err(VmcsSetupError::Controls)?;

        self.write32(VmcsField32::Cr3TargetCount, 0)
            .map_err(VmcsSetupError::Controls)?;

        self.write32(VmcsField32::VmEntryInterruptionInfo, 0)
            .map_err(VmcsSetupError::Controls)?;

        Ok(())
    }

    fn setup(
        &self,
        ept_pointer: u64,
        msr_bitmap_addr: Option<HostPhysAddr>,
        host: &HostState,
    ) -> Result<(), VmcsSetupError> {
        self.clear().map_err(VmcsSetupError::Clear)?;

        {
            let _guard = VmcsGuard::new(self).map_err(VmcsSetupError::Guard)?;

            self.setup_host_state(host)
                .map_err(VmcsSetupError::HostState)?;

            self.setup_controls(msr_bitmap_addr)?;

            self.write64(VmcsField64::EptPointer, ept_pointer)
                .map_err(VmcsSetupError::EptPointer)?;
            log_info!("Configured EPTP=0x{:x}", ept_pointer);
        }

        log_info!("VMCS setup complete (EPTP=0x{:x})", ept_pointer);
        Ok(())
    }
}

/// RAII guard that loads a VMCS on creation and clears it on drop.
pub struct VmcsGuard<'a, T: VirtualMachineControlStructure> {
    vmcs: &'a T,
}

impl<'a, T: VirtualMachineControlStructure> VmcsGuard<'a, T> {
    /// No other VMCS should be loaded while the guard is alive.
    pub fn new(vmcs: &'a T) -> Result<Self, &'static str> {
        vmcs.load()?;
        Ok(Self { vmcs })
    }
}

impl<'a, T: VirtualMachineControlStructure> Drop for VmcsGuard<'a, T> {
    fn drop(&mut self) {
        let _ = self.vmcs.clear();
    }
}
