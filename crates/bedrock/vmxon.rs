// SPDX-License-Identifier: GPL-2.0

//! VMXON region, per-CPU VMX state, and VMX implementation.

use core::arch::asm;

use super::c_helpers;
use super::machine::LinuxMachine;
use super::memory::{HostPhysAddr, VirtAddr};
use super::page::KernelPage;
use super::vmx::traits::{Page as PageTrait, VmxCpu, VmxOnRegion};
use super::vmx::{InveptError, InvvpidError, Vmx, VmxCapabilities, VmxoffError, VmxonError};

/// VMXON region for a CPU. The page is intentionally leaked: the region must
/// persist for the whole VMX operation lifetime.
#[allow(dead_code)] // Fields are used via trait methods
pub(crate) struct RealVmxOnRegion {
    phys: HostPhysAddr,
    virt: VirtAddr,
}

impl VmxOnRegion for RealVmxOnRegion {
    type M = LinuxMachine;

    fn from_page(page: KernelPage) -> Self {
        let phys = page.physical_address();
        let virt = page.virtual_address();
        core::mem::forget(page);
        Self { phys, virt }
    }
}

/// Per-CPU VMX state (zero-sized). All access goes through C helpers using
/// `this_cpu_ptr()`, because Rust's `#[link_section = ".data..percpu"]` doesn't
/// generate proper per-CPU relocations like `DEFINE_PER_CPU()`.
pub(crate) struct RealVmxCpu;

// SAFETY: Only accessed from the owning CPU via C helpers.
unsafe impl Send for RealVmxCpu {}
// SAFETY: Only accessed from the owning CPU via C helpers; no mutable shared state.
unsafe impl Sync for RealVmxCpu {}

impl VmxCpu for RealVmxCpu {
    type M = LinuxMachine;
    type R = RealVmxOnRegion;

    fn capabilities(&self) -> &VmxCapabilities {
        // SAFETY: Called with preemption disabled; the pointer is valid for the
        // current CPU and the C struct has the same layout as VmxCapabilities.
        unsafe {
            let caps_ptr = c_helpers::bedrock_vcpu_get_capabilities();
            &*(caps_ptr.cast::<VmxCapabilities>())
        }
    }

    fn is_vmxon(&self) -> bool {
        // SAFETY: Called with preemption disabled.
        unsafe { c_helpers::bedrock_vcpu_is_vmxon() }
    }

    fn set_vmxon(&self, enabled: bool) {
        // SAFETY: Called with preemption disabled.
        unsafe { c_helpers::bedrock_vcpu_set_vmxon(enabled) }
    }

    fn set_capabilities(&self, caps: VmxCapabilities) {
        // SAFETY: Called during initialization with preemption disabled.
        unsafe {
            c_helpers::bedrock_vcpu_set_capabilities(
                caps.pin_based_exec_ctrl,
                caps.cpu_based_exec_ctrl,
                caps.cpu_based_exec_ctrl2,
                caps.vmexit_ctrl,
                caps.vmentry_ctrl,
                caps.cr0_fixed0,
                caps.cr0_fixed1,
                caps.cr4_fixed0,
                caps.cr4_fixed1,
                caps.has_ept,
                caps.has_vpid,
                caps.pebs_format,
                caps.pebs_baseline,
                caps.pebs_trap,
            );
        }
    }

    fn set_vmxon_region(&self, region: Self::R) {
        // SAFETY: Called during initialization with preemption disabled.
        unsafe {
            c_helpers::bedrock_vcpu_set_vmxon_region(region.phys.as_u64(), region.virt.as_u64());
        }
    }
}

static VCPU: RealVmxCpu = RealVmxCpu;

pub(crate) static mut BASIC_INFO: super::vmx::traits::VmxBasic = super::vmx::traits::VmxBasic {
    vmcs_revision_id: 0,
    vmcs_size: 0,
    mem_type_wb: false,
    io_exit_info: false,
    vmx_flex_controls: false,
};

/// Real VMX implementation using hardware instructions.
pub(crate) struct RealVmx;

impl Vmx for RealVmx {
    type M = LinuxMachine;

    fn is_supported() -> bool {
        // CPUID.1:ECX.VMX[bit 5]. rbx is reserved by LLVM, so save/restore it.
        let ecx: u32;
        let rbx_save: u64;
        // SAFETY: CPUID is a read-only instruction; we save/restore rbx around it.
        unsafe {
            asm!(
                "mov {0}, rbx",
                "cpuid",
                "mov rbx, {0}",
                out(reg) rbx_save,
                inout("eax") 1u32 => _,
                lateout("ecx") ecx,
                lateout("edx") _,
                options(nomem, nostack)
            );
        }
        let _ = rbx_save;
        (ecx & (1 << 5)) != 0
    }

    fn current_vcpu() -> &'static <Self::M as super::vmx::traits::Machine>::Vcpu {
        &VCPU
    }

    fn basic_info() -> &'static super::vmx::traits::VmxBasic {
        // SAFETY: Only written during initialization.
        #[allow(static_mut_refs)]
        unsafe {
            &BASIC_INFO
        }
    }

    fn set_basic_info(basic: super::vmx::traits::VmxBasic) {
        // SAFETY: Only called during initialization.
        unsafe {
            BASIC_INFO = basic;
        }
    }

    fn vmxon(phys_addr: HostPhysAddr) -> Result<(), VmxonError> {
        let addr = phys_addr.as_u64();
        let rflags: u64;
        // SAFETY: VMXON requires a valid physical address to the VMXON region; caller ensures this.
        unsafe {
            asm!(
                "vmxon [{0}]",
                "pushfq",
                "pop {1}",
                in(reg) &addr,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 {
            Err(VmxonError::InvalidPointer)
        } else if zf == 1 {
            Err(VmxonError::AlreadyInVmxOperation)
        } else {
            Ok(())
        }
    }

    fn vmxoff() -> Result<(), VmxoffError> {
        let rflags: u64;
        // SAFETY: VMXOFF is valid when the processor is in VMX root operation.
        unsafe {
            asm!(
                "vmxoff",
                "pushfq",
                "pop {0}",
                out(reg) rflags,
                options(nostack)
            );
        }

        let zf = (rflags >> 6) & 1;

        if zf == 1 {
            Err(VmxoffError::DualMonitorTreatmentActive)
        } else {
            Ok(())
        }
    }

    fn invept_single_context(eptp: u64) -> Result<(), InveptError> {
        // Bits 0-63: EPTP; bits 64-127 reserved (must be 0).
        #[repr(C, align(16))]
        struct InveptDescriptor {
            eptp: u64,
            reserved: u64,
        }

        let descriptor = InveptDescriptor { eptp, reserved: 0 };

        const INVEPT_TYPE_SINGLE_CONTEXT: u64 = 1;

        let rflags: u64;
        // SAFETY: INVEPT with a valid descriptor and type invalidates EPT translations.
        unsafe {
            asm!(
                "invept {0}, [{1}]",
                "pushfq",
                "pop {2}",
                in(reg) INVEPT_TYPE_SINGLE_CONTEXT,
                in(reg) &descriptor,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 {
            Err(InveptError::InvalidOperand)
        } else if zf == 1 {
            Err(InveptError::NotSupported)
        } else {
            Ok(())
        }
    }

    fn invvpid_single_context(vpid: u16) -> Result<(), InvvpidError> {
        // Bits 0-15: VPID (rest of the qword reserved); bits 64-127: linear
        // address (type 0 only).
        #[repr(C, align(16))]
        struct InvvpidDescriptor {
            vpid: u64,
            linear_address: u64,
        }

        let descriptor = InvvpidDescriptor {
            vpid: u64::from(vpid),
            linear_address: 0,
        };

        // Type 1: all entries for the specified VPID.
        const INVVPID_TYPE_SINGLE_CONTEXT: u64 = 1;

        let rflags: u64;
        // SAFETY: INVVPID with a valid descriptor and type invalidates VPID translations.
        unsafe {
            asm!(
                "invvpid {0}, [{1}]",
                "pushfq",
                "pop {2}",
                in(reg) INVVPID_TYPE_SINGLE_CONTEXT,
                in(reg) &descriptor,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 {
            Err(InvvpidError::InvalidOperand)
        } else if zf == 1 {
            Err(InvvpidError::NotSupported)
        } else {
            Ok(())
        }
    }

    fn invvpid_all_context() -> Result<(), InvvpidError> {
        // Descriptor is ignored for type 2.
        #[repr(C, align(16))]
        struct InvvpidDescriptor {
            vpid: u64,
            linear_address: u64,
        }

        let descriptor = InvvpidDescriptor {
            vpid: 0,
            linear_address: 0,
        };

        // Type 2: all VPIDs except 0.
        const INVVPID_TYPE_ALL_CONTEXT: u64 = 2;

        let rflags: u64;
        // SAFETY: INVVPID with type all-context invalidates all VPID translations.
        unsafe {
            asm!(
                "invvpid {0}, [{1}]",
                "pushfq",
                "pop {2}",
                in(reg) INVVPID_TYPE_ALL_CONTEXT,
                in(reg) &descriptor,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 {
            Err(InvvpidError::InvalidOperand)
        } else if zf == 1 {
            Err(InvvpidError::NotSupported)
        } else {
            Ok(())
        }
    }
}
