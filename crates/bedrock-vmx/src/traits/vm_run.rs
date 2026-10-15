// SPDX-License-Identifier: GPL-2.0

//! VM run loop and GPR synchronization.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::{
    CowAllocator, InstructionCounter, IrqGuard, Kernel, Machine, Page, ReverseIrqGuard,
    VirtualMachineControlStructure, VmContext, VmRunError, VmRunner,
};

// ========== GPR Sync Methods ==========

/// Copy GPRs into `VmxContext` and set up the XSAVE area pointers.
pub fn sync_gprs_to_vmx_ctx<V, I>(state: &mut VmState<V, I>)
where
    V: VirtualMachineControlStructure,
    I: InstructionCounter,
{
    state.vmx_ctx.guest_rax = state.gprs.rax;
    state.vmx_ctx.guest_rbx = state.gprs.rbx;
    state.vmx_ctx.guest_rcx = state.gprs.rcx;
    state.vmx_ctx.guest_rdx = state.gprs.rdx;
    state.vmx_ctx.guest_rsi = state.gprs.rsi;
    state.vmx_ctx.guest_rdi = state.gprs.rdi;
    state.vmx_ctx.guest_rbp = state.gprs.rbp;
    state.vmx_ctx.guest_r8 = state.gprs.r8;
    state.vmx_ctx.guest_r9 = state.gprs.r9;
    state.vmx_ctx.guest_r10 = state.gprs.r10;
    state.vmx_ctx.guest_r11 = state.gprs.r11;
    state.vmx_ctx.guest_r12 = state.gprs.r12;
    state.vmx_ctx.guest_r13 = state.gprs.r13;
    state.vmx_ctx.guest_r14 = state.gprs.r14;
    state.vmx_ctx.guest_r15 = state.gprs.r15;
    // RSP lives in the VMCS.

    state.vmx_ctx.guest_xsave_ptr = state.guest_xsave_page.virtual_address().as_u64();
    state.vmx_ctx.host_xsave_ptr = state.host_xsave_page.virtual_address().as_u64();
    state.vmx_ctx.xcr0_mask = state.xcr0_mask;
}

/// Copy GPRs out of `VmxContext`.
pub fn sync_gprs_from_vmx_ctx<V, I>(state: &mut VmState<V, I>)
where
    V: VirtualMachineControlStructure,
    I: InstructionCounter,
{
    if let Ok(rsp) = state.vmcs.read_natural(VmcsFieldNatural::GuestRsp) {
        state.gprs.rsp = rsp;
    }
    state.gprs.rax = state.vmx_ctx.guest_rax;
    state.gprs.rbx = state.vmx_ctx.guest_rbx;
    state.gprs.rcx = state.vmx_ctx.guest_rcx;
    state.gprs.rdx = state.vmx_ctx.guest_rdx;
    state.gprs.rsi = state.vmx_ctx.guest_rsi;
    state.gprs.rdi = state.vmx_ctx.guest_rdi;
    state.gprs.rbp = state.vmx_ctx.guest_rbp;
    state.gprs.r8 = state.vmx_ctx.guest_r8;
    state.gprs.r9 = state.vmx_ctx.guest_r9;
    state.gprs.r10 = state.vmx_ctx.guest_r10;
    state.gprs.r11 = state.vmx_ctx.guest_r11;
    state.gprs.r12 = state.vmx_ctx.guest_r12;
    state.gprs.r13 = state.vmx_ctx.guest_r13;
    state.gprs.r14 = state.vmx_ctx.guest_r14;
    state.gprs.r15 = state.vmx_ctx.guest_r15;
}

// ========== VM Run Methods ==========

fn restore_held_clock_execute<Ctx, A>(
    ctx: &mut Ctx,
    allocator: &A,
    first: &mut Option<NptExecuteGuard>,
    paired: &mut [Option<NptExecuteGuard>; 3],
)
where
    Ctx: VmContext,
    A: CowAllocator<Ctx::CowPage>,
{
    if let Some(execute) = first.take() {
        execute.restore(&mut ctx.state_mut().ept, allocator);
    }
    for execute in paired.iter_mut().filter_map(Option::take) {
        execute.restore(&mut ctx.state_mut().ept, allocator);
    }
}

/// Run the VM until an exit requiring userspace handling. Swaps the MSRs that
/// have no VMCS fields around the run loop.
///
/// # Safety
///
/// VMCS (including HOST_RIP) must be configured, interrupts in an appropriate
/// state, and preemption disabled so the thread cannot migrate.
pub unsafe fn run<Ctx, R, M, A>(
    ctx: &mut Ctx,
    runner: &mut R,
    machine: &M,
    allocator: &mut A,
) -> Result<ExitReason, VmRunError>
where
    Ctx: VmContext,
    R: VmRunner<Vmcs = Ctx::Vmcs>,
    M: Machine,
    A: CowAllocator<Ctx::CowPage>,
{
    let msr = machine.msr_access();

    // Save host KERNEL_GS_BASE (per-thread, changes between runs).
    let host_kernel_gs_base = msr.read_msr(msr::IA32_KERNEL_GS_BASE).unwrap_or(0);

    // No VMCS fields for these; SYSCALL/SYSRET/SWAPGS read hardware directly.
    if !Ctx::V::uses_nested_paging() {
        ctx.state().msr_state.syscall.load(msr);
    }

    ctx.state().vmcs.load().map_err(VmRunError::VmcsLoad)?;

    // EPT TLB entries are per-LP and survive VM entry/exit (SDM Vol 3C
    // §30.4.3.2); cross-LP propagation is software's job (§30.4.3.4). Between
    // ioctls the thread can migrate, and CoW remaps made elsewhere may leave
    // this CPU caching stale parent HPAs. EPT violations only flush entries
    // that block the access; a permitted read through a stale entry silently
    // returns parent data (§30.4.2). So INVEPT whenever the CPU changed.
    let cur_cpu = machine.kernel().current_cpu_id() as u32;
    if ctx.state().last_cpu != Some(cur_cpu) {
        let eptp = ctx.state().ept.eptp();
        <Ctx::V as Vmx>::invept_single_context(eptp).map_err(VmRunError::InveptFailed)?;
        ctx.state_mut().last_cpu = Some(cur_cpu);
    }

    // Requires the VMCS to be loaded.
    ctx.state().apply_intercept_pf();

    // Otherwise single-stepping would only start at the first exit.
    update_mtf_state(ctx).map_err(VmRunError::ExitHandler)?;

    ctx.sync_gprs_to_vmx_ctx();
    runner.begin_run(&mut ctx.state_mut().vmx_ctx);
    // SAFETY: Caller guarantees VMCS is properly configured
    let result = unsafe { run_loop(ctx, runner, machine, allocator, host_kernel_gs_base) };
    runner.finish_run(&mut ctx.state_mut().vmx_ctx);

    let clear_result = ctx.state().vmcs.clear();

    // After VMCLEAR the next entry must be VMLAUNCH.
    ctx.state_mut().vmx_ctx.launched = 0;

    // kernel_gs_base is already saved by run_loop on every exit.
    ctx.state_mut().msr_state.syscall = if Ctx::V::uses_nested_paging() {
        let vmcs = &ctx.state().vmcs;
        SyscallMsrs {
            star: Star::new(runner.saved_guest_msr(vmcs, msr::IA32_STAR).unwrap_or(0)),
            lstar: Lstar::new(runner.saved_guest_msr(vmcs, msr::IA32_LSTAR).unwrap_or(0)),
            cstar: Cstar::new(runner.saved_guest_msr(vmcs, msr::IA32_CSTAR).unwrap_or(0)),
            fmask: Fmask::new(runner.saved_guest_msr(vmcs, msr::IA32_FMASK).unwrap_or(0)),
        }
    } else {
        SyscallMsrs::capture(msr)
    };

    // Restore host MSRs even if VMCLEAR failed.
    ctx.state().host_state.syscall_msrs.load(msr);
    let _ = msr.write_msr(msr::IA32_KERNEL_GS_BASE, host_kernel_gs_base);

    // VMCS is in an undefined state; re-entry could crash the host.
    if let Err(e) = clear_result {
        log_err!("VMCLEAR failed - VMCS in undefined state, cannot continue\n");
        return Err(VmRunError::VmcsClear(e));
    }

    result
}

/// Inner loop, split out so `run` clears the VMCS on every exit path.
///
/// `host_kernel_gs_base` is restored after each exit so host interrupt handlers
/// in the IRQ windows see the host value.
unsafe fn run_loop<Ctx, R, M, A>(
    ctx: &mut Ctx,
    runner: &mut R,
    machine: &M,
    allocator: &mut A,
    host_kernel_gs_base: u64,
) -> Result<ExitReason, VmRunError>
where
    Ctx: VmContext,
    R: VmRunner<Vmcs = Ctx::Vmcs>,
    M: Machine,
    A: CowAllocator<Ctx::CowPage>,
{
    // vmx_run_guest loads guest XCR0 before VMRESUME; a host interrupt in that
    // window could #UD on AVX-512. Host XCR0 is restored on exit, so the
    // ReverseIrqGuard windows between exits are safe.
    let _irq_guard = IrqGuard::new(machine.kernel());

    // Host state is constant while preemption is disabled; write it once
    // (each VMWRITE traps to L0 under nested virt).
    let cr3 = machine
        .cr_access()
        .read_cr3()
        .map_err(|_| VmRunError::ReadHostCr3)?
        .bits();
    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::HostCr3, cr3)
        .map_err(VmRunError::WriteHostCr3)?;

    {
        let msr = machine.msr_access();
        let fs_base = msr.read_msr(msr::IA32_FS_BASE).unwrap_or(0);
        ctx.state()
            .vmcs
            .write_natural(VmcsFieldNatural::HostFsBase, fs_base)
            .map_err(VmRunError::WriteHostFsBase)?;
        let gs_base = msr.read_msr(msr::IA32_GS_BASE).unwrap_or(0);
        ctx.state()
            .vmcs
            .write_natural(VmcsFieldNatural::HostGsBase, gs_base)
            .map_err(VmRunError::WriteHostGsBase)?;
    }

    let dta = machine.descriptor_table_access();
    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::HostTrBase, dta.read_tr_base())
        .map_err(VmRunError::WriteHostTrBase)?;
    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::HostGdtrBase, dta.read_gdtr().base)
        .map_err(VmRunError::WriteHostGdtrBase)?;

    // HOST_RSP points at VmxContext.
    let host_rsp = core::ptr::from_mut(&mut ctx.state_mut().vmx_ctx) as u64;
    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::HostRsp, host_rsp)
        .map_err(VmRunError::WriteHostRsp)?;

    // Must run on the loop's CPU with preemption disabled.
    ctx.state_mut()
        .instruction_counter
        .prepare()
        .map_err(VmRunError::InstructionCounter)?;

    // Auto-save/load the instruction counter across exits so host-side ticks
    // (e.g. perf's NMI handler rewriting GLOBAL_CTRL via
    // `__intel_pmu_enable_all`) are wiped on the next entry.
    if let Some(entry_phys) = ctx.state().instruction_counter.msr_save_load_entry_phys() {
        let _ = ctx
            .state()
            .vmcs
            .write64(VmcsField64::VmExitMsrStoreAddr, entry_phys);
        let _ = ctx
            .state()
            .vmcs
            .write32(VmcsField32::VmExitMsrStoreCount, 1);
        let _ = ctx
            .state()
            .vmcs
            .write64(VmcsField64::VmEntryMsrLoadAddr, entry_phys);
        let _ = ctx
            .state()
            .vmcs
            .write32(VmcsField32::VmEntryMsrLoadCount, 1);
    }

    // Hardware PERF_GLOBAL_CTRL switching, constant for the loop.
    //
    // Re-add the PEBS FIXED_CTR0 bit: the IC's snapshot lacks it, so this
    // write would otherwise clobber `register_pebs_page`'s OR and PEBS would
    // never fire.
    let pebs_registered = ctx.state().pebs_state.is_some();
    if let Some((mut guest_val, host_val)) =
        ctx.state().instruction_counter.perf_global_ctrl_values()
    {
        if pebs_registered {
            guest_val |= PERF_GLOBAL_CTRL_FIXED_CTR0;
        }
        let _ = ctx
            .state()
            .vmcs
            .write64(VmcsField64::GuestIa32PerfGlobalCtrl, guest_val);
        let _ = ctx
            .state()
            .vmcs
            .write64(VmcsField64::HostIa32PerfGlobalCtrl, host_val);

        if let Ok(entry_ctrl) = ctx.state().vmcs.read32(VmcsField32::VmEntryControls) {
            let _ = ctx.state().vmcs.write32(
                VmcsField32::VmEntryControls,
                entry_ctrl | vm_entry::LOAD_IA32_PERF_GLOBAL_CTRL,
            );
        }
        if let Ok(exit_ctrl) = ctx.state().vmcs.read32(VmcsField32::PrimaryVmExitControls) {
            let _ = ctx.state().vmcs.write32(
                VmcsField32::PrimaryVmExitControls,
                exit_ctrl | vm_exit::LOAD_IA32_PERF_GLOBAL_CTRL,
            );
        }
    }

    let mut force_single_step = false;
    // A masked #DB may stop at a proven ordinary instruction. RF skips that
    // one instruction breakpoint on the next guest entry. Keep this state
    // local so synthetic RF can never survive a return to userspace.
    let mut masked_retry_rip = None;
    let mut masked_retry_plan: Option<(super::InstructionBatch, u64, u64)> = None;
    // A clock read does not change guest memory or translations. Reuse its
    // global proof on the next entry. The code write guard is rearmed for
    // every entry; validated execute grants may stay live between clock exits.
    let mut clock_retry_plan: Option<(super::InstructionBatch, u64, u64)> = None;
    let mut held_clock_execute: Option<NptExecuteGuard> = None;
    let mut held_clock_paired: [Option<NptExecuteGuard>; 3] = [None, None, None];
    let mut held_clock_pages = [0u64; 4];
    let mut held_clock_page_count = 0;
    if Ctx::V::uses_nested_paging() && !ctx.state().svm_host_memory_stable {
        // Userspace may have changed RAM between RUN calls.
        ctx.state_mut().svm_guard.valid = false;
        let mut guarded_code = false;
        for memo in &mut ctx.state_mut().svm_guard.hazard_memos {
            if memo.guarded {
                memo.valid = false;
                guarded_code = true;
            }
        }
        if guarded_code {
            let guard = &mut ctx.state_mut().svm_guard;
            guard.code_epoch = guard.code_epoch.wrapping_add(1);
            guard.code_count = 0;
        }
        ctx.state_mut().ept.invalidate_all_npt_code(allocator);
        ctx.state_mut().svm_gate_scalar_page = None;
        ctx.state_mut().svm_gate_pending_write = false;
        ctx.state_mut().svm_gate_replay_start = None;
        ctx.state_mut().svm_gate_replay_rip = None;
        ctx.state_mut().svm_guard.gate_dirty = true;
        ctx.state_mut().svm_guard.gate_links_untrusted = true;
        ctx.state_mut().svm_guard.gate_host_write_count = 0;
        ctx.state_mut().svm_guard.gate_host_write_all = false;
    }
    let loop_result = (|| loop {
        let loop_start_tsc = rdtsc();

        // Host RAM writes can revoke a trusted page without changing the
        // guarded hazard memo's code epoch. Never reuse an old page plan after
        // consuming those writes, including plans with no retained grant.
        if ctx.state().svm_guard.gate_host_write_all
            || ctx.state().svm_guard.gate_host_write_count != 0
        {
            clock_retry_plan = None;
        }

        // An execute grant can span only consecutive clock exits on the same
        // guarded page. Revoke it before any EPT/table refresh or host write
        // can replace the leaf referenced by NptExecuteGuard.
        if held_clock_execute.is_some() {
            let still_guarded = clock_retry_plan.as_ref().is_some_and(|(_, epoch, generation)| {
                ctx.state().svm_guard.gate_ready && !ctx.state().svm_guard.gate_dirty
                    && !ctx.state().svm_guard.gate_host_write_all
                    && ctx.state().svm_guard.gate_host_write_count == 0
                    && ctx.state().svm_guard.code_epoch == *epoch
                    && ctx.state().ept.mapping_generation() == *generation
                    && ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr3).ok()
                        .is_some_and(|root| root & 0x000f_ffff_ffff_f000
                            == ctx.state().svm_guard.gate_root)
            });
            if !still_guarded {
                restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);
                held_clock_page_count = 0;
                clock_retry_plan = None;
            }
        }

        if Ctx::V::uses_nested_paging() && held_clock_execute.is_none() {
            if ctx.state().svm_guard.gate_host_write_all {
                ctx.state_mut().ept.invalidate_all_npt_code(allocator);
                ctx.state_mut().svm_guard.gate_dirty = true;
                ctx.state_mut().svm_guard.gate_links_untrusted = true;
            } else {
                let count = ctx.state().svm_guard.gate_host_write_count;
                for index in 0..count {
                    let page = ctx.state().svm_guard.gate_host_writes[index];
                    if ctx.state().svm_guard.gate_tables[..ctx.state().svm_guard.gate_count]
                        .contains(&page)
                        || ctx.state().svm_guard.retained_guards
                            [..ctx.state().svm_guard.retained_guard_count]
                            .iter().any(|saved| saved.valid && saved.guest == page)
                    {
                        ctx.state_mut().svm_guard.gate_dirty = true;
                        ctx.state_mut().svm_guard.gate_links_untrusted = true;
                    }
                    let _ = ctx
                        .state_mut()
                        .ept
                        .invalidate_npt_code_4k(allocator, GuestPhysAddr::new(page));
                }
            }
            ctx.state_mut().svm_guard.gate_host_write_count = 0;
            ctx.state_mut().svm_guard.gate_host_write_all = false;
            super::super::exits::refresh_global_tree(
                ctx,
                allocator,
                runner.can_guard_page_tables(),
            );
        }

        ctx.sync_gprs_to_vmx_ctx();

        inject_pending_interrupt(ctx).map_err(VmRunError::ExitHandler)?;

        // SVM software event delivery may write a COW stack frame. Release
        // the old grant before prepare_instruction_exit can perform it.
        if held_clock_execute.is_some()
            && ctx.state().vmcs.read32(VmcsField32::VmEntryInterruptionInfo)
                .ok().is_none_or(|info| info & (1 << 31) != 0)
        {
            restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);
            held_clock_page_count = 0;
            clock_retry_plan = None;
        }

        // Interrupt preparation may stage an event in the full trace buffer.
        // Drain before entering the guest, or its next exit record is dropped.
        if ctx.state().event_buffer_full() {
            ctx.state_mut().exit_stats.total_run_cycles += rdtsc().saturating_sub(loop_start_tsc);
            break Ok(ExitReason::EventBufferFull);
        }

        let pre_entry_tsc = rdtsc();
        ctx.state_mut().exit_stats.vmentry_overhead_cycles +=
            pre_entry_tsc.saturating_sub(loop_start_tsc);

        // IA32_KERNEL_GS_BASE has no VMCS field.
        let msr = machine.msr_access();
        if !Ctx::V::uses_nested_paging() {
            let _ = msr.write_msr(msr::IA32_KERNEL_GS_BASE, ctx.state().kernel_gs_base);
        }

        // Captured before entry: the exit handler may clear `armed_action`.
        let pebs_armed_this_iter = ctx
            .state()
            .pebs_state
            .as_deref()
            .is_some_and(|p| p.armed_action.is_some());
        if pebs_armed_this_iter {
            pebs_pre_vm_entry(ctx, msr);
        }

        // Split borrow: vmx_ctx mutably, vmcs immutably.
        let mut instruction_window = None;
        let software_exit = Ctx::V::uses_nested_paging()
            && super::super::exits::prepare_instruction_exit(
                ctx,
                runner,
                allocator,
                &mut instruction_window,
            )
            .map_err(VmRunError::ExitHandler)?;
        if software_exit {
            ctx.sync_gprs_to_vmx_ctx();
        }
        let mut scalar_page = ctx.state().svm_gate_scalar_page;
        if let (Some(page), Some(window)) = (scalar_page, instruction_window.as_ref()) {
            let current = window.physical.as_u64() & !4095;
            if current != page
                && ctx.state().ept.npt_trusted_code_4k(allocator, GuestPhysAddr::new(current))
            {
                let split_fetch = window.linear & 4095 >= 4096 - 15
                    && window.following_page(ctx).ok()
                        .is_some_and(|next| next.as_u64() & !4095 == page);
                if !split_fetch {
                    ctx.state_mut().svm_gate_scalar_page = None;
                    scalar_page = None;
                }
            }
        }
        let replay_rip_matches = ctx.state().svm_gate_replay_start.is_some_and(|start| {
                ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip).ok() == Some(start)
            });
        let replay_pending = ctx.state().svm_gate_pending_write
            && replay_rip_matches
            && ctx.state().vmcs.read32(VmcsField32::VmEntryInterruptionInfo)
                .ok().is_some_and(|info| info & (1 << 31) == 0);
        let replay_expected = ctx.state().svm_gate_replay_rip;
        let replay_single_step = replay_pending && !software_exit;
        if replay_single_step {
            force_single_step = true;
        }
        let mut batch = None;
        let mut reused_clock_plan = false;
        // A false address-mask hit retires no instruction at its RIP. The
        // guarded code and translation proofs remain valid, so reusing their
        // four-breakpoint plan avoids another alias walk before RF retry.
        if let (Some(rip), Some((mut previous, code_epoch, mapping_generation)), Some(window)) =
            (masked_retry_rip, masked_retry_plan, instruction_window.as_ref())
        {
            let budget = super::super::exits::instruction_budget(ctx);
            if !software_exit && !force_single_step
                && budget > super::super::traits::InstructionBatch::COUNTER_DEADLINE_MARGIN
                && ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip).ok() == Some(rip)
                && ctx.state().svm_guard.valid
                && ctx.state().svm_guard.gate_ready && !ctx.state().svm_guard.gate_dirty
                && ctx.state().svm_guard.code_epoch == code_epoch
                && ctx.state().ept.mapping_generation() == mapping_generation
                && ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr3).ok()
                    .is_some_and(|root| root & 0x000f_ffff_ffff_f000
                        == ctx.state().svm_guard.gate_root)
                && ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRflags).ok()
                    .is_some_and(|flags| flags & ((1 << 8) | (1 << 16)) == 0)
                && !ctx.state().mtf_enabled && !ctx.state().svm_gate_pending_write
                && previous.pages[..previous.code_page_count]
                    .contains(&(window.physical.as_u64() & !4095))
            {
                previous.instruction_budget = budget;
                batch = Some(previous);
            }
        }
        if batch.is_none() {
            if let (Some((mut previous, code_epoch, mapping_generation)), Some(window)) =
                (clock_retry_plan.take(), instruction_window.as_ref())
            {
                let budget = super::super::exits::instruction_budget(ctx);
                if !software_exit && !force_single_step
                    && scalar_page.is_none_or(|page| {
                        previous.pages[..previous.code_page_count].contains(&page)
                    })
                    && previous.global_execution
                    && budget > super::super::traits::InstructionBatch::COUNTER_DEADLINE_MARGIN
                    && ctx.state().svm_guard.gate_ready && !ctx.state().svm_guard.gate_dirty
                    && !ctx.state().svm_guard.gate_host_write_all
                    && ctx.state().svm_guard.gate_host_write_count == 0
                    && ctx.state().svm_guard.code_epoch == code_epoch
                    && ctx.state().ept.mapping_generation() == mapping_generation
                    && ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr3).ok()
                        .is_some_and(|root| root & 0x000f_ffff_ffff_f000
                            == ctx.state().svm_guard.gate_root)
                    && ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRflags).ok()
                        .is_some_and(|flags| flags & ((1 << 8) | (1 << 16)) == 0)
                    && !ctx.state().mtf_enabled && !ctx.state().svm_gate_pending_write
                    && !super::super::exits::bounded_repeat_entry(&window.bytes)
                    && previous.pages[..previous.code_page_count]
                        .contains(&(window.physical.as_u64() & !4095))
                {
                    previous.instruction_budget = budget;
                    if let Ok(start) = ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip) {
                        previous.start = start;
                        batch = Some(previous);
                        reused_clock_plan = true;
                        // The retained batch already grants this selected
                        // page. A fetch-fault scalar hint cannot require a
                        // second, overlapping execute-grant token.
                        scalar_page = None;
                    }
                }
            }
        }
        if held_clock_execute.is_some() && !reused_clock_plan {
            restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);
            held_clock_page_count = 0;
        }
        if Ctx::V::uses_nested_paging() && !software_exit && !force_single_step {
            let trusted_scalar = scalar_page.is_some_and(|page| {
                ctx.state().ept.npt_trusted_code_4k(allocator, GuestPhysAddr::new(page))
            });
            if !trusted_scalar && batch.is_none() {
                if let Some(window) = instruction_window.as_mut() {
                    if super::super::exits::prepare_global_into(
                        ctx,
                        allocator,
                        runner.can_count_instructions(),
                        scalar_page.is_some(),
                        window,
                        &mut batch,
                    ).is_none() {
                        batch = window.fill_full(ctx).ok().and_then(|_| {
                            super::super::exits::prepare_instruction_batch(
                                ctx,
                                runner.can_count_instructions(),
                                runner.can_guard_page_tables(),
                                window,
                            )
                        });
                    }
                }
            }
        }
        force_single_step = false;
        let mut arm_masked_retry = masked_retry_rip.is_some_and(|rip| {
            batch.as_ref().is_some_and(|batch| batch.global_execution)
                && ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip).ok() == Some(rip)
                && ctx.state().vmcs.read16(VmcsField16::GuestCsSelector)
                    .ok().is_some_and(|cs| cs & 3 == 3)
                && ctx.state().vmcs.read32(VmcsField32::VmEntryInterruptionInfo)
                    .ok().is_some_and(|info| info & (1 << 31) == 0)
                && instruction_window.as_ref().is_some_and(|window| {
                    batch.as_ref().is_some_and(|batch| batch.pages[..batch.code_page_count]
                        .contains(&(window.physical.as_u64() & !4095)))
                        && super::super::exits::proven_nonhazard(ctx, window.physical.as_u64())
                })
        });
        if !arm_masked_retry {
            masked_retry_rip = None;
            masked_retry_plan = None;
        }
        if held_clock_execute.is_some()
            && !(reused_clock_plan
                && batch.as_ref().is_some_and(|batch| {
                    batch.global_execution
                        && batch.code_page_count == held_clock_page_count
                        && batch.pages[..batch.code_page_count]
                            == held_clock_pages[..held_clock_page_count]
                }))
        {
            restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);
            held_clock_page_count = 0;
        }
        let mut scalar_execute = if !software_exit {
            scalar_page
                .map(|page| {
                    let page = instruction_window.as_ref()
                        .map_or(page, |window| window.physical.as_u64() & !4095);
                    ctx.state_mut()
                        .ept
                        .allow_npt_execute_4k(allocator, GuestPhysAddr::new(page))
                        .ok_or(VmRunError::ExitHandler(
                            super::super::exits::ExitError::Fatal("SVM scalar code page is unmapped"),
                        ))
                })
                .transpose()?
        } else {
            None
        };
        let scalar_next_execute = if !software_exit && scalar_page.is_some() {
            instruction_window.as_ref().and_then(|window| {
                (window.linear & 4095 >= 4096 - 15)
                    .then(|| window.following_page(ctx).ok())
                    .flatten()
                    .and_then(|page| ctx.state_mut().ept.allow_npt_execute_4k(
                        allocator, GuestPhysAddr::new(page.as_u64() & !4095)))
            })
        } else {
            None
        };
        let guard = batch.as_ref().and_then(|batch| {
            super::super::exits::protect_instruction_batch(ctx, allocator, batch)
        });
        if batch.is_some() && guard.is_none() {
            restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);
            held_clock_page_count = 0;
            batch = None;
        }
        // A global batch has validated the current hazardous page's exact
        // bytes and executable aliases. It can grant NPT execute permission
        // for this interval even without an earlier execute-fault marker.
        // Wait until protection succeeds so a rejected batch cannot grant
        // permission to an unvalidated scalar entry.
        if scalar_execute.is_none() {
            if let Some(page) = batch.as_ref()
                .filter(|batch| batch.global_execution)
                .map(|batch| batch.pages[0])
            {
                let execute = held_clock_execute.take().or_else(||
                    ctx.state_mut().ept.allow_npt_execute_4k(
                        allocator, GuestPhysAddr::new(page),
                    ));
                let Some(execute) = execute else {
                    if let Some(guard) = guard {
                        guard.restore(ctx, allocator);
                    }
                    return Err(VmRunError::ExitHandler(
                        super::super::exits::ExitError::Fatal("SVM global code page is unmapped"),
                    ));
                };
                scalar_execute = Some(execute);
            }
        }
        // Protection can reject the batch after the retry decision was made.
        // In that case the synthetic flag must not reach a scalar entry.
        if batch.as_ref().is_none_or(|batch| !batch.global_execution) {
            arm_masked_retry = false;
            masked_retry_rip = None;
            masked_retry_plan = None;
        }
        let mut paired_execute: [Option<NptExecuteGuard>; 3] = [None, None, None];
        if let Some(batch) = batch.as_ref().filter(|batch| batch.global_execution) {
            for index in 1..batch.code_page_count {
                let Some(execute) = held_clock_paired[index - 1].take().or_else(|| {
                    ctx.state_mut().ept.allow_npt_execute_4k(
                        allocator,
                        GuestPhysAddr::new(batch.pages[index]),
                    )
                }) else {
                    for execute in paired_execute.into_iter().flatten() {
                        execute.restore(&mut ctx.state_mut().ept, allocator);
                    }
                    restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);
                    held_clock_page_count = 0;
                    if let Some(execute) = scalar_next_execute {
                        execute.restore(&mut ctx.state_mut().ept, allocator);
                    }
                    if let Some(execute) = scalar_execute {
                        execute.restore(&mut ctx.state_mut().ept, allocator);
                    }
                    if let Some(guard) = guard {
                        guard.restore(ctx, allocator);
                    }
                    return Err(VmRunError::ExitHandler(
                        super::super::exits::ExitError::Fatal("SVM paired code page is unmapped"),
                    ));
                };
                paired_execute[index - 1] = Some(execute);
            }
        }
        held_clock_page_count = 0;
        if Ctx::V::uses_nested_paging() {
            super::super::exits::retain_translation_cache(
                ctx,
                batch.as_ref(),
                instruction_window.as_ref(),
                software_exit,
            );
        }
        let pre_guest_tsc = rdtsc();
        ctx.state_mut().exit_stats.vmentry_overhead_cycles +=
            pre_guest_tsc.saturating_sub(pre_entry_tsc);
        let state = ctx.state_mut();
        state.vmx_ctx.svm_clock_base = state.last_instruction_count.wrapping_add(state.tsc_offset);
        state.vmx_ctx.svm_pmc_clock_enabled = u64::from(state.svm_pmc_clock_enabled);
        state.vmx_ctx.svm_entry_repeat = u64::from(
            batch.is_none()
                && instruction_window.as_ref().is_some_and(|window| {
                    super::super::exits::bounded_repeat_entry(&window.bytes)
                }),
        );
        if arm_masked_retry {
            state.vmcs.write_natural(
                VmcsFieldNatural::GuestRflags,
                state.vmcs.read_natural(VmcsFieldNatural::GuestRflags)
                    .map_err(|_| VmRunError::ExitHandler(
                        super::super::exits::ExitError::Fatal("SVM RF entry flags unavailable")))? | (1 << 16),
            ).map_err(VmRunError::WriteHostRsp)?;
        }
        // SAFETY: Caller guarantees VMCS is properly configured and loaded,
        // interrupts are disabled, and preemption cannot migrate us.
        let run_result = if software_exit {
            Ok(())
        } else {
            unsafe { runner.run(&mut state.vmx_ctx, &state.vmcs, batch.as_ref()) }
        };
        let post_guest_tsc = rdtsc();
        ctx.state_mut().exit_stats.guest_cycles += post_guest_tsc.saturating_sub(pre_guest_tsc);
        #[cfg(not(feature = "cargo"))]
        {
            use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
            static FETCHES: AtomicU64 = AtomicU64::new(0);
            static BINS: [AtomicU64; 50] = [const { AtomicU64::new(0) }; 50];
            static DUMPED: AtomicBool = AtomicBool::new(false);
            static PAGE_DUMPED: [AtomicBool; 3] = [const { AtomicBool::new(false) }; 3];
            let tsc = ctx.state().vmx_ctx.svm_clock_base;
            if tsc >= 300_000_000_000 && !DUMPED.load(Relaxed) {
                if run_result.is_ok()
                    && ctx.state().vmcs.read32(VmcsField32::VmExitReason).ok() == Some(48)
                    && ctx.state().vmcs.read_natural(VmcsFieldNatural::ExitQualification)
                        .ok().is_some_and(|qualification| qualification & 4 != 0)
                {
                    let event = FETCHES.fetch_add(1, Relaxed);
                    if event & 255 == 0 {
                        if let Some(target) = ctx.state().vmcs.read64(VmcsField64::GuestPhysicalAddr)
                            .ok().map(|gpa| gpa & !4095)
                        {
                            let kind = batch.as_ref().map_or(0, |plan|
                                super::super::exits::classify_ci_fetch_target(
                                    ctx, allocator, plan, target, reused_clock_plan,
                                    arm_masked_retry));
                            BINS[usize::from(kind)].fetch_add(1, Relaxed);
                            if kind == 6 && event & 8191 == 0 {
                                if let Some(plan) = batch.as_ref() {
                                    let memos = &ctx.state().svm_guard.hazard_memos;
                                    let count = |page: u64| memos.iter()
                                        .find(|memo| memo.valid && memo.proof.page == page)
                                        .map_or(0, |memo| memo.proof.count);
                                    let rip = ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip)
                                        .unwrap_or(0);
                                    kernel::pr_info!(
                                        "SVM CI CAP src={:#x} dst={:#x} off={:#x} src_h={} dst_h={} slots={} pages={} p1={:#x} p2={:#x} p3={:#x}\n",
                                        plan.pages[0], target, rip & 4095,
                                        count(plan.pages[0]), count(target),
                                        plan.page_breakpoint_count, plan.code_page_count,
                                        plan.pages[1], plan.pages[2], plan.pages[3],
                                    );
                                    for (index, page) in [0x1398a2000, 0xfd49000, 0xfd37000]
                                        .into_iter().enumerate() {
                                        if page != plan.pages[0] && page != target {
                                            continue;
                                        }
                                        if PAGE_DUMPED[index].swap(true, Relaxed) {
                                            continue;
                                        }
                                        if let Some(memo) = memos.iter()
                                            .find(|memo| memo.valid && memo.proof.page == page) {
                                            let hash = memo.bytes.iter().fold(
                                                0xcbf2_9ce4_8422_2325u64,
                                                |hash, &byte| (hash ^ u64::from(byte))
                                                    .wrapping_mul(0x100_0000_01b3),
                                            );
                                            let offsets = memo.proof.offsets;
                                            kernel::pr_info!(
                                                "SVM CI PAGE page={:#x} count={} hash={:#x} offsets={:#x},{:#x},{:#x},{:#x},{:#x},{:#x}\n",
                                                page, memo.proof.count, hash,
                                                offsets[0], offsets[1], offsets[2],
                                                offsets[3], offsets[4], offsets[5],
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if tsc >= 600_000_000_000 && !DUMPED.swap(true, Relaxed) {
                    kernel::pr_info!("SVM CI FIT fetches={}\n", FETCHES.load(Relaxed));
                    for (index, bin) in BINS.iter().enumerate() {
                        kernel::pr_info!("SVM CI FIT category={} sampled={}\n",
                            index, bin.load(Relaxed));
                    }
                }
            }
        }
        if arm_masked_retry {
            let flags = ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRflags)
                .map_err(|_| VmRunError::ExitHandler(
                    super::super::exits::ExitError::Fatal("SVM RF exit flags unavailable")))?;
            ctx.state().vmcs.write_natural(VmcsFieldNatural::GuestRflags, flags & !(1 << 16))
                .map_err(VmRunError::WriteHostRsp)?;
            if runner.completed_instructions().is_some_and(|count| count != 0) {
                masked_retry_rip = None;
                masked_retry_plan = None;
            }
        }
        if replay_single_step
            && run_result.is_ok()
            && runner.completed_instructions() == Some(1)
            && ctx.state().vmcs.read32(VmcsField32::VmExitReason)
                .ok().is_some_and(|reason| reason & 0xffff == 37)
            && replay_expected.is_none_or(|rip| {
                ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip).ok() == Some(rip)
            })
        {
            ctx.state_mut().svm_gate_pending_write = false;
            ctx.state_mut().svm_gate_replay_start = None;
            ctx.state_mut().svm_gate_replay_rip = None;
        }
        // The write guard was released for exactly one VM entry. Even if an
        // interrupt or a fault prevented the store from retiring, rearming it
        // is safe: a later retry will fault on the guard again. REP stores may
        // fault on subsequent iterations, which remains conservative.
        if !software_exit && run_result.is_ok() && ctx.state().svm_gate_pending_write {
            ctx.state_mut().svm_gate_pending_write = false;
            ctx.state_mut().svm_gate_replay_start = None;
            ctx.state_mut().svm_gate_replay_rip = None;
        }
        if run_result.is_ok()
            && batch.as_ref().is_some_and(|batch| batch.global_execution)
            && ctx.state().vmcs.read_natural(VmcsFieldNatural::ExitQualification)
                .ok().is_some_and(|qual| qual & super::super::traits::InstructionBatch::PAGE_SCALAR_REPLAY != 0)
        {
            let window = super::super::exits::InstructionWindow::read_cached(ctx).ok();
            let breakpoint_replay = ctx.state().vmcs.read_natural(VmcsFieldNatural::ExitQualification)
                .ok().is_some_and(|qual| qual
                    & super::super::traits::InstructionBatch::PAGE_BREAKPOINT_REPLAY != 0);
            let harmless = breakpoint_replay
                && ctx.state().vmcs.read16(VmcsField16::GuestCsSelector)
                    .ok().is_some_and(|cs| cs & 3 == 3)
                && window.as_ref().is_some_and(|window| {
                batch.as_ref().is_some_and(|batch| batch.pages[..batch.code_page_count]
                    .contains(&(window.physical.as_u64() & !4095)))
                    && super::super::exits::proven_nonhazard(ctx, window.physical.as_u64())
            });
            if harmless {
                let rip = ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip)
                    .map_err(|_| VmRunError::ExitHandler(
                        super::super::exits::ExitError::Fatal("SVM masked breakpoint RIP unavailable")))?;
                let qual = ctx.state().vmcs.read_natural(VmcsFieldNatural::ExitQualification)
                    .map_err(|_| VmRunError::ExitHandler(
                        super::super::exits::ExitError::Fatal("SVM masked breakpoint qualification unavailable")))?;
                ctx.state().vmcs.write_natural(VmcsFieldNatural::ExitQualification,
                    qual & !super::super::traits::InstructionBatch::PAGE_SCALAR_REPLAY)
                    .map_err(VmRunError::WriteHostRsp)?;
                masked_retry_rip = Some(rip);
                masked_retry_plan = batch.map(|batch| (
                    batch,
                    ctx.state().svm_guard.code_epoch,
                    ctx.state().ept.mapping_generation(),
                ));
                ctx.state_mut().svm_gate_scalar_page = window.as_ref()
                    .map(|window| window.physical.as_u64() & !4095);
            } else {
                ctx.state_mut().svm_gate_scalar_page = window.as_ref()
                    .map(|window| window.physical.as_u64() & !4095);
                // At a REP breakpoint, the global gate yields to the bounded
                // REP executor. Other hazards replay one scalar instruction.
                force_single_step = !window.as_ref().is_some_and(|window| {
                    super::super::exits::bounded_repeat_entry(&window.bytes)
                });
            }
        }
        if let Some(guard) = guard {
            if guard.restore(ctx, allocator) && run_result.is_ok() {
                // A store attempted to change this batch's code or address
                // translation. Restore permissions, step it once, and replan.
                ctx.state()
                    .vmcs
                    .write32(VmcsField32::VmExitReason, 37)
                    .map_err(VmRunError::WriteHostRsp)?;
                force_single_step = true;
                masked_retry_rip = None;
                masked_retry_plan = None;
            }
        }
        let has_scalar_next_execute = scalar_next_execute.is_some();
        if let Some(execute) = scalar_next_execute {
            execute.restore(&mut ctx.state_mut().ept, allocator);
        }
        let hold_clock_execute = run_result.is_ok()
            && batch.as_ref().is_some_and(|batch| {
                batch.global_execution && batch.code_page_count <= 4
            })
            && matches!(ctx.state().vmcs.read32(VmcsField32::VmExitReason).ok(), Some(16 | 51))
            && !force_single_step && masked_retry_rip.is_none()
            && !has_scalar_next_execute;
        if hold_clock_execute {
            let selected = batch.as_ref().unwrap();
            held_clock_page_count = selected.code_page_count;
            held_clock_pages[..selected.code_page_count]
                .copy_from_slice(&selected.pages[..selected.code_page_count]);
            for (held, current) in held_clock_paired.iter_mut().zip(paired_execute.iter_mut()) {
                *held = current.take();
            }
        } else {
            for execute in paired_execute.into_iter().flatten() {
                execute.restore(&mut ctx.state_mut().ept, allocator);
            }
        }
        if let Some(execute) = scalar_execute.take_if(|_| !hold_clock_execute) {
            execute.restore(&mut ctx.state_mut().ept, allocator);
            let replay = ctx.state().vmcs.read_natural(VmcsFieldNatural::ExitQualification)
                .ok().is_some_and(|qual| qual
                    & super::super::traits::InstructionBatch::PAGE_SCALAR_REPLAY != 0);
            if (batch.as_ref().is_some_and(|batch| batch.global_execution)
                && !replay && !force_single_step && masked_retry_rip.is_none())
                || (batch.as_ref().is_none_or(|batch| !batch.page_execution)
                    && (runner.completed_instructions().is_some_and(|count| count != 0)
                        || ctx.state().vmx_ctx.svm_entry_repeat != 0)
                    && ctx.state().svm_gate_scalar_page.is_some_and(|page| {
                        ctx.state()
                            .ept
                            .npt_trusted_code_4k(allocator, GuestPhysAddr::new(page))
                    }))
            {
                ctx.state_mut().svm_gate_scalar_page = None;
            }
            // Keep an untrusted page selected across scalar steps. Its NPT
            // execute permission is restored above on every entry, so this
            // skips the redundant fetch fault and full-page hazard scan
            // before the next instruction on that page. A trusted page
            // returns to the global gate after its one scalar replay.
        }
        if run_result.is_ok()
            && batch
                .as_ref()
                .is_some_and(|batch| batch.global_execution && batch.code_page_count == 2)
            && !force_single_step
            && !matches!(ctx.state().vmcs.read32(VmcsField32::VmExitReason).ok(), Some(16 | 51))
        {
            if let Some(page) = super::super::exits::InstructionWindow::read_cached(ctx)
                .ok()
                .map(|window| window.physical.as_u64() & !4095)
            {
                if batch
                    .as_ref()
                    .is_some_and(|batch| batch.pages[..2].contains(&page))
                    && !ctx
                        .state()
                        .ept
                        .npt_trusted_code_4k(allocator, GuestPhysAddr::new(page))
                {
                    ctx.state_mut().svm_gate_scalar_page = Some(page);
                }
            }
        }

        if pebs_armed_this_iter {
            pebs_post_vm_exit(ctx, msr);
        }

        // Before any IRQ window, so host handlers neither clobber the guest
        // value nor see it.
        ctx.state_mut().kernel_gs_base = runner
            .saved_guest_msr(&ctx.state().vmcs, msr::IA32_KERNEL_GS_BASE)
            .unwrap_or_else(|| msr.read_msr(msr::IA32_KERNEL_GS_BASE).unwrap_or(0));
        let _ = msr.write_msr(msr::IA32_KERNEL_GS_BASE, host_kernel_gs_base);

        let post_exit_tsc = rdtsc();
        ctx.state_mut().exit_stats.vmexit_overhead_cycles +=
            post_exit_tsc.saturating_sub(post_guest_tsc);

        // Prior instructions must complete before RDPMC (SDM Vol 3A §10.3 fn 3).
        // LFENCE rather than CPUID, which exits to L0 under nested virt.
        // SAFETY: LFENCE is a safe ordering instruction that ensures prior
        // instructions complete locally before RDPMC reads the performance counter.
        #[cfg(not(feature = "cargo"))]
        unsafe {
            core::arch::asm!("lfence", options(preserves_flags, nostack));
        }

        // Service pending host interrupts; host XCR0 is already restored.
        if let Ok(reason) = ctx.state().vmcs.read32(VmcsField32::VmExitReason) {
            if let Some(count) = runner.completed_instructions().filter(|_| !software_exit) {
                ctx.state_mut()
                    .instruction_counter
                    .record_instructions(count);
            } else {
                // A software-emulated SYSRET with R11.TF retires one
                // instruction before reporting its architectural #DB.
                ctx.state_mut().instruction_counter.record_exit(
                    if software_exit && reason == 0 { 37 } else { reason },
                );
            }
        }
        let pre_irq_tsc = rdtsc();
        {
            let _irq_window = ReverseIrqGuard::new(machine.kernel());
            let count = ctx.state().instruction_counter.read();
            ctx.state_mut().last_instruction_count = count;
            runner.host_irq_window(&mut ctx.state_mut().vmx_ctx, pre_irq_tsc);
        }
        let post_irq_tsc = rdtsc();
        ctx.state_mut().exit_stats.irq_window_cycles += post_irq_tsc.saturating_sub(pre_irq_tsc);

        if let Err(ref e) = run_result {
            if let Ok(error) = ctx.state().vmcs.read32(VmcsField32::VmInstructionError) {
                log_err!("VM entry failed: {:?}, VM_INSTRUCTION_ERROR={}", e, error);
            } else {
                log_err!("VM entry failed: {:?}, couldn't read error field", e);
            }
            return Err(VmRunError::VmEntry(run_result.unwrap_err()));
        }

        ctx.sync_gprs_from_vmx_ctx();

        let pre_handler_tsc = rdtsc();
        let total_exit_overhead = pre_handler_tsc.saturating_sub(post_exit_tsc);
        let irq_window = post_irq_tsc.saturating_sub(pre_irq_tsc);
        ctx.state_mut().exit_stats.vmexit_overhead_cycles +=
            total_exit_overhead.saturating_sub(irq_window);

        let kernel = machine.kernel();
        if Ctx::V::uses_nested_paging()
            && !matches!(
                ctx.state().vmcs.read32(VmcsField32::VmExitReason).ok(),
                Some(10 | 16 | 30 | 37 | 51 | 512 | 513)
            )
        {
            // Emulated exits can write guest RAM, including interrupt frames
            // and device responses. Synthetic MTF handling writes no RAM;
            // CPUID, clock reads, port I/O, and POPF change no guest RAM.
            // The PUSHF handler checks
            // its actual stack destinations itself. Event delivery revokes
            // the proof separately before writing an interrupt frame.
            ctx.state_mut().svm_guard.valid = false;
        }
        match handle_exit(ctx, kernel, allocator) {
            ExitHandlerResult::Continue => {
                clock_retry_plan = if matches!(
                    ctx.state().vmcs.read32(VmcsField32::VmExitReason).ok(),
                    Some(16 | 51)
                ) && !force_single_step && masked_retry_rip.is_none()
                {
                    batch.filter(|batch| batch.global_execution).map(|batch| (
                        batch,
                        ctx.state().svm_guard.code_epoch,
                        ctx.state().ept.mapping_generation(),
                    ))
                } else {
                    None
                };
                ctx.finalize_exit_record(kernel);

                let need_resched = kernel.need_resched();
                if let Some(execute) = scalar_execute.take() {
                    if clock_retry_plan.is_some() && !need_resched {
                        held_clock_execute = Some(execute);
                    } else {
                        execute.restore(&mut ctx.state_mut().ept, allocator);
                    }
                }
                if held_clock_execute.is_none() {
                    restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);
                    held_clock_page_count = 0;
                }

                let loop_end_tsc = rdtsc();
                ctx.state_mut().exit_stats.total_run_cycles +=
                    loop_end_tsc.saturating_sub(loop_start_tsc);

                // cond_resched() would need preemption (and could migrate the
                // loaded VMCS); return to userspace to let the scheduler run.
                if need_resched {
                    break Ok(ExitReason::NeedResched);
                }
                continue;
            }
            ExitHandlerResult::ExitToUserspace(reason) => {
                if let Some(execute) = scalar_execute.take() {
                    execute.restore(&mut ctx.state_mut().ept, allocator);
                }
                ctx.finalize_exit_record(machine.kernel());

                let loop_end_tsc = rdtsc();
                ctx.state_mut().exit_stats.total_run_cycles +=
                    loop_end_tsc.saturating_sub(loop_start_tsc);

                break Ok(reason);
            }
            ExitHandlerResult::Error(e) => {
                if let Some(execute) = scalar_execute.take() {
                    execute.restore(&mut ctx.state_mut().ept, allocator);
                }
                let loop_end_tsc = rdtsc();
                ctx.state_mut().exit_stats.total_run_cycles +=
                    loop_end_tsc.saturating_sub(loop_start_tsc);

                break Err(VmRunError::ExitHandler(e));
            }
        }
    })();
    restore_held_clock_execute(ctx, allocator, &mut held_clock_execute, &mut held_clock_paired);

    // Must run on the same CPU as `prepare`.
    let finish_result = ctx
        .state_mut()
        .instruction_counter
        .finish()
        .map_err(VmRunError::InstructionCounter);

    // Read once here rather than per iteration (VMCS still loaded).
    ctx.state_mut().last_exit_qualification = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::ExitQualification)
        .unwrap_or(0);
    ctx.state_mut().last_guest_physical_addr = ctx
        .state()
        .vmcs
        .read64(VmcsField64::GuestPhysicalAddr)
        .unwrap_or(0);

    finish_result?;
    loop_result
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::events::{EventCategories, EventKind, EVENT_BUFFER_SIZE, EVENT_HEADER_SIZE};
    use crate::test_mocks::{MockFrameAllocator, MockMachine, MockVmcs};
    use crate::tests::MockVmContext;
    use crate::traits::{InstructionBatch, VmEntryError, VmxContext};

    struct MustNotEnterGuest;

    impl VmRunner for MustNotEnterGuest {
        type Vmcs = MockVmcs;

        unsafe fn run(
            &mut self,
            _ctx: &mut VmxContext,
            _vmcs: &Self::Vmcs,
            _batch: Option<&InstructionBatch>,
        ) -> Result<(), VmEntryError> {
            panic!("guest entered before draining the staged timer event");
        }
    }

    #[test]
    fn timer_event_overflow_drains_before_guest_entry() {
        let mut ctx = MockVmContext::new();
        ctx.state()
            .vmcs
            .write32(VmcsField32::IdtVectoringInfo, 0)
            .unwrap();
        ctx.state()
            .vmcs
            .write_natural(VmcsFieldNatural::GuestRflags, 0)
            .unwrap();
        ctx.state()
            .vmcs
            .write32(VmcsField32::PrimaryProcBasedVmExecControls, 0)
            .unwrap();
        let mut buffer = std::vec![0u8; EVENT_BUFFER_SIZE];
        ctx.state_mut().set_event_buffer(buffer.as_mut_ptr());
        ctx.state_mut().set_event_categories(EventCategories(0x3f));
        ctx.state_mut().enable_exit_capture();
        let fill = std::vec![0u8; EVENT_BUFFER_SIZE - EVENT_HEADER_SIZE];
        assert!(ctx.state_mut().event_append(EventKind::Serial, &fill));

        ctx.state_mut().emulated_tsc = 1;
        let apic = &mut ctx.state_mut().devices.apic;
        apic.svr = 1 << 8;
        apic.lvt_timer = 236;
        apic.timer_deadline = 1;

        // Mocks perform no hardware VM entry. The runner panics if the loop
        // reaches it while the timer is waiting in the pending-event slot.
        let result = unsafe {
            run_loop(
                &mut ctx,
                &mut MustNotEnterGuest,
                &MockMachine,
                &mut MockFrameAllocator::new(),
                0,
            )
        };
        assert!(
            matches!(result, Ok(ExitReason::EventBufferFull)),
            "{result:?}"
        );
        assert!(ctx.state().event_buffer_full());

        // The next RUN re-appends the timer, leaving room for the guest's
        // following exit record rather than silently dropping that record.
        ctx.state_mut().event_clear();
        ctx.state_mut()
            .capture_exit(ExitReason::CrAccess, 259, true);
        assert_eq!(
            ctx.state().event_buffer_len(),
            EVENT_HEADER_SIZE + 16 + EVENT_HEADER_SIZE + 512
        );
    }
}
