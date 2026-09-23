// SPDX-License-Identifier: GPL-2.0

//! Host state loaded on every VM exit (VMCS host-state area, SDM Vol 3C §26.5).

#[cfg(not(feature = "cargo"))]
use super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// Host CPU state to be written to the VMCS host-state area.
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct HostState {
    /// Host CR0 - system control flags.
    pub cr0: u64,
    /// Host CR3 - page table base address.
    pub cr3: u64,
    /// Host CR4 - architectural extension flags.
    pub cr4: u64,

    /// Host CS selector.
    pub cs_selector: u16,
    /// Host SS selector.
    pub ss_selector: u16,
    /// Host DS selector.
    pub ds_selector: u16,
    /// Host ES selector.
    pub es_selector: u16,
    /// Host FS selector.
    pub fs_selector: u16,
    /// Host GS selector.
    pub gs_selector: u16,
    /// Host TR (Task Register) selector.
    pub tr_selector: u16,

    /// Host FS base address.
    pub fs_base: u64,
    /// Host GS base address.
    pub gs_base: u64,
    /// Host TR base address - must point to a valid TSS.
    pub tr_base: u64,
    /// Host GDTR base address.
    pub gdtr_base: u64,
    /// Host IDTR base address.
    pub idtr_base: u64,

    /// IA32_SYSENTER_CS value.
    pub sysenter_cs: u32,
    /// IA32_SYSENTER_ESP value.
    pub sysenter_esp: u64,
    /// IA32_SYSENTER_EIP value.
    pub sysenter_eip: u64,

    /// IA32_EFER value.
    pub efer: u64,
    /// IA32_PAT value.
    pub pat: u64,
    /// IA32_MISC_ENABLE value (for guest emulation).
    pub misc_enable: u64,
    /// IA32_PLATFORM_INFO value (for guest emulation).
    pub platform_info: u64,

    /// SYSCALL/SYSRET MSRs (STAR, LSTAR, CSTAR, FMASK); static, not per-thread.
    pub syscall_msrs: SyscallMsrs,

    /// Host RSP - stack pointer for VM exit handler.
    pub rsp: u64,
    /// Host RIP - entry point of VM exit handler.
    pub rip: u64,
}

impl HostState {
    /// Capture host state from the current CPU. Call on the CPU where the VM
    /// will run, with preemption disabled. `rsp` may be 0 if set later.
    pub fn capture<C: CrAccess, M: MsrAccess, D: DescriptorTableAccess>(
        cr: &C,
        msr_access: &M,
        dt: &D,
        rip: u64,
        rsp: u64,
    ) -> Self {
        let cr0 = cr.read_cr0().map(|cr| cr.bits()).unwrap_or(0);
        let cr3 = cr.read_cr3().map(|cr| cr.bits()).unwrap_or(0);
        let cr4 = cr.read_cr4().map(|cr| cr.bits()).unwrap_or(0);

        let cs_selector = dt.read_cs().bits();
        let ss_selector = dt.read_ss().bits();
        let ds_selector = dt.read_ds().bits();
        let es_selector = dt.read_es().bits();
        let fs_selector = dt.read_fs().bits();
        let gs_selector = dt.read_gs().bits();
        let tr_selector = dt.read_tr().bits();

        let gdtr = dt.read_gdtr();
        let idtr = dt.read_idtr();

        let fs_base = msr_access.read_msr(msr::IA32_FS_BASE).unwrap_or(0);
        let gs_base = msr_access.read_msr(msr::IA32_GS_BASE).unwrap_or(0);
        let tr_base = dt.read_tr_base();

        let sysenter_cs = msr_access.read_msr(msr::IA32_SYSENTER_CS).unwrap_or(0) as u32;
        let sysenter_esp = msr_access.read_msr(msr::IA32_SYSENTER_ESP).unwrap_or(0);
        let sysenter_eip = msr_access.read_msr(msr::IA32_SYSENTER_EIP).unwrap_or(0);

        let efer = msr_access.read_msr(msr::IA32_EFER).unwrap_or(0);
        let pat = msr_access.read_msr(msr::IA32_PAT).unwrap_or(0);
        let misc_enable = msr_access.read_msr(msr::IA32_MISC_ENABLE).unwrap_or(0);
        let platform_info = msr_access.read_msr(msr::IA32_PLATFORM_INFO).unwrap_or(0);

        let syscall_msrs = SyscallMsrs::capture(msr_access);

        // Copy out of packed structs to avoid unaligned references.
        let gdtr_base = { gdtr.base };
        let idtr_base = { idtr.base };

        Self {
            cr0,
            cr3,
            cr4,
            cs_selector,
            ss_selector,
            ds_selector,
            es_selector,
            fs_selector,
            gs_selector,
            tr_selector,
            fs_base,
            gs_base,
            tr_base,
            gdtr_base,
            idtr_base,
            sysenter_cs,
            sysenter_esp,
            sysenter_eip,
            efer,
            pat,
            misc_enable,
            platform_info,
            syscall_msrs,
            rsp,
            rip,
        }
    }
}
