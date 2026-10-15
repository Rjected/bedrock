// SPDX-License-Identifier: GPL-2.0

//! EPT violation exit handler and GVA-to-GPA translation.

use super::apic::{
    handle_apic_access, handle_ioapic_access, APIC_BASE, APIC_SIZE, IOAPIC_BASE, IOAPIC_SIZE,
};
use super::helpers::ExitHandlerResult;
use super::pebs::{handle_pebs_precise_exit, is_pebs_induced};
use super::qualifications::EptViolationQualification;
use super::reasons::ExitReason;

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

// A guarded guest page-table write usually takes an NPT exit followed by a
// one-instruction TF replay. For the simple MOV r64 -> [base] form, the NPT
// exit already supplies the resolved destination, so retire the store here.
// Every other instruction keeps the hardware replay path.
fn emulate_guarded_table_mov<C: VmContext>(
    ctx: &mut C,
    qual: EptViolationQualification,
    guest_phys: u64,
) -> Result<bool, ExitError> {
    let candidate = (|| -> Option<(u64, u64)> {
        if !qual.write || !qual.guest_final_translation || guest_phys & 7 != 0
            || guest_phys & 4095 > 4096 - 8
            || super::svm_batch::instruction_budget(ctx) <= 1
            || ctx.state().mtf_enabled
        {
            return None;
        }
        let vmcs = &ctx.state().vmcs;
        if vmcs.read32(VmcsField32::GuestCsAccessRights).ok()? & (1 << 13) == 0
            || vmcs.read_natural(VmcsFieldNatural::GuestRflags).ok()? & ((1 << 8) | (1 << 16)) != 0
            || vmcs.read_natural(VmcsFieldNatural::GuestDr7).ok()? & 0x20ff != 0
            || vmcs.read32(VmcsField32::VmEntryInterruptionInfo).ok()? & (1 << 31) != 0
        {
            return None;
        }
        let rip = vmcs.read_natural(VmcsFieldNatural::GuestRip).ok()?;
        let rsp = vmcs.read_natural(VmcsFieldNatural::GuestRsp).ok()?;
        let window = super::svm::InstructionWindow::read_cached(ctx).ok()?;
        let [rex, opcode, modrm] = window.bytes[..3] else { return None; };
        if rex & 0xf8 != 0x48 || opcode != 0x89 || modrm >> 6 != 0
            || matches!(modrm & 7, 4 | 5)
        {
            return None;
        }
        let source = ((modrm >> 3) & 7) | ((rex & 4) << 1);
        let base = (modrm & 7) | ((rex & 1) << 3);
        let g = &ctx.state().gprs;
        let regs = [g.rax, g.rcx, g.rdx, g.rbx, rsp, g.rbp, g.rsi, g.rdi,
            g.r8, g.r9, g.r10, g.r11, g.r12, g.r13, g.r14, g.r15];
        let address = regs[base as usize];
        // The cached window reads only the current code page. Never decode
        // its zero-filled tail as bytes from the next page.
        if window.linear != rip || rip & 4095 > 4096 - 3
            || super::svm::physical(ctx, address).ok()?.as_u64() != guest_phys
        {
            return None;
        }
        Some((rip.checked_add(3)?, regs[source as usize]))
    })();
    let Some((next_rip, value)) = candidate else { return Ok(false); };
    if ctx.write_guest_memory_from_guest(GuestPhysAddr::new(guest_phys), &value.to_le_bytes()).is_err() {
        return Ok(false);
    }
    // Once the store is visible, replay would execute it twice. Treat a
    // failed RIP update as fatal rather than falling back to hardware replay.
    ctx.state().vmcs.write_natural(VmcsFieldNatural::GuestRip, next_rip)?;
    ctx.state_mut().instruction_counter.record_instructions(1);
    let count = ctx.state().instruction_counter.read();
    let offset = ctx.state().tsc_offset;
    ctx.state_mut().last_instruction_count = count;
    ctx.state_mut().emulated_tsc = count.wrapping_add(offset);
    ctx.state_mut().svm_gate_scalar_page = None;
    ctx.state_mut().svm_gate_pending_write = false;
    ctx.state_mut().svm_gate_replay_start = None;
    ctx.state_mut().svm_gate_replay_rip = None;
    Ok(true)
}

/// Handle EPT violation exit.
pub fn handle_ept_violation<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    qual: EptViolationQualification,
    allocator: &mut A,
) -> ExitHandlerResult {
    // PEBS-induced violations (qualification bit 16, EPT-friendly PEBS) are
    // asynchronous to the instruction and only matter for precise exits.
    if is_pebs_induced(&qual) {
        return handle_pebs_precise_exit(ctx);
    }

    let guest_phys = ctx
        .state()
        .vmcs
        .read64(VmcsField64::GuestPhysicalAddr)
        .unwrap_or(0);

    if (APIC_BASE..APIC_BASE + APIC_SIZE).contains(&guest_phys) {
        return handle_apic_access(ctx, guest_phys, qual);
    }

    if (IOAPIC_BASE..IOAPIC_BASE + IOAPIC_SIZE).contains(&guest_phys) {
        return handle_ioapic_access(ctx, guest_phys, qual);
    }

    if C::V::uses_nested_paging() {
        let page = guest_phys & !4095;
        if qual.execute {
            if ctx.state().svm_guard.gate_disabled_no_rogpt
                && ctx
                    .state_mut()
                    .ept
                    .permit_npt_execute_4k(allocator, GuestPhysAddr::new(page))
                    .is_some()
            {
                ctx.state_mut().svm_gate_scalar_page = None;
                return ExitHandlerResult::Continue;
            }
            let gate = &ctx.state().svm_guard;
            let may_trust = gate.gate_ready
                && !gate.gate_dirty
                && !super::svm_batch::gate_table_contains(gate, page);
            let safe = may_trust && super::svm_batch::globally_safe_code(ctx, page);
            if may_trust {
                super::svm_batch::protect_recurrent_code(ctx, allocator, page);
            }
            if safe {
                if ctx
                    .state_mut()
                    .ept
                    .trust_npt_code_4k(allocator, GuestPhysAddr::new(page))
                    .is_some()
                {
                    ctx.state_mut().svm_gate_scalar_page = None;
                    return ExitHandlerResult::Continue;
                }
            } else {
                ctx.state_mut().svm_gate_scalar_page = Some(page);
                return ExitHandlerResult::Continue;
            }
        }
        if qual.write && super::svm_batch::release_global_table_write(ctx, allocator, page) {
            match emulate_guarded_table_mov(ctx, qual, guest_phys) {
                Ok(true) => return ExitHandlerResult::Continue,
                Err(error) => return ExitHandlerResult::Error(error),
                Ok(false) => {}
            }
            let window = super::svm::InstructionWindow::read(ctx).ok();
            let scalar_page = window.as_ref().map(|window| window.physical.as_u64() & !4095);
            let replay_rip = window.as_ref()
                .and_then(|window| super::svm_batch::scalar_table_store_next_rip(ctx, window));
            ctx.state_mut().svm_gate_scalar_page = scalar_page;
            ctx.state_mut().svm_gate_pending_write = true;
            ctx.state_mut().svm_gate_replay_start =
                ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip).ok();
            ctx.state_mut().svm_gate_replay_rip = replay_rip;
            return ExitHandlerResult::Continue;
        }
        if qual.write && super::svm_batch::release_recurrent_code_write(ctx, allocator, page) {
            let scalar_page = super::svm::InstructionWindow::read(ctx)
                .ok()
                .map(|window| window.physical.as_u64() & !4095);
            ctx.state_mut().svm_gate_scalar_page = scalar_page;
            return ExitHandlerResult::Continue;
        }
        if qual.write
            && ctx
                .state()
                .ept
                .npt_trusted_code_4k(allocator, GuestPhysAddr::new(page))
        {
            if ctx
                .state_mut()
                .ept
                .invalidate_npt_code_4k(allocator, GuestPhysAddr::new(page))
                .is_some()
            {
                let scalar_page = super::svm::InstructionWindow::read(ctx)
                    .ok()
                    .map(|window| window.physical.as_u64() & !4095);
                ctx.state_mut().svm_gate_scalar_page = scalar_page;
                return ExitHandlerResult::Continue;
            }
        }
    }

    // CoW fault: write to a non-writable page (forked VMs start pages R+X).
    if qual.write && !qual.writable {
        if let Some(result) = ctx.handle_cow_fault(GuestPhysAddr::new(guest_phys), allocator) {
            return result;
        }
        // Pool exhausted: exit to refill in sleepable context.
        return ExitHandlerResult::ExitToUserspace(ExitReason::PoolExhausted);
    }

    let _guest_linear = if qual.guest_linear_valid {
        ctx.state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestLinearAddr)
            .unwrap_or(0)
    } else {
        0
    };

    let _rip = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestRip)
        .unwrap_or(0);

    log_err!(
        "EPT violation: GPA={:#x}, GLA={:#x}, RIP={:#x}\n",
        guest_phys,
        _guest_linear,
        _rip
    );
    log_err!(
        "  Access: read={}, write={}, execute={}\n",
        qual.read,
        qual.write,
        qual.execute
    );
    log_err!(
        "  EPT permissions: readable={}, writable={}, executable={}\n",
        qual.readable,
        qual.writable,
        qual.executable
    );

    ExitHandlerResult::ExitToUserspace(ExitReason::EptViolation)
}

/// Page size constant for GVA translation.
const PAGE_SIZE: u64 = 4096;

/// Translate each page of `[gva, gva+size)` into page-aligned GPAs in `gpas`.
/// Returns the number of pages filled; errors if any page fails to translate
/// or `gpas` is too small.
pub fn translate_gva_range_to_gpas<C: VmContext>(
    ctx: &C,
    gva: u64,
    size: u64,
    gpas: &mut [u64],
) -> Result<usize, ()> {
    if size == 0 {
        return Ok(0);
    }

    let start_page = gva & !0xFFF;
    let end_addr = gva.checked_add(size.saturating_sub(1)).ok_or(())?;
    let end_page = end_addr & !0xFFF;
    let num_pages = ((end_page - start_page) / PAGE_SIZE + 1) as usize;

    if num_pages > gpas.len() {
        return Err(());
    }

    for (i, gpa_slot) in gpas.iter_mut().enumerate().take(num_pages) {
        let page_gva = start_page + (i as u64 * PAGE_SIZE);
        let gpa = translate_gva_to_gpa(ctx, page_gva)?;
        *gpa_slot = gpa.as_u64() & !0xFFF;
    }

    Ok(num_pages)
}

/// Translate a GVA to a GPA by walking the guest's 4-level page tables.
/// Fails if any level is not present.
pub fn translate_gva_to_gpa<C: VmContext>(ctx: &C, gva: u64) -> Result<GuestPhysAddr, ()> {
    let cr3 = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestCr3)
        .map_err(|_| ())?;

    // CR3 bits 51:12 (PCID masked out).
    let pml4_addr = cr3 & 0x000F_FFFF_FFFF_F000;

    let pml4_index = ((gva >> 39) & 0x1FF) as usize;
    let pdpt_index = ((gva >> 30) & 0x1FF) as usize;
    let pd_index = ((gva >> 21) & 0x1FF) as usize;
    let pt_index = ((gva >> 12) & 0x1FF) as usize;
    let page_offset = gva & 0xFFF;

    let pml4e_addr = GuestPhysAddr::new(pml4_addr + (pml4_index * 8) as u64);
    let mut buf = [0u8; 8];
    ctx.read_guest_memory(pml4e_addr, &mut buf)
        .map_err(|_| ())?;
    let pml4e = u64::from_le_bytes(buf);

    if pml4e & 1 == 0 {
        return Err(());
    }

    let pdpt_addr = pml4e & 0x000F_FFFF_FFFF_F000;

    let pdpte_addr = GuestPhysAddr::new(pdpt_addr + (pdpt_index * 8) as u64);
    ctx.read_guest_memory(pdpte_addr, &mut buf)
        .map_err(|_| ())?;
    let pdpte = u64::from_le_bytes(buf);

    if pdpte & 1 == 0 {
        return Err(());
    }

    // 1GB page (PS bit)
    if pdpte & (1 << 7) != 0 {
        let page_base = pdpte & 0x000F_FFFF_C000_0000; // 1GB aligned
        let offset_1g = gva & 0x3FFF_FFFF; // 30-bit offset
        return Ok(GuestPhysAddr::new(page_base | offset_1g));
    }

    let pd_addr = pdpte & 0x000F_FFFF_FFFF_F000;

    let pde_addr = GuestPhysAddr::new(pd_addr + (pd_index * 8) as u64);
    ctx.read_guest_memory(pde_addr, &mut buf).map_err(|_| ())?;
    let pde = u64::from_le_bytes(buf);

    if pde & 1 == 0 {
        return Err(());
    }

    // 2MB page (PS bit)
    if pde & (1 << 7) != 0 {
        let page_base = pde & 0x000F_FFFF_FFE0_0000; // 2MB aligned
        let offset_2m = gva & 0x1F_FFFF; // 21-bit offset
        return Ok(GuestPhysAddr::new(page_base | offset_2m));
    }

    let pt_addr = pde & 0x000F_FFFF_FFFF_F000;

    let pte_addr = GuestPhysAddr::new(pt_addr + (pt_index * 8) as u64);
    ctx.read_guest_memory(pte_addr, &mut buf).map_err(|_| ())?;
    let pte = u64::from_le_bytes(buf);

    if pte & 1 == 0 {
        return Err(());
    }

    let page_base = pte & 0x000F_FFFF_FFFF_F000;
    Ok(GuestPhysAddr::new(page_base | page_offset))
}
