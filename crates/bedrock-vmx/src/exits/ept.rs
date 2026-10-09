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
                && !gate.gate_tables[..gate.gate_count].contains(&page);
            let safe = may_trust && super::svm_batch::globally_safe_code(ctx, page);
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
