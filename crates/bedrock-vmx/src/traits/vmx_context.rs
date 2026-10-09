// SPDX-License-Identifier: GPL-2.0

//! VMX context for guest/host register switching, shared with assembly.

/// Guest/host GPRs saved/restored around VM entry/exit. Layout must match
/// vmx_support.S exactly.
///
/// No RSP: guest RSP lives in the VMCS and host RSP points at this struct.
#[repr(C)]
pub struct VmxContext {
    // Guest GPRs (offsets 0-112)
    pub guest_rax: u64,
    pub guest_rbx: u64,
    pub guest_rcx: u64,
    pub guest_rdx: u64,
    pub guest_rsi: u64,
    pub guest_rdi: u64,
    pub guest_rbp: u64,
    pub guest_r8: u64,
    pub guest_r9: u64,
    pub guest_r10: u64,
    pub guest_r11: u64,
    pub guest_r12: u64,
    pub guest_r13: u64,
    pub guest_r14: u64,
    pub guest_r15: u64,

    // Host GPRs (offsets 120-232)
    pub host_rax: u64,
    pub host_rbx: u64,
    pub host_rcx: u64,
    pub host_rdx: u64,
    pub host_rsi: u64,
    pub host_rdi: u64,
    pub host_rbp: u64,
    pub host_r8: u64,
    pub host_r9: u64,
    pub host_r10: u64,
    pub host_r11: u64,
    pub host_r12: u64,
    pub host_r13: u64,
    pub host_r14: u64,
    pub host_r15: u64,

    // 0 = VMLAUNCH, 1 = VMRESUME (offset 240)
    pub launched: u32,

    // offset 244
    pub _pad: u32,

    // 64-byte aligned XsaveArea pointers (offsets 248, 256); 0 skips XSAVE.
    pub guest_xsave_ptr: u64,
    pub host_xsave_ptr: u64,

    // XSAVE/XRSTOR component mask, also loaded into XCR0 while the guest runs
    // so XGETBV returns it (offset 264).
    pub xcr0_mask: u64,

    // Saved on VM entry, restored on VM exit (offset 272).
    pub host_xcr0: u64,

    // CR2 is not in the VMCS, so it's swapped manually (offset 280).
    pub guest_cr2: u64,

    // AMD SVM guest save: 0 = unsupported/uninitialized, 1 = first full XSAVE
    // pending, 2 = XSAVEOPT permitted (offset 288).
    pub svm_guest_xsaveopt: u64,

    // A kernel FPU reservation protects host state across a short SVM run
    // group, allowing assembly to omit its per-entry host XSAVE/XRSTOR.
    pub svm_host_fpu_reserved: u64,
}

const _: () = assert!(core::mem::offset_of!(VmxContext, svm_host_fpu_reserved) == 296);

impl Default for VmxContext {
    fn default() -> Self {
        Self::new()
    }
}

impl VmxContext {
    pub const fn new() -> Self {
        Self {
            guest_rax: 0,
            guest_rbx: 0,
            guest_rcx: 0,
            guest_rdx: 0,
            guest_rsi: 0,
            guest_rdi: 0,
            guest_rbp: 0,
            guest_r8: 0,
            guest_r9: 0,
            guest_r10: 0,
            guest_r11: 0,
            guest_r12: 0,
            guest_r13: 0,
            guest_r14: 0,
            guest_r15: 0,
            host_rax: 0,
            host_rbx: 0,
            host_rcx: 0,
            host_rdx: 0,
            host_rsi: 0,
            host_rdi: 0,
            host_rbp: 0,
            host_r8: 0,
            host_r9: 0,
            host_r10: 0,
            host_r11: 0,
            host_r12: 0,
            host_r13: 0,
            host_r14: 0,
            host_r15: 0,
            launched: 0,
            _pad: 0,
            guest_xsave_ptr: 0,
            host_xsave_ptr: 0,
            xcr0_mask: 0,
            host_xcr0: 0,
            guest_cr2: 0,
            svm_guest_xsaveopt: 0,
            svm_host_fpu_reserved: 0,
        }
    }
}
