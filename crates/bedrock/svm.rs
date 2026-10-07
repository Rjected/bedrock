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

pub(crate) struct Counters;
impl Counters {
    pub(crate) fn new() -> Result<Self, kernel::error::Error> {
        if supported() && unsafe { c_helpers::bedrock_svm_pmu_init() } != 0 {
            kernel::pr_warn!("SVM PMU unavailable; using instruction stepping\n");
        }
        Ok(Self)
    }
}
impl Drop for Counters {
    fn drop(&mut self) {
        if supported() {
            unsafe {
                c_helpers::bedrock_svm_pmu_cleanup();
            }
        }
    }
}

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
        let features: u32;
        unsafe {
            asm!("push rbx", "cpuid", "pop rbx",
                inout("eax") 0x8000000au32 => _, inout("ecx") 0u32 => _,
                lateout("edx") features, options(nomem));
        }
        // Flush this guest's ASID rather than every host/guest translation.
        v.write(
            o::TLB_CONTROL,
            1,
            if features & (1 << 6) != 0 { 3 } else { 1 },
        );
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
    fn svm_run_guest(
        ctx: *mut VmxContext,
        guest_pa: u64,
        host_pa: u64,
        breakpoint: u64,
        pmu_mask: u64,
    ) -> i32;
}

/// Caller holds the VM lock, is pinned, and has disabled local interrupts.
pub(crate) unsafe fn run(
    ctx: &mut VmxContext,
    v: &mut Vmcb,
    pa: u64,
    batch: Option<&super::vmx::InstructionBatch>,
) -> Result<u64, VmEntryError> {
    v.write(o::RAX, 8, ctx.guest_rax);
    v.write(o::CR2, 8, ctx.guest_cr2);
    let original_tf = v.read(o::RFLAGS, 8) & (1 << 8);
    let original_dr7 = v.read(o::DR7, 8);
    let original_dr6 = v.read(o::DR6, 8);
    if let Some(repeat) = batch.and_then(|b| b.repeat) {
        ctx.guest_rcx = repeat.iterations;
    }
    let breakpoint = if let Some(batch) = batch {
        if batch.endpoint_intercepted {
            0 // The endpoint itself exits through an unconditional SVM intercept.
        } else {
            v.write(o::DR7, 8, 0x401); // DR0 local execution breakpoint.
            v.write(o::DR6, 8, original_dr6 & !0x400f);
            batch.endpoint() + v.read(o::CS + 8, 8)
        }
    } else {
        v.write(o::RFLAGS, 8, v.read(o::RFLAGS, 8) | (1 << 8));
        0
    };
    let host_pa = unsafe { c_helpers::bedrock_svm_host_vmcb() };
    let mut before = 0;
    let counting = batch.is_some_and(|b| b.uses_counter);
    let pmu_ready = if let Some(batch) = batch.filter(|b| b.uses_counter) {
        (unsafe { c_helpers::bedrock_svm_pmu_arm(batch.counter_period(), &mut before) }) == 0
    } else {
        false
    };
    let pmu_mask = unsafe { c_helpers::bedrock_svm_pmu_mask() };
    let guest_mask = if pmu_mask != 0 {
        (1 << 63) | if counting { pmu_mask } else { 0 }
    } else {
        0
    };
    unsafe {
        svm_run_guest(ctx, pa, host_pa, breakpoint, guest_mask);
    }
    let mut after = 0;
    let pmu_ready = pmu_ready && unsafe { c_helpers::bedrock_svm_pmu_read(&mut after) } == 0;
    #[cfg(kernel_log)]
    if pmu_ready {
        static SAMPLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        if SAMPLES.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 32 {
            log_info!(
                "SVM PMU: batch={:?} delta={} rip={:#x} exit={:#x}\n",
                batch.map(|b| (b.start, b.count, b.repeat)),
                after.wrapping_sub(before) & ((1 << 48) - 1),
                v.read(o::RIP, 8),
                v.read(o::EXIT_CODE, 8)
            );
        }
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
    if (0x20..=0x3f).contains(&code) {
        // Decode-assist supplies the GPR. Guest debug addresses have their own
        // VMCB shadow; they must never overwrite the host's DR0 breakpoint.
        let register = (v.read(o::EXIT_INFO1, 8) & 15) as usize;
        let dr = (code & 15) as usize;
        let offset = match dr {
            0..=3 => 0xe00 + dr * 8,
            6 => o::DR6,
            7 => o::DR7,
            _ => return Err(VmEntryError::VmEntryFailed),
        };
        if batch.is_some() || v.read(o::CPL, 1) != 0 || original_dr7 & (1 << 13) != 0 {
            return Err(VmEntryError::VmEntryFailed);
        }
        let mut rsp = v.read(o::RSP, 8);
        let mut registers = [
            &mut ctx.guest_rax,
            &mut ctx.guest_rcx,
            &mut ctx.guest_rdx,
            &mut ctx.guest_rbx,
            &mut rsp,
            &mut ctx.guest_rbp,
            &mut ctx.guest_rsi,
            &mut ctx.guest_rdi,
            &mut ctx.guest_r8,
            &mut ctx.guest_r9,
            &mut ctx.guest_r10,
            &mut ctx.guest_r11,
            &mut ctx.guest_r12,
            &mut ctx.guest_r13,
            &mut ctx.guest_r14,
            &mut ctx.guest_r15,
        ];
        let value = if register == 4 {
            v.read(o::RSP, 8)
        } else {
            *registers[register]
        };
        if code >= 0x30 {
            if dr == 7 && value & 0x20ff != 0 {
                // Active guest breakpoints/GD need a separate debug backend.
                return Err(VmEntryError::VmEntryFailed);
            }
            v.write(offset, 8, value);
        } else if register == 4 {
            v.write(o::RSP, 8, v.read(offset, 8));
        } else {
            *registers[register] = v.read(offset, 8);
        }
        let next = v.read(o::NEXT_RIP, 8);
        if !matches!(next.checked_sub(v.read(o::RIP, 8)), Some(3..=15)) {
            return Err(VmEntryError::VmEntryFailed);
        }
        v.write(o::RIP, 8, next);
        v.write(
            o::RFLAGS,
            8,
            (v.read(o::RFLAGS, 8) & !(1 << 8)) | original_tf,
        );
        let e =
            exits::decode(0x41, 0, 0, next, next, true).map_err(|_| VmEntryError::VmEntryFailed)?;
        fields::record_exit(v, &e);
        return Ok(1);
    }
    if stepped {
        v.write(o::DR6, 8, original_dr6);
    }
    let batch_stop = batch.is_some_and(|b| {
        code == 0x41 && v.read(o::DR6, 8) & 1 != 0 && v.read(o::RIP, 8) == b.endpoint()
    });
    // Replay the endpoint intercept on the next entry, as with an execution
    // breakpoint. Timer/deadline handling must run before that instruction's
    // emulation can change guest registers or produce device output.
    let natural_stop = batch.is_some_and(|b| {
        b.endpoint_intercepted && v.read(o::RIP, 8) == b.endpoint()
            && !matches!(code, 0x60 | 0x61 | 0x52)
    });
    let completed = if let Some(batch) = batch {
        v.write(o::DR7, 8, original_dr7);
        v.write(o::DR6, 8, original_dr6);
        if batch_stop || natural_stop {
            v.write(o::RFLAGS, 8, v.read(o::RFLAGS, 8) & !(1 << 16));
        }
        if let Some(repeat) = batch.repeat {
            let completed = repeat
                .iterations
                .checked_sub(ctx.guest_rcx)
                .ok_or(VmEntryError::VmEntryFailed)?;
            ctx.guest_rcx = repeat.original_count - completed;
            let rip = v.read(o::RIP, 8);
            if rip != batch.start && rip != batch.endpoint() {
                return Err(VmEntryError::VmEntryFailed);
            }
            if rip == batch.endpoint() && ctx.guest_rcx != 0 {
                v.write(o::RIP, 8, batch.start);
            }
            completed
        } else if batch.uses_counter {
            if !pmu_ready || batch.completed_at(v.read(o::RIP, 8)).is_none() {
                kernel::pr_err!("SVM PMU invalid boundary: ready={} before={} after={} rip={:#x} code={:#x} batch={:?}\n", pmu_ready, before, after, v.read(o::RIP,8), code, batch);
                return Err(VmEntryError::VmEntryFailed);
            }
            let count = exits::retired_instructions(before, after, code)
                .ok_or(VmEntryError::VmEntryFailed)?;
            if count > batch.instruction_budget
                || (batch.counter_bounded && count > batch.count as u64)
            {
                kernel::pr_err!(
                    "SVM PMU exceeded budget: count={} budget={} code={:#x}\n",
                    count,
                    batch.instruction_budget,
                    code
                );
                return Err(VmEntryError::VmEntryFailed);
            }
            count
        } else {
            batch.completed_at(v.read(o::RIP, 8)).ok_or_else(|| {
                kernel::pr_err!(
                    "SVM batch invalid boundary: rip={:#x} code={:#x} batch={:?}\n",
                    v.read(o::RIP, 8),
                    code,
                    batch
                );
                VmEntryError::VmEntryFailed
            })?
        }
    } else {
        u64::from(stepped)
    };
    v.write(
        o::RFLAGS,
        8,
        (v.read(o::RFLAGS, 8) & !(1 << 8)) | original_tf,
    );
    let target_stop = batch.is_some_and(|b| completed == b.instruction_budget)
        && matches!(code, 0x41 | 0x60 | 0x61);
    let e = exits::decode(
        if natural_stop || target_stop { 0x41 } else { code },
        v.read(o::EXIT_INFO1, 8),
        v.read(o::EXIT_INFO2, 8),
        v.read(o::RIP, 8),
        v.read(o::NEXT_RIP, 8),
        stepped || batch_stop || natural_stop || target_stop,
    )
    .map_err(|e| {
        kernel::pr_err!(
            "SVM unsupported exit: {:?}, rip={:#x}\n",
            e,
            v.read(o::RIP, 8)
        );
        VmEntryError::VmEntryFailed
    })?;
    fields::record_exit(v, &e);
    // Consumed injections must not be repeated on the next entry.
    let _ = fields::write(v, 0x4016, 0);
    Ok(completed)
}
