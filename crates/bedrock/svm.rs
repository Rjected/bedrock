// SPDX-License-Identifier: GPL-2.0

//! AMD hardware backend. The common VM logic uses a VMCS field adapter while
//! this module owns SVM setup, contiguous intercept bitmaps, and VMRUN.

use super::c_helpers;
use super::svm_core::{
    exits, fields, pmu::PmcEntry,
    vmcb::{offset as o, Vmcb},
};
use super::vmx::{InstructionBatch, VmEntryError, VmxContext};
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

fn guest_xsaveopt_supported() -> bool {
    use core::sync::atomic::{AtomicU8, Ordering};
    static SUPPORT: AtomicU8 = AtomicU8::new(0);
    match SUPPORT.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    let features: u32;
    unsafe {
        asm!("push rbx", "cpuid", "pop rbx",
            inout("eax") 0xdu32 => features,
            inout("ecx") 1u32 => _, lateout("edx") _, options(nomem));
    }
    let supported = features & 1 != 0;
    SUPPORT.store(if supported { 2 } else { 1 }, Ordering::Relaxed);
    supported
}

pub(crate) struct Bitmaps {
    msr: *mut core::ffi::c_void,
    io: *mut core::ffi::c_void,
}

pub(crate) fn features() -> u32 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static FEATURES: AtomicU64 = AtomicU64::new(0);
    let cached = FEATURES.load(Ordering::Relaxed);
    if cached != 0 { return (cached - 1) as u32; }
    let features: u32;
    unsafe {
        asm!("push rbx", "cpuid", "pop rbx",
            inout("eax") 0x8000000au32 => _, inout("ecx") 0u32 => _,
            lateout("edx") features, options(nomem));
    }
    FEATURES.store(u64::from(features) + 1, Ordering::Relaxed);
    features
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
        let features = features();
        // The global gate excludes every active translation-table page from
        // trusted code, so ROGPT can update table A/D bits under write guards.
        v.configure_nested_features(features);
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
        breakpoints: *const u64,
        pmu_mask: u64,
        vmcb: *mut Vmcb,
    ) -> i32;
}

/// Caller holds the VM lock, is pinned, and has disabled local interrupts.
pub(crate) unsafe fn run(
    ctx: &mut VmxContext,
    v: &mut Vmcb,
    pa: u64,
    batch: Option<&super::vmx::InstructionBatch>,
) -> Result<u64, VmEntryError> {
    ctx.svm_guest_xsaveopt = u64::from(guest_xsaveopt_supported());
    v.write(o::RAX, 8, ctx.guest_rax);
    v.write(o::CR2, 8, ctx.guest_cr2);
    let entry_rip = v.read(o::RIP, 8);
    let original_tf = v.read(o::RFLAGS, 8) & (1 << 8);
    let original_dr7 = v.read(o::DR7, 8);
    let original_dr6 = v.read(o::DR6, 8);
    if let Some(repeat) = batch.and_then(|b| b.repeat) {
        ctx.guest_rcx = repeat.iterations;
    }
    if let Some(counted) = batch.and_then(|b| b.counted_loop) {
        match counted.register {
            1 => ctx.guest_rcx = counted.iterations,
            2 => ctx.guest_rdx = counted.iterations,
            _ => return Err(VmEntryError::VmEntryFailed),
        }
    }
    let mut breakpoints = [0u64; 4];
    if let Some(batch) = batch {
        let count = if batch.page_execution {
            breakpoints = batch.page_breakpoints;
            batch.page_breakpoint_count
        } else {
            let mut count = 0;
            if !batch.endpoint_intercepted {
                breakpoints[count] = batch.endpoint() + v.read(o::CS + 8, 8);
                count += 1;
            }
            for target in &batch.branch_exits[..batch.branch_exit_count] {
                breakpoints[count] = target + v.read(o::CS + 8, 8);
                count += 1;
            }
            count
        };
        v.write(o::DR7, 8, 0x400 | (0..count).fold(0u64, |mask, i| mask | (1 << (i * 2))));
        v.write(o::DR6, 8, original_dr6 & !0x400f);
    } else {
        v.write(o::RFLAGS, 8, v.read(o::RFLAGS, 8) | (1 << 8));
    }
    let host_pa = unsafe { c_helpers::bedrock_svm_host_vmcb() };
    let counting = batch.is_some_and(|b| b.uses_counter);
    let pmu_mask = unsafe { c_helpers::bedrock_svm_pmu_mask() };
    let counter = if let Some(batch) = batch.filter(|b| b.uses_counter) {
        let mut host_count = 0;
        if pmu_mask == 0
            || unsafe { c_helpers::bedrock_svm_pmu_arm(batch.counter_period(), &mut host_count) } != 0
        {
            kernel::pr_err!("SVM PMU arm failed: mask={:#x} period={} rip={:#x}\n", pmu_mask, batch.counter_period(), entry_rip);
            return Err(VmEntryError::VmEntryFailed);
        }
        Some(PmcEntry::prepare(v, batch.counter_period()).ok_or_else(|| {
            kernel::pr_err!("SVM PMC prepare failed: rip={:#x} period={} event={:#x} int={:#x} misc={:#x}\n", entry_rip, batch.counter_period(), v.read(o::EVENT_INJECTION, 4), v.read(o::INT_CONTROL, 4), v.read(o::INTERCEPT_MISC1, 4));
            VmEntryError::VmEntryFailed
        })?)
    } else {
        None
    };
    // Retain the serialized host PMU handoff around hardware autoswap.
    // The host event supplies readiness, not the guest retirement count.
    let guest_mask = if pmu_mask != 0 {
        (1 << 63) | if counting { pmu_mask } else { 0 }
    } else {
        0
    };
    if batch.is_some_and(|batch| batch.page_execution) {
        if let Some(counter) = counter.as_ref() {
            // TF is clear for guarded native execution. NPT catches PUSHF
            // writes to protected code or tables before scalar replay.
            counter.allow_native_pushf(v);
        }
    }
    unsafe {
        // Pass the virtual pointer as well as the physical address: VMRUN
        // modifies this allocation, which the compiler must see at the FFI
        // boundary. Assembly uses the physical address and ignores arg six.
        svm_run_guest(ctx, pa, host_pa, breakpoints.as_ptr(), guest_mask, v);
    }
    let mut host_count = 0;
    let pmu_ready = !counting || unsafe { c_helpers::bedrock_svm_pmu_read(&mut host_count) } == 0;
    let overflow_nmi = counter.as_ref().is_some_and(|_| PmcEntry::overflow_nmi(v));
    let retired = counter.and_then(|counter| counter.finish(v));
    if !pmu_ready {
        kernel::pr_err!("SVM PMU read failed: rip={:#x} code={:#x}\n", entry_rip, v.read(o::EXIT_CODE, 8));
        return Err(VmEntryError::VmEntryFailed);
    }
    #[cfg(kernel_log)]
    if counting {
        static SAMPLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        if SAMPLES.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 32 {
            log_info!(
                "SVM PMU: batch={:?} retired={:?} rip={:#x} exit={:#x}\n",
                batch.map(|b| (b.start, b.count, b.repeat)),
                retired,
                v.read(o::RIP, 8),
                v.read(o::EXIT_CODE, 8)
            );
        }
    }
    ctx.guest_rax = v.read(o::RAX, 8);
    ctx.guest_cr2 = v.read(o::CR2, 8);
    let code = v.read(o::EXIT_CODE, 8);
    if let Some(batch) = batch.filter(|b| b.global_execution) {
        v.write(o::DR7, 8, original_dr7);
        v.write(o::DR6, 8, original_dr6);
        v.write(o::RFLAGS, 8, (v.read(o::RFLAGS, 8) & !(1 << 8)) | original_tf);
        let count = retired.ok_or_else(|| {
            kernel::pr_err!("SVM global retired unavailable: entry={:#x} rip={:#x} code={:#x} raw={:#x} pmc={:#x} status={:#x}\n", entry_rip, v.read(o::RIP, 8), code, v.read(o::INSTR_RETIRED_CTR, 8), v.read(o::PERF_CTR0, 8), v.read(o::PERF_GLOBAL_STATUS, 8));
            VmEntryError::VmEntryFailed
        })?;
        if count > batch.instruction_budget {
            kernel::pr_err!("SVM global gate exceeded deadline: count={} budget={} code={:#x}\n",
                count, batch.instruction_budget, code);
            return Err(VmEntryError::VmEntryFailed);
        }
        if code == 0x61 {
            unsafe { core::arch::asm!("int $2", options(nomem, nostack)); }
        }
        // MOV SS can suppress the DR6 breakpoint bits for the following
        // instruction even when SVM reports the debug exit at its address.
        // Guest TF and DR7 were excluded before entering this gate, so the
        // protected hazard address itself is sufficient to request replay.
        let hazard_breakpoint = code == 0x41
            && batch.page_breakpoints[..batch.page_breakpoint_count]
                .contains(&v.read(o::RIP, 8));
        let replay = hazard_breakpoint || (0x20..=0x3f).contains(&code)
            || matches!(code, 0x66 | 0x6a | 0x74);
        if hazard_breakpoint {
            v.write(o::RFLAGS, 8, v.read(o::RFLAGS, 8) & !(1 << 16));
        }
        let mut e = if overflow_nmi || code == 0x61 || replay {
            let mut e = exits::decode(0x41, 0, 0, v.read(o::RIP, 8), 0, true)
                .map_err(|_| VmEntryError::VmEntryFailed)?;
            e.qualification = InstructionBatch::PAGE_EXECUTION_BOUNDARY
                | if replay { InstructionBatch::PAGE_SCALAR_REPLAY } else { 0 };
            e
        } else {
            exits::decode(code, v.read(o::EXIT_INFO1, 8), v.read(o::EXIT_INFO2, 8),
                v.read(o::RIP, 8), v.read(o::NEXT_RIP, 8), false)
                .map_err(|error| {
                    kernel::pr_err!("SVM global decode failed: error={:?} code={:#x} entry={:#x} rip={:#x}\n", error, code, entry_rip, v.read(o::RIP, 8));
                    VmEntryError::VmEntryFailed
                })?
        };
        if code == 0x400 {
            e.guest_physical_address = v.read(o::EXIT_INFO2, 8);
        }
        fields::record_exit(v, &e);
        let _ = fields::write(v, 0x4016, 0);
        return Ok(count);
    }
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
    if let Some(batch) = batch.filter(|b| b.page_execution) {
        // Free execution starts with TF and guest breakpoints disabled.
        // Guarded unsafe entries stop before they can restore guest TF mid-run.
        let trapped = code == 0x41 && v.read(o::DR6, 8) & 15 != 0
            && batch.page_breakpoints[..batch.page_breakpoint_count].contains(&v.read(o::RIP, 8));
        if code == 0x41 && !trapped {
            kernel::pr_err!("SVM page execution unexpected debug trap: rip={:#x} dr6={:#x}\n",
                v.read(o::RIP, 8), v.read(o::DR6, 8));
            return Err(VmEntryError::VmEntryFailed);
        }
        v.write(o::DR7, 8, original_dr7);
        v.write(o::DR6, 8, original_dr6);
        if trapped { v.write(o::RFLAGS, 8, v.read(o::RFLAGS, 8) & !(1 << 16)); }
        let count = retired.ok_or_else(|| {
            kernel::pr_err!("SVM page execution invalid count: code={:#x} entry_rip={:#x} rip={:#x}\n",
                code, entry_rip, v.read(o::RIP, 8));
            VmEntryError::VmEntryFailed
        })?;
        if count > batch.instruction_budget {
            kernel::pr_err!("SVM page execution exceeded deadline: count={} budget={} code={:#x}\n",
                count, batch.instruction_budget, code);
            return Err(VmEntryError::VmEntryFailed);
        }
        if code == 0x61 {
            // This is a host NMI, including perf counter overflow. A synthetic
            // boundary must still acknowledge it through the host handler.
            unsafe { core::arch::asm!("int $2", options(nomem, nostack)); }
        }
        // Guest exceptions retain their exit information: trap-style exits
        // can already have advanced RIP and must not be replaced by stepping.
        if (0x40..=0x5f).contains(&code) && !trapped {
            let e = exits::decode(code, v.read(o::EXIT_INFO1, 8), v.read(o::EXIT_INFO2, 8),
                v.read(o::RIP, 8), v.read(o::NEXT_RIP, 8), false)
                .map_err(|_| VmEntryError::VmEntryFailed)?;
            fields::record_exit(v, &e);
            return Ok(count);
        }
        // Replay an intercept or a guarded unsafe entry after accounting and
        // timers. Guest debug state was restored above before exposing it.
        let mut e = exits::decode(0x41, 0, 0, v.read(o::RIP, 8), 0, true)
            .map_err(|_| VmEntryError::VmEntryFailed)?;
        e.qualification = InstructionBatch::PAGE_EXECUTION_BOUNDARY;
        if code == 0x400 && v.read(o::EXIT_INFO1, 8) & 0x13 == 0x11
            && !batch.pages[..batch.code_page_count].contains(&(v.read(o::EXIT_INFO2, 8) & !4095)) {
            // A present, non-writing fetch hit the temporary NX guard. Save
            // its physical page so restore can distinguish a new instruction
            // from a split fetch, which still needs scalar replay.
            e.qualification |= InstructionBatch::PAGE_FETCH_BOUNDARY;
            e.guest_physical_address = v.read(o::EXIT_INFO2, 8);
        }
        fields::record_exit(v, &e);
        return Ok(count);
    }
    let debug_step = code == 0x41 && v.read(o::DR6, 8) & (1 << 14) != 0;
    let stepped = debug_step && original_tf == 0;
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
        code == 0x41 && v.read(o::DR6, 8) & 15 != 0 && b.is_execution_stop(v.read(o::RIP, 8))
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
        } else if let Some(counted) = batch.counted_loop {
            let counter = match counted.register {
                1 => ctx.guest_rcx,
                2 => ctx.guest_rdx,
                _ => return Err(VmEntryError::VmEntryFailed),
            };
            let instruction = batch.completed_at(v.read(o::RIP, 8))
                .ok_or_else(|| {
                    kernel::pr_err!("SVM counted loop invalid boundary: rip={:#x} code={:#x} counter={:#x} batch={:?}\n",
                        v.read(o::RIP, 8), code, counter, batch);
                    VmEntryError::VmEntryFailed
                })?;
            let (completed, remaining, flags) = counted
                .account(instruction, batch.count as u64, counter, v.read(o::RFLAGS, 8))
                .ok_or_else(|| {
                    kernel::pr_err!("SVM counted loop invalid accounting: rip={:#x} code={:#x} counter={:#x} batch={:?}\n",
                        v.read(o::RIP, 8), code, counter, batch);
                    VmEntryError::VmEntryFailed
                })?;
            if completed > batch.instruction_budget {
                return Err(VmEntryError::VmEntryFailed);
            }
            match counted.register {
                1 => ctx.guest_rcx = remaining,
                2 => ctx.guest_rdx = remaining,
                _ => return Err(VmEntryError::VmEntryFailed),
            }
            v.write(o::RFLAGS, 8, flags);
            if instruction == batch.count as u64 && remaining != 0 {
                v.write(o::RIP, 8, batch.start);
            }
            completed
        } else if batch.uses_counter {
            if retired.is_none() || !batch.is_boundary(v.read(o::RIP, 8)) {
                kernel::pr_err!("SVM PMU invalid boundary: retired={:?} rip={:#x} code={:#x} batch={:?}\n", retired, v.read(o::RIP,8), code, batch);
                return Err(VmEntryError::VmEntryFailed);
            }
            let count = retired.ok_or(VmEntryError::VmEntryFailed)?;
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
        u64::from(debug_step || (code == 0x41 && original_tf != 0 && original_dr7 & 0x20ff == 0))
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
