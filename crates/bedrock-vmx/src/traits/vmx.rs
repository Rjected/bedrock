#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::{
    cpu_based, pin_based, secondary_exec, vm_entry, vm_exit, HostPhysAddr, InveptError,
    InvvpidError, Kernel, Machine, Page, VmxBasic, VmxCapabilities,
    VmxConfigureFeatureControlError, VmxCpuInitError, VmxInitError, VmxoffError, VmxonAllocError,
    VmxonError,
};

/// Trait representing a VMXON region.
pub trait VmxOnRegion {
    type M: Machine;

    fn from_page(page: <<Self::M as Machine>::K as Kernel>::P) -> Self
    where
        Self: Sized;

    /// Allocate and initialize a new VMXON region.
    fn new(machine: &Self::M) -> Result<Self, VmxonAllocError>
    where
        Self: Sized,
    {
        let kernel = machine.kernel();
        let page = kernel.alloc_zeroed_page().ok_or_else(|| {
            log_err!("afailed to allocate VMXON region page\n");
            VmxonAllocError::MemoryAllocationFailed
        })?;

        let phys_addr = page.physical_address();
        log_debug!("VMXON region allocated at phys={:#x}\n", phys_addr.as_u64());

        let revision_id = <Self::M as Machine>::V::basic_info().vmcs_revision_id & 0x7fff_ffff;
        // SAFETY: page is a freshly-allocated zeroed page. Writing the 4-byte
        // revision ID at the start of the page is within bounds.
        unsafe {
            let region_ptr = page.virtual_address().as_u64() as *mut u32;
            *region_ptr = revision_id;
        }

        <Self::M as Machine>::V::vmxon(phys_addr).map_err(|e| {
            log_err!("VMXON instruction failed: {:?}\n", e);
            VmxonAllocError::VmxonFailed(e)
        })?;
        log_debug!("VMXON successful\n");

        Ok(Self::from_page(page))
    }
}

/// Trait representing global VMX operations.
pub trait Vmx {
    type M: Machine;

    /// Check if VMX is supported on this machine.
    fn is_supported() -> bool;

    /// Initialize VMX operation on all processors.
    fn initialize(machine: &Self::M) -> Result<(), VmxInitError>
    where
        Self: Sized,
    {
        log_info!("starting initialization\n");

        if !Self::is_supported() {
            log_err!("not supported on this CPU\n");
            return Err(VmxInitError::Unsupported);
        }
        log_debug!("CPU support verified\n");

        let basic_info = vmx_load_basic_info(machine.msr_access()).map_err(|e| {
            log_err!("failed to read basic info MSR: {:?}\n", e);
            VmxInitError::FailedToReadBasicInfo(e)
        })?;
        log_info!("{:?}\n", basic_info);
        Self::set_basic_info(basic_info);

        log_info!("initializing on all CPUs\n");
        let kernel = machine.kernel();
        kernel.call_on_all_cpus_with_data(
            machine,
            |machine: &Self::M| -> Result<(), VmxInitError> {
                let core_id = machine.kernel().current_cpu_id();
                log_debug!("initializing CPU {}\n", core_id);
                Self::current_vcpu().init(machine).map_err(|e| {
                    log_err!("failed to initialize CPU {}: {:?}\n", core_id, e);
                    VmxInitError::FailedToEnableCPU {
                        core: core_id,
                        error: e,
                    }
                })?;
                log_debug!("CPU {} initialized successfully\n", core_id);

                Ok(())
            },
        )?;

        log_info!("initialization complete\n");
        Ok(())
    }

    /// Get the Vcpu for the current processor ('static: per-cpu globals in production).
    fn current_vcpu() -> &'static <Self::M as Machine>::Vcpu;

    /// Get basic VMX information.
    fn basic_info() -> &'static VmxBasic;
    fn set_basic_info(basic: VmxBasic);

    /// Execute VMXON with the (4KB-aligned) VMXON region at `phys_addr`.
    /// Enters VMX operation with no current VMCS. SDM Vol 3C, "VMXON".
    fn vmxon(phys_addr: HostPhysAddr) -> Result<(), VmxonError>;

    /// Execute VMXOFF (leave VMX operation). SDM Vol 3C, "VMXOFF".
    fn vmxoff() -> Result<(), VmxoffError>;

    /// INVEPT single-context (type 1): invalidate EPT-derived translations for
    /// `eptp` only. Used e.g. for forks so they don't inherit the parent's stale
    /// TLB entries. SDM Vol 3C, "INVEPT".
    fn invept_single_context(eptp: u64) -> Result<(), InveptError>;

    /// INVVPID single-context (type 1): flush linear and combined translations
    /// tagged with `vpid` (must be nonzero). SDM Vol 3C, "INVVPID".
    fn invvpid_single_context(vpid: u16) -> Result<(), InvvpidError>;

    /// INVVPID all-context (type 2): flush translations for every VPID except 0.
    /// SDM Vol 3C, "INVVPID".
    fn invvpid_all_context() -> Result<(), InvvpidError>;

    /// Force CR0 to satisfy IA32_VMX_CR0_FIXED0/1 (SDM Vol 3C, Appendix A.7).
    fn fix_cr0(cr0: &Cr0, cap: &VmxCapabilities) -> Cr0 {
        Cr0::new((cr0.bits() | cap.cr0_fixed0) & cap.cr0_fixed1)
    }

    /// Force CR4 to satisfy IA32_VMX_CR4_FIXED0/1 (SDM Vol 3C, Appendix A.8).
    fn fix_cr4(cr4: &Cr4, cap: &VmxCapabilities) -> Cr4 {
        Cr4::new((cr4.bits() | cap.cr4_fixed0) & cap.cr4_fixed1)
    }
}

pub fn vmx_load_basic_info<M: MsrAccess>(msr: &M) -> Result<VmxBasic, MsrError> {
    let vmx_basic_msr = msr.read_msr(msr::IA32_VMX_BASIC)?;

    Ok(VmxBasic {
        vmcs_revision_id: (vmx_basic_msr & 0x7fff_ffff) as u32,
        vmcs_size: ((vmx_basic_msr >> 32) & 0x1fff) as u16,
        mem_type_wb: ((vmx_basic_msr >> 50) & 1) != 0,
        io_exit_info: ((vmx_basic_msr >> 54) & 1) != 0,
        vmx_flex_controls: ((vmx_basic_msr >> 55) & 1) != 0,
    })
}

/// Trait representing a VMX-capable virtual CPU core.
pub trait VmxCpu {
    type M: Machine;
    type R: VmxOnRegion<M = Self::M>;

    /// Get VMX capabilities.
    fn capabilities(&self) -> &VmxCapabilities;
    /// Check if VMX operation is enabled.
    fn is_vmxon(&self) -> bool;

    fn set_vmxon(&self, enabled: bool);
    fn set_capabilities(&self, caps: VmxCapabilities);
    fn set_vmxon_region(&self, region: Self::R);

    /// Enable VMX on this CPU: feature control, CR4.VMXE, VMXON, capabilities.
    fn init(&self, machine: &Self::M) -> Result<(), VmxCpuInitError> {
        assert!(!self.is_vmxon(), "VmxCpu is already initialized");

        log_debug!("configuring IA32_FEATURE_CONTROL MSR\n");
        Self::configure_feature_control(machine).map_err(|e| {
            log_err!("failed to configure feature control: {:?}\n", e);
            VmxCpuInitError::FeatureControlConfigFailed(e)
        })?;

        // set_vmxe() also updates the kernel's CR4 shadow (cpu_tlbstate.cr4). A raw
        // MOV to CR4 would desync it, and a later kernel CR4 write without VMXE #GPs.
        log_debug!("enabling VMXE in CR4\n");
        let cr = machine.cr_access();
        cr.set_vmxe().map_err(|e| {
            log_err!("failed to set CR4.VMXE: {:?}\n", e);
            VmxCpuInitError::FailedToEnableVMX(e)
        })?;

        log_debug!("allocating VMXON region\n");
        let vmxon_region = VmxOnRegion::new(machine).map_err(|e| {
            log_err!("failed to allocate VMXON region: {:?}\n", e);
            VmxCpuInitError::VmxonAllocFailed(e)
        })?;

        log_debug!("reading capabilities\n");
        let caps = Self::read_capabilities(machine);
        log_info!("{:?}\n", caps);

        self.set_vmxon(true);
        self.set_vmxon_region(vmxon_region);
        self.set_capabilities(caps);

        Ok(())
    }

    /// Adjust a control value by the capability MSR's allowed-0 (low, must be 1)
    /// and allowed-1 (high, may be 1) bits. SDM Vol 3C, Appendix A.3.
    fn adjust_controls(msr_value: u64, requested: u32) -> u32 {
        let allowed0 = msr_value as u32;
        let allowed1 = (msr_value >> 32) as u32;

        let mut adjusted = requested | allowed0;

        adjusted &= allowed1;

        adjusted
    }

    /// Read the VMX capability MSRs and compute the adjusted VMCS control values
    /// for the controls bedrock requests. A failed MSR read leaves that field at
    /// its default. SDM Vol 3C, Appendix A.
    fn read_capabilities<M: Machine>(machine: &M) -> VmxCapabilities {
        let msr = machine.msr_access();
        let mut cap = VmxCapabilities::default();

        // External interrupts exit so the host can service them; the preemption
        // timer guarantees periodic exits even if the guest spins.
        let requested =
            pin_based::EXT_INTR_EXITING | pin_based::NMI_EXITING | pin_based::PREEMPTION_TIMER;
        if let Ok(msr_value) = msr.read_msr(msr::IA32_VMX_PINBASED_CTLS) {
            cap.pin_based_exec_ctrl = Self::adjust_controls(msr_value, requested);
        }

        // CR3 load/store exiting is for determinism; the CR3 handler issues
        // INVVPID to keep the TLB coherent when the guest switches page tables.
        let requested = cpu_based::HLT_EXITING
            | cpu_based::MWAIT_EXITING
            | cpu_based::MONITOR_EXITING // so address-range monitoring is never armed
            | cpu_based::RDPMC_EXITING
            | cpu_based::RDTSC_EXITING   // also RDTSCP; deterministic time
            | cpu_based::USE_MSR_BITMAPS
            | cpu_based::ACTIVATE_SECONDARY_CONTROLS
            | cpu_based::UNCOND_IO_EXITING
            | cpu_based::CR3_LOAD_EXITING
            | cpu_based::CR3_STORE_EXITING
            | cpu_based::CR8_LOAD_EXITING
            | cpu_based::CR8_STORE_EXITING;
        if let Ok(msr_value) = msr.read_msr(msr::IA32_VMX_PROCBASED_CTLS) {
            cap.cpu_based_exec_ctrl = Self::adjust_controls(msr_value, requested);
        }

        if cap.cpu_based_exec_ctrl & cpu_based::ACTIVATE_SECONDARY_CONTROLS != 0 {
            let requested = secondary_exec::ENABLE_EPT
                | secondary_exec::ENABLE_VPID
                | secondary_exec::UNRESTRICTED_GUEST
                | secondary_exec::ENABLE_RDTSCP
                | secondary_exec::ENABLE_INVPCID
                | secondary_exec::RDRAND_EXITING
                | secondary_exec::RDSEED_EXITING;

            if let Ok(msr_value) = msr.read_msr(msr::IA32_VMX_PROCBASED_CTLS2) {
                cap.cpu_based_exec_ctrl2 = Self::adjust_controls(msr_value, requested);
            }

            cap.has_ept = cap.cpu_based_exec_ctrl2 & secondary_exec::ENABLE_EPT != 0;
            cap.has_vpid = cap.cpu_based_exec_ctrl2 & secondary_exec::ENABLE_VPID != 0;
        } else {
            cap.cpu_based_exec_ctrl2 = 0;
            cap.has_ept = false;
            cap.has_vpid = false;
        }

        // No ACK_INTR_ON_EXIT: on external-interrupt exits we briefly enable
        // interrupts and let the CPU deliver through the IDT (like KVM on SVM).
        let requested =
            vm_exit::HOST_ADDR_SPACE_SIZE | vm_exit::SAVE_IA32_EFER | vm_exit::LOAD_IA32_EFER;
        if let Ok(msr_value) = msr.read_msr(msr::IA32_VMX_EXIT_CTLS) {
            cap.vmexit_ctrl = Self::adjust_controls(msr_value, requested);
        }

        let requested = vm_entry::IA32E_MODE | vm_entry::LOAD_IA32_EFER;
        if let Ok(msr_value) = msr.read_msr(msr::IA32_VMX_ENTRY_CTLS) {
            cap.vmentry_ctrl = Self::adjust_controls(msr_value, requested);
        }

        if let Ok(value) = msr.read_msr(msr::IA32_VMX_CR0_FIXED0) {
            cap.cr0_fixed0 = value;
        }
        if let Ok(value) = msr.read_msr(msr::IA32_VMX_CR0_FIXED1) {
            cap.cr0_fixed1 = value;
        }
        if let Ok(value) = msr.read_msr(msr::IA32_VMX_CR4_FIXED0) {
            cap.cr4_fixed0 = value;
        }
        if let Ok(value) = msr.read_msr(msr::IA32_VMX_CR4_FIXED1) {
            cap.cr4_fixed1 = value;
        }

        // PEBS bits from IA32_PERF_CAPABILITIES; a read failure (no PMU
        // enumeration) means no PEBS. SDM Vol 3B §21.8 / Figure 21-67.
        if let Ok(value) = msr.read_msr(msr::IA32_PERF_CAPABILITIES) {
            cap.pebs_trap = (value >> 6) & 1 != 0;
            cap.pebs_format = ((value >> 8) & 0xF) as u8;
            cap.pebs_baseline = (value >> 14) & 1 != 0;
        }

        cap
    }

    /// Ensure IA32_FEATURE_CONTROL has lock (bit 0) and VMX-outside-SMX (bit 2) set.
    fn configure_feature_control<M: Machine>(
        machine: &M,
    ) -> Result<(), VmxConfigureFeatureControlError> {
        let mut feature_control = machine
            .msr_access()
            .read_msr(msr::IA32_FEATURE_CONTROL)
            .map_err(VmxConfigureFeatureControlError::MsrReadFailed)?;

        const FEAT_CTL_LOCKED: u64 = 1 << 0;
        const FEAT_CTL_VMX_ENABLED_OUTSIDE_SMX: u64 = 1 << 2;

        if (feature_control & FEAT_CTL_LOCKED) != 0
            && (feature_control & FEAT_CTL_VMX_ENABLED_OUTSIDE_SMX) != 0
        {
            return Ok(());
        }

        if (feature_control & FEAT_CTL_LOCKED) != 0 {
            return Err(VmxConfigureFeatureControlError::Locked);
        }

        feature_control |= FEAT_CTL_VMX_ENABLED_OUTSIDE_SMX;
        feature_control |= FEAT_CTL_LOCKED;

        machine
            .msr_access()
            .write_msr(msr::IA32_FEATURE_CONTROL, feature_control)
            .map_err(VmxConfigureFeatureControlError::MsrWriteFailed)?;

        Ok(())
    }

    fn deinitialize<M: Machine>(&self, machine: &M) -> Result<(), VmxoffError> {
        if self.is_vmxon() {
            log_debug!("executing VMXOFF\n");
            M::V::vmxoff().inspect_err(|&e| {
                log_err!("VMXOFF failed: {:?}\n", e);
            })?;

            // clear_vmxe() also updates the kernel's CR4 shadow.
            log_debug!("disabling VMXE in CR4\n");
            let cr = machine.cr_access();
            let _ = cr.clear_vmxe();

            self.set_vmxon(false);
            log_debug!("deinitialization complete\n");
        }

        Ok(())
    }
}
