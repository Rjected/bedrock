// SPDX-License-Identifier: GPL-2.0

//! AMD hardware backend. The common VM logic uses a VMCS field adapter while
//! this module owns SVM setup, contiguous intercept bitmaps, and VMRUN.

use super::c_helpers;
use super::svm_core::{
    exits, fields,
    vmcb::{offset as o, Vmcb},
};
use super::vmx::{VmEntryError, VmxContext};
use core::arch::asm;

pub(crate) fn supported() -> bool {
    use core::sync::atomic::{AtomicU8, Ordering};
    static SUPPORT: AtomicU8 = AtomicU8::new(0);
    match SUPPORT.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    // CPUID is unprivileged. LLVM reserves RBX, so preserve it explicitly.
    let ecx: u32;
    unsafe {
        asm!("push rbx", "cpuid", "pop rbx",
            inout("eax") 0x80000001u32 => _,
            lateout("ecx") ecx, lateout("edx") _, options(nomem));
    }
    let supported = ecx & (1 << 2) != 0;
    SUPPORT.store(if supported { 2 } else { 1 }, Ordering::Relaxed);
    supported
}

pub(crate) struct Bitmaps {
    msr: *mut core::ffi::c_void,
    io: *mut core::ffi::c_void,
}
// Owned kernel allocations, accessed only under the VM lock.
unsafe impl Send for Bitmaps {}
unsafe impl Sync for Bitmaps {}

impl Bitmaps {
    pub(crate) fn new() -> Option<Self> {
        let msr = unsafe { c_helpers::bedrock_svm_alloc_bitmap(1) };
        let io = unsafe { c_helpers::bedrock_svm_alloc_bitmap(2) };
        let b = Self { msr, io };
        if msr.is_null() || io.is_null() {
            return None;
        }
        // VMLOAD/VMSAVE and VMRUN own these architectural guest MSRs.
        // MSRPM has two bits per MSR (read/write), with the C000 range
        // starting at byte 0x800. All other MSRs remain intercepted.
        for index in [
            0x174u32, 0x175, 0x176, 0xc0000081, 0xc0000082, 0xc0000083, 0xc0000084, 0xc0000100,
            0xc0000101, 0xc0000102,
        ] {
            let bit = if index < 0x2000 {
                index * 2
            } else {
                0x800 * 8 + (index - 0xc0000000) * 2
            };
            unsafe {
                let byte = (msr as *mut u8).add((bit / 8) as usize);
                *byte &= !(3 << (bit % 8));
            }
        }
        Some(b)
    }
    pub(crate) fn bind(&self, v: &mut Vmcb) {
        v.write(o::MSRPM_BASE, 8, unsafe {
            c_helpers::bedrock_svm_bitmap_phys(self.msr)
        });
        v.write(o::IOPM_BASE, 8, unsafe {
            c_helpers::bedrock_svm_bitmap_phys(self.io)
        });
    }
}
impl Drop for Bitmaps {
    fn drop(&mut self) {
        unsafe {
            c_helpers::bedrock_svm_free_bitmap(self.msr, 1);
            c_helpers::bedrock_svm_free_bitmap(self.io, 2);
        }
    }
}

extern "C" {
    fn svm_run_guest(ctx: *mut VmxContext, guest_pa: u64, host_pa: u64) -> i32;
}

/// Caller holds the VM lock, is pinned, and has disabled local interrupts.
pub(crate) unsafe fn run(ctx: &mut VmxContext, v: &mut Vmcb, pa: u64) -> Result<(), VmEntryError> {
    v.write(o::RAX, 8, ctx.guest_rax);
    v.write(o::CR2, 8, ctx.guest_cr2);
    // Initially step every instruction. This gives a correctness reference
    // before introducing AMD PMU-overflow acceleration.
    let original_tf = v.read(o::RFLAGS, 8) & (1 << 8);
    v.write(o::RFLAGS, 8, v.read(o::RFLAGS, 8) | (1 << 8));
    let host_pa = unsafe { c_helpers::bedrock_svm_host_vmcb() };
    unsafe {
        svm_run_guest(ctx, pa, host_pa);
    }
    ctx.guest_rax = v.read(o::RAX, 8);
    ctx.guest_cr2 = v.read(o::CR2, 8);
    let code = v.read(o::EXIT_CODE, 8);
    #[cfg(kernel_log)]
    {
        static STEPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let n = STEPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if n < 16 || n % 100_000 == 0 {
            log_info!(
                "SVM progress: exit={} rip={:#x} code={:#x}\n",
                n,
                v.read(o::RIP, 8),
                code
            );
        }
    }
    let stepped = code == 0x41 && original_tf == 0 && v.read(o::DR6, 8) & (1 << 14) != 0;
    v.write(
        o::RFLAGS,
        8,
        (v.read(o::RFLAGS, 8) & !(1 << 8)) | original_tf,
    );
    let e = exits::decode(
        code,
        v.read(o::EXIT_INFO1, 8),
        v.read(o::EXIT_INFO2, 8),
        v.read(o::RIP, 8),
        v.read(o::NEXT_RIP, 8),
        stepped,
    )
    .map_err(|e| {
        log_err!("SVM unsupported exit: {:?}\n", e);
        VmEntryError::VmEntryFailed
    })?;
    fields::record_exit(v, &e);
    // Consumed injections must not be repeated on the next entry.
    let _ = fields::write(v, 0x4016, 0);
    Ok(())
}
