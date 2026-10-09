// SPDX-License-Identifier: GPL-2.0
//! Instructions requiring software emulation when SVM uses guest TF to step.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::helpers::{advance_rip, ExitError, ExitHandlerResult};

pub(super) fn physical<C: VmContext>(ctx: &C, linear: u64) -> Result<GuestPhysAddr, ExitError> {
    let cr0 = ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr0)?;
    if cr0 & (1 << 31) == 0 {
        return Ok(GuestPhysAddr::new(linear));
    }
    let efer = ctx.state().vmcs.read64(VmcsField64::GuestIa32Efer)?;
    if efer & (1 << 10) == 0 {
        return Err(ExitError::Fatal("SVM emulation requires long-mode paging"));
    }
    super::ept::translate_gva_to_gpa(ctx, linear)
        .map_err(|_| ExitError::Fatal("SVM emulation address is not mapped"))
}

pub(crate) struct InstructionWindow {
    pub linear: u64,
    pub physical: GuestPhysAddr,
    pub bytes: [u8; 256],
    length: usize,
}

impl InstructionWindow {
    pub(crate) fn following_page<C: VmContext>(&self, ctx: &C) -> Result<GuestPhysAddr, ExitError> {
        physical(ctx, (self.linear & !4095).wrapping_add(4096))
    }

    pub(crate) fn read<C: VmContext>(ctx: &C) -> Result<Self, ExitError> {
        let v = &ctx.state().vmcs;
        let linear = v
            .read_natural(VmcsFieldNatural::GuestRip)?
            .wrapping_add(v.read_natural(VmcsFieldNatural::GuestCsBase)?);
        let physical = physical(ctx, linear)?;
        Self::read_at(ctx, linear, physical)
    }

    /// Reuse a translation only while the global gate guards every reachable
    /// guest page-table page. A table write revokes the gate before reentry.
    pub(crate) fn read_cached<C: VmContext>(ctx: &mut C) -> Result<Self, ExitError> {
        let v = &ctx.state().vmcs;
        let root = v.read_natural(VmcsFieldNatural::GuestCr3)?
            & 0x000f_ffff_ffff_f000;
        if !ctx.state().svm_guard.gate_ready
            || ctx.state().svm_guard.gate_dirty
            || ctx.state().svm_guard.gate_root != root
        {
            return Self::read(ctx);
        }
        let linear = v
            .read_natural(VmcsFieldNatural::GuestRip)?
            .wrapping_add(v.read_natural(VmcsFieldNatural::GuestCsBase)?);
        let page = linear & !4095;
        let physical = super::svm_batch::cached_code_translation(ctx, page)
            .map(|address| GuestPhysAddr::new(address | (linear & 4095)))
            .ok_or(ExitError::Fatal("SVM instruction address is not mapped"))?;
        Self::read_at(ctx, linear, physical)
    }

    fn read_at<C: VmContext>(
        ctx: &C,
        linear: u64,
        physical: GuestPhysAddr,
    ) -> Result<Self, ExitError> {
        let length = (4096 - (linear & 4095) as usize).min(256);
        let mut window = Self {
            linear,
            physical,
            bytes: [0; 256],
            length,
        };
        ctx.read_guest_memory(physical, &mut window.bytes[..length])
            .map_err(|_| ExitError::Fatal("SVM instruction fetch failed"))?;
        Ok(window)
    }

    fn fetch<C: VmContext>(&self, ctx: &C, offset: usize) -> Result<u8, ExitError> {
        if offset < self.length {
            return Ok(self.bytes[offset]);
        }
        let mut byte = [0];
        ctx.read_guest_memory(
            physical(ctx, self.linear.wrapping_add(offset as u64))?,
            &mut byte,
        )
        .map_err(|_| ExitError::Fatal("SVM instruction fetch failed"))?;
        Ok(byte[0])
    }
}

#[cfg(test)]
fn prepare_random_exit<C: VmContext>(ctx: &C) -> Result<bool, ExitError> {
    let window = InstructionWindow::read(ctx).ok();
    prepare_random_exit_with_window(ctx, window.as_ref())
}

/// SVM has no RDRAND/RDSEED intercept. Decode before entry and reuse the
/// existing controlled randomness handlers without executing the host RNG.
fn prepare_random_exit_with_window<C: VmContext>(
    ctx: &C,
    window: Option<&InstructionWindow>,
) -> Result<bool, ExitError> {
    let v = &ctx.state().vmcs;
    if v.read_natural(VmcsFieldNatural::GuestCr0)? & (1 << 31) != 0
        && v.read64(VmcsField64::GuestIa32Efer)? & (1 << 10) == 0
    {
        return Err(ExitError::Fatal(
            "SVM stepping does not support legacy paging",
        ));
    }
    if v.read32(VmcsField32::VmEntryInterruptionInfo)? & (1 << 31) != 0 {
        return Ok(false);
    }
    let cs = v.read32(VmcsField32::GuestCsAccessRights)?;
    let long = cs & (1 << 13) != 0;
    let rip = v.read_natural(VmcsFieldNatural::GuestRip)?;
    let base = v.read_natural(VmcsFieldNatural::GuestCsBase)?;
    let fetch = |offset: usize| -> Result<u8, ExitError> {
        if let Some(window) = window {
            return window.fetch(ctx, offset);
        }
        let address = physical(ctx, base.wrapping_add(rip).wrapping_add(offset as u64))?;
        let mut byte = [0];
        ctx.read_guest_memory(address, &mut byte)
            .map_err(|_| ExitError::Fatal("SVM instruction fetch failed"))?;
        Ok(byte[0])
    };
    // Unmapped instructions must fault through hardware, before any RNG can
    // execute. Legacy paged modes are rejected by physical().
    let first = match fetch(0) {
        Ok(byte) => byte,
        Err(_) => return Ok(false),
    };
    let mut byte = first;
    let mut len = 0;
    let mut operand_override = false;
    let mut rex = 0;
    while matches!(
        byte,
        0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf2 | 0xf3
    ) || (long && (0x40..=0x4f).contains(&byte))
    {
        if len == 12 {
            return Ok(false);
        }
        if byte == 0x66 {
            operand_override = true;
        }
        rex = if long && (0x40..=0x4f).contains(&byte) {
            byte
        } else {
            0
        };
        len += 1;
        byte = fetch(len)?;
    }
    if byte != 0x0f || fetch(len + 1)? != 0xc7 {
        return Ok(false);
    }
    let modrm = fetch(len + 2)?;
    let operation = (modrm >> 3) & 7;
    if modrm & 0xc0 != 0xc0 || !(operation == 6 || operation == 7) {
        return Ok(false);
    }
    let size = if long && rex & 8 != 0 {
        2
    } else if (cs & (1 << 14) != 0 || long) ^ operand_override {
        1
    } else {
        0
    };
    let register = (modrm & 7) | ((rex & 1) << 3);
    v.write32(
        VmcsField32::VmExitReason,
        if operation == 6 { 57 } else { 61 },
    )?;
    v.write32(VmcsField32::VmExitInstructionLen, (len + 3) as u32)?;
    v.write32(
        VmcsField32::VmExitInstructionInfo,
        (size << 11) | (u32::from(register) << 3),
    )?;
    v.write_natural(VmcsFieldNatural::ExitQualification, 0)?;
    Ok(true)
}

/// Load an already-accessed GDT or LDT stack descriptor without using TF. Hardware
/// suppresses the trap for MOV SS and would also retire the next instruction.
/// Other operand/descriptor forms still require equivalent software handling.
fn prepare_mov_ss_register<C: VmContext>(
    ctx: &C,
    modrm: u8,
    rex: u8,
    length: usize,
) -> Result<bool, ExitError> {
    if modrm & 0xf8 != 0xd0 || rex & 4 != 0 {
        return Ok(false);
    }
    let v = &ctx.state().vmcs;
    let register = (modrm & 7) | ((rex & 1) << 3);
    let selector = if register == 4 {
        v.read_natural(VmcsFieldNatural::GuestRsp)? as u16
    } else {
        super::cr::get_gpr_value(&ctx.state().gprs, register) as u16
    };
    let cr0 = v.read_natural(VmcsFieldNatural::GuestCr0)?;
    let (base, limit, attributes) = if cr0 & 1 == 0 {
        (u64::from(selector) << 4, 0xffff, 0x93)
    } else {
        // Let hardware deliver invalid-selector faults before retirement.
        // Null selectors and unaccessed descriptors need
        // separate handling; do not manufacture cached segment state for them.
        let cpl = (v.read32(VmcsField32::GuestCsAccessRights)? >> 5) & 3;
        if selector & !3 == 0 || u32::from(selector & 3) != cpl {
            return Ok(false);
        }
        let (table_base, table_limit) = if selector & 4 != 0 {
            let attributes = v.read32(VmcsField32::GuestLdtrAccessRights)?;
            if attributes & 0x1009f != 0x82 {
                return Ok(false);
            }
            (
                v.read_natural(VmcsFieldNatural::GuestLdtrBase)?,
                v.read32(VmcsField32::GuestLdtrLimit)?,
            )
        } else {
            (
                v.read_natural(VmcsFieldNatural::GuestGdtrBase)?,
                v.read32(VmcsField32::GuestGdtrLimit)?,
            )
        };
        let offset = u64::from(selector & !7);
        if offset + 7 > u64::from(table_limit) {
            return Ok(false);
        }
        let address = table_base.wrapping_add(offset);
        let mut descriptor = [0u8; 8];
        for (i, byte) in descriptor.iter_mut().enumerate() {
            let Ok(address) = physical(ctx, address.wrapping_add(i as u64)) else {
                return Ok(false);
            };
            if ctx
                .read_guest_memory(address, core::slice::from_mut(byte))
                .is_err()
            {
                return Ok(false);
            }
        }
        if descriptor[5] & 0x9b != 0x93 || u32::from((descriptor[5] >> 5) & 3) != cpl {
            return Ok(false);
        }
        let base = u64::from(u16::from_le_bytes([descriptor[2], descriptor[3]]))
            | (u64::from(descriptor[4]) << 16)
            | (u64::from(descriptor[7]) << 24);
        let mut limit = u32::from(u16::from_le_bytes([descriptor[0], descriptor[1]]))
            | (u32::from(descriptor[6] & 15) << 16);
        if descriptor[6] & 0x80 != 0 {
            limit = (limit << 12) | 0xfff;
        }
        (
            base,
            limit,
            u32::from(descriptor[5]) | (u32::from(descriptor[6] & 0xf0) << 8),
        )
    };
    v.write16(VmcsField16::GuestSsSelector, selector)?;
    v.write_natural(VmcsFieldNatural::GuestSsBase, base)?;
    v.write32(VmcsField32::GuestSsLimit, limit)?;
    v.write32(VmcsField32::GuestSsAccessRights, attributes)?;
    v.write_natural(
        VmcsFieldNatural::GuestRip,
        v.read_natural(VmcsFieldNatural::GuestRip)?
            .wrapping_add(length as u64),
    )?;
    v.write_natural(
        VmcsFieldNatural::GuestRflags,
        v.read_natural(VmcsFieldNatural::GuestRflags)? & !(1 << 16),
    )?;
    // SVM stores its one-instruction interrupt shadow in bit zero.
    v.write32(VmcsField32::GuestInterruptibilityState, 1)?;
    v.write32(VmcsField32::VmExitReason, 37)?;
    v.write_natural(VmcsFieldNatural::ExitQualification, 0)?;
    Ok(true)
}

pub(crate) fn prepare_instruction_exit<
    C: VmContext,
    R: super::super::traits::VmRunner<Vmcs = C::Vmcs>,
    A: CowAllocator<C::CowPage>,
>(
    ctx: &mut C,
    runner: &R,
    allocator: &mut A,
    window: &mut Option<InstructionWindow>,
) -> Result<bool, ExitError> {
    if super::svm_interrupts::pending_event(ctx, allocator)? {
        return Ok(true);
    }
    *window = InstructionWindow::read_cached(ctx).ok();
    if let Some(window) = window.as_ref() {
        super::svm_batch::remember_page(ctx, window.linear);
    }
    if prepare_random_exit_with_window(ctx, window.as_ref())? {
        return Ok(true);
    }
    let v = &ctx.state().vmcs;
    if v.read32(VmcsField32::VmEntryInterruptionInfo)? & (1 << 31) != 0 {
        return Ok(false);
    }
    let cs = v.read32(VmcsField32::GuestCsAccessRights)?;
    let rip = v.read_natural(VmcsFieldNatural::GuestRip)?;
    let base = v.read_natural(VmcsFieldNatural::GuestCsBase)?;
    let long = cs & (1 << 13) != 0;
    let fetch = |offset: usize| -> Result<u8, ExitError> {
        if let Some(window) = window.as_ref() {
            return window.fetch(ctx, offset);
        }
        let address = physical(ctx, base.wrapping_add(rip).wrapping_add(offset as u64))?;
        let mut byte = [0];
        ctx.read_guest_memory(address, &mut byte)
            .map_err(|_| ExitError::Fatal("SVM instruction fetch failed"))?;
        Ok(byte[0])
    };
    let mut opcode = match fetch(0) {
        Ok(byte) => byte,
        Err(_) => return Ok(false), // Hardware delivers an instruction-fetch fault.
    };
    let mut prefix_len = 0;
    let mut rex = 0;
    while matches!(
        opcode,
        0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf2 | 0xf3
    ) || (long && (0x40..=0x4f).contains(&opcode))
    {
        // A two-byte opcode after fourteen prefixes is architecturally invalid.
        if prefix_len == 13 {
            return Ok(false);
        }
        rex = if long && (0x40..=0x4f).contains(&opcode) {
            opcode
        } else {
            0
        };
        prefix_len += 1;
        opcode = fetch(prefix_len)?;
    }
    if opcode == 0x8e {
        return prepare_mov_ss_register(ctx, fetch(prefix_len + 1)?, rex, prefix_len + 2);
    }
    if opcode != 0x0f {
        return Ok(false);
    }
    let operation = fetch(prefix_len + 1)?;
    if !matches!(operation, 0x05 | 0x07) {
        return Ok(false);
    }
    let len = (prefix_len + 2) as u64;
    let wide = operation == 0x05 || rex & 8 != 0;
    if cs & (1 << 13) == 0 || v.read64(VmcsField64::GuestIa32Efer)? & 1 == 0 {
        return Err(ExitError::Fatal(
            "SVM system-call stepping requires long mode and EFER.SCE",
        ));
    }
    let star = runner
        .saved_guest_msr(v, msr::IA32_STAR)
        .ok_or(ExitError::Fatal("SVM STAR unavailable"))?;
    let (target, flags, selector) = if operation == 0x05 {
        let target = runner
            .saved_guest_msr(v, msr::IA32_LSTAR)
            .ok_or(ExitError::Fatal("SVM LSTAR unavailable"))?;
        let mask = runner
            .saved_guest_msr(v, msr::IA32_FMASK)
            .ok_or(ExitError::Fatal("SVM FMASK unavailable"))?;
        let flags = v.read_natural(VmcsFieldNatural::GuestRflags)?;
        (target, flags & !mask, ((star >> 32) & 0xfffc) as u16)
    } else {
        if cs & 0x60 != 0 {
            return Err(ExitError::Fatal("SVM SYSRET requires CPL zero"));
        }
        let gprs = ctx.state().gprs;
        let target = if wide {
            gprs.rcx
        } else {
            gprs.rcx as u32 as u64
        };
        let flags = (gprs.r11 & 0x3c7fd7) | 2;
        let selector = ((star >> 48) as u16 & !3).wrapping_add(if wide { 16 } else { 0 }) | 3;
        (target, flags, selector)
    };
    if ((target >> 47) != 0 && (target >> 47) != 0x1ffff) || selector == 0 {
        return Err(ExitError::Fatal("Invalid SVM system-call target"));
    }
    let ss_selector = if operation == 0x05 {
        selector.wrapping_add(8)
    } else {
        ((star >> 48) as u16 & !3).wrapping_add(8) | 3
    };
    let dpl = if operation == 0x05 { 0 } else { 0x60 };
    for (field, value) in [
        (
            VmcsField32::GuestCsAccessRights,
            (if wide { 0xa09b } else { 0xc09b }) | dpl,
        ),
        (VmcsField32::GuestSsAccessRights, 0xc093 | dpl),
        (VmcsField32::GuestCsLimit, u32::MAX),
        (VmcsField32::GuestSsLimit, u32::MAX),
    ] {
        v.write32(field, value)?;
    }
    v.write16(VmcsField16::GuestCsSelector, selector)?;
    v.write16(VmcsField16::GuestSsSelector, ss_selector)?;
    v.write_natural(VmcsFieldNatural::GuestCsBase, 0)?;
    v.write_natural(VmcsFieldNatural::GuestSsBase, 0)?;
    v.write_natural(VmcsFieldNatural::GuestRip, target)?;
    if operation == 0x05 {
        let original_flags = v.read_natural(VmcsFieldNatural::GuestRflags)?;
        ctx.state_mut().gprs.rcx = rip.wrapping_add(len);
        ctx.state_mut().gprs.r11 = original_flags & !(1 << 16);
    }
    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::GuestRflags, flags)?;
    ctx.state().vmcs.write32(VmcsField32::VmExitReason, 37)?;
    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::ExitQualification, 0)?;
    Ok(true)
}

fn emulate<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &mut A,
    push: bool,
) -> Result<ExitHandlerResult, ExitError> {
    let v = &ctx.state().vmcs;
    let cs = v.read32(VmcsField32::GuestCsAccessRights)?;
    let long = cs & (1 << 13) != 0;
    let rip = v.read_natural(VmcsFieldNatural::GuestRip)?;
    let cs_base = v.read_natural(VmcsFieldNatural::GuestCsBase)?;
    let mut override_size = false;
    let mut found_opcode = false;
    for offset in 0..15 {
        let mut opcode = [0u8; 1];
        ctx.read_guest_memory(
            physical(ctx, cs_base.wrapping_add(rip).wrapping_add(offset))?,
            &mut opcode,
        )
        .map_err(|_| ExitError::Fatal("SVM flags opcode unavailable"))?;
        let byte = opcode[0];
        if matches!(
            byte,
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf2 | 0xf3
        ) || (long && (0x40..=0x4f).contains(&byte))
        {
            override_size |= byte == 0x66;
            continue;
        }
        if byte != if push { 0x9c } else { 0x9d } {
            return Err(ExitError::Fatal("SVM flags opcode invalid"));
        }
        found_opcode = true;
        break;
    }
    if !found_opcode {
        return Err(ExitError::Fatal(
            "SVM flags instruction exceeds fifteen bytes",
        ));
    }
    let width = if long {
        if override_size {
            2
        } else {
            8
        }
    } else if (cs & (1 << 14) != 0) ^ override_size {
        4
    } else {
        2
    };
    let ss = v.read32(VmcsField32::GuestSsAccessRights)?;
    let stack_mask = if long {
        u64::MAX
    } else if ss & (1 << 14) != 0 {
        0xffff_ffff
    } else {
        0xffff
    };
    let rsp = v.read_natural(VmcsFieldNatural::GuestRsp)?;
    let offset = if push {
        rsp.wrapping_sub(width as u64)
    } else {
        rsp
    } & stack_mask;
    let ss_base = if long {
        0
    } else {
        v.read_natural(VmcsFieldNatural::GuestSsBase)?
    };
    let flags = v.read_natural(VmcsFieldNatural::GuestRflags)?;
    let mut addresses = [GuestPhysAddr::new(0); 8];
    for (i, address) in addresses[..width].iter_mut().enumerate() {
        *address = physical(ctx, ss_base.wrapping_add((offset + i as u64) & stack_mask))?;
    }
    if push {
        // PUSHF writes only these translated stack bytes. Preserve guarded
        // table/code proofs when disjoint, including physical stack aliases.
        let guard = &mut ctx.state_mut().svm_guard;
        if addresses[..width]
            .iter()
            .any(|address| guard.tables[..guard.count].contains(&(address.as_u64() & !4095)))
        {
            guard.valid = false;
            guard.code_count = 0;
        } else if addresses[..width].iter().any(|address| {
            guard.code[..guard.code_count]
                .iter()
                .any(|proof| proof.page == address.as_u64() & !4095)
        }) {
            guard.code_count = 0;
        }
        if ctx.is_forked() {
            for i in 0..width {
                if i == 0 || addresses[i].as_u64() >> 12 != addresses[i - 1].as_u64() >> 12 {
                    match ctx.handle_cow_fault(addresses[i], allocator) {
                        Some(ExitHandlerResult::Continue) => {}
                        Some(other) => return Ok(other),
                        None => {
                            return Ok(ExitHandlerResult::ExitToUserspace(
                                ExitReason::PoolExhausted,
                            ))
                        }
                    }
                }
            }
        }
        let bytes = (flags & !((1 << 16) | (1 << 17))).to_le_bytes();
        for i in 0..width {
            ctx.write_guest_memory(addresses[i], &bytes[i..i + 1])
                .map_err(|_| ExitError::Fatal("SVM PUSHF stack write failed"))?;
        }
    } else {
        let mut bytes = [0u8; 8];
        for i in 0..width {
            ctx.read_guest_memory(addresses[i], &mut bytes[i..i + 1])
                .map_err(|_| ExitError::Fatal("SVM POPF stack read failed"))?;
        }
        let value = u64::from_le_bytes(bytes);
        let cpl = (cs >> 5) & 3;
        let mut mask = 0x247fd5u64;
        if width == 2 {
            mask &= 0xffff;
        }
        if cpl != 0 {
            mask &= !(3 << 12);
        }
        if u64::from(cpl) > ((flags >> 12) & 3) {
            mask &= !(1 << 9);
        }
        ctx.state().vmcs.write_natural(
            VmcsFieldNatural::GuestRflags,
            ((flags & !mask) | (value & mask) | 2) & !(1 << 16),
        )?;
    }
    let new_rsp = if push {
        offset
    } else {
        offset.wrapping_add(width as u64) & stack_mask
    };
    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::GuestRsp, (rsp & !stack_mask) | new_rsp)?;
    advance_rip(ctx)?;
    Ok(ExitHandlerResult::Continue)
}

pub(super) fn handle_flags<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &mut A,
    push: bool,
) -> ExitHandlerResult {
    match emulate(ctx, allocator, push) {
        Ok(result) => result,
        Err(error) => ExitHandlerResult::Error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_mocks::MockFrameAllocator;
    use crate::tests::MockVmContext;

    fn context() -> MockVmContext {
        let ctx = MockVmContext::new();
        ctx.set_guest_rip(0x1000);
        ctx.set_instruction_len(1);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 0);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCsBase, 0);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestSsBase, 0);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestRsp, 0x8000);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestCsAccessRights, 0x9b);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestSsAccessRights, 0x93);
        ctx
    }

    #[test]
    fn mov_ss_register_preserves_next_instruction_boundary() {
        let mut ctx = context();
        ctx.state_mut().gprs.rax = 0x18;
        ctx.set_guest_rflags(0x202);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 1);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestGdtrBase, 0x9000);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestGdtrLimit, 0x1f);
        ctx.memory[0x9018..0x9020].copy_from_slice(&0x00cf93000000ffffu64.to_le_bytes());
        assert!(prepare_mov_ss_register(&ctx, 0xd0, 0, 2).unwrap());
        assert_eq!(ctx.get_guest_rip(), Some(0x1002));
        assert_eq!(
            ctx.state()
                .vmcs
                .read16(VmcsField16::GuestSsSelector)
                .unwrap(),
            0x18
        );
        assert_eq!(
            ctx.state().vmcs.read32(VmcsField32::GuestSsLimit).unwrap(),
            u32::MAX
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read32(VmcsField32::GuestInterruptibilityState)
                .unwrap(),
            1
        );
    }

    #[test]
    fn mov_ss_uses_cached_ldt_and_expand_down_descriptor() {
        let mut ctx = context();
        ctx.state_mut().gprs.rax = 0x1c;
        ctx.set_guest_rflags(0x202);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 1);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestLdtrBase, 0x9000);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestLdtrLimit, 0x1f);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestLdtrAccessRights, 0x82);
        // Base 0x12345678, byte limit 0xabcde, present expand-down data.
        ctx.memory[0x9018..0x9020]
            .copy_from_slice(&[0xde, 0xbc, 0x78, 0x56, 0x34, 0x97, 0x4a, 0x12]);
        assert!(prepare_mov_ss_register(&ctx, 0xd0, 0, 2).unwrap());
        assert_eq!(
            ctx.state()
                .vmcs
                .read_natural(VmcsFieldNatural::GuestSsBase)
                .unwrap(),
            0x12345678
        );
        assert_eq!(
            ctx.state().vmcs.read32(VmcsField32::GuestSsLimit).unwrap(),
            0xabcde
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read32(VmcsField32::GuestSsAccessRights)
                .unwrap(),
            0x4097
        );
        // LDT index zero is valid; only GDT index zero is a null selector.
        ctx.set_guest_rip(0x1000);
        ctx.state_mut().gprs.rax = 4;
        ctx.memory[0x9000..0x9008].copy_from_slice(&0x00cf93000000ffffu64.to_le_bytes());
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestLdtrAccessRights, 0xe2);
        assert!(prepare_mov_ss_register(&ctx, 0xd0, 0, 2).unwrap());
        assert_eq!(
            ctx.state()
                .vmcs
                .read16(VmcsField16::GuestSsSelector)
                .unwrap(),
            4
        );
        ctx.set_guest_rip(0x1000);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestLdtrAccessRights, 1 << 16);
        assert!(!prepare_mov_ss_register(&ctx, 0xd0, 0, 2).unwrap());
        assert_eq!(ctx.get_guest_rip(), Some(0x1000));
    }

    #[test]
    fn mov_ss_invalid_descriptor_does_not_retire() {
        let mut ctx = context();
        ctx.state_mut().gprs.rax = 0x18;
        ctx.set_guest_rflags(0x202);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 1);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestGdtrBase, 0x9000);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestGdtrLimit, 0x1f);
        for access in [0x13u8, 0x91, 0x9b, 0xf3] {
            ctx.memory[0x901d] = access;
            assert!(!prepare_mov_ss_register(&ctx, 0xd0, 0, 2).unwrap());
            assert_eq!(ctx.get_guest_rip(), Some(0x1000));
        }
        assert!(!prepare_mov_ss_register(&ctx, 0xd0, 4, 3).unwrap());
        assert!(!prepare_mov_ss_register(&ctx, 0x10, 0, 2).unwrap());
    }

    #[test]
    fn pushf_preserves_disjoint_proofs_and_revokes_stack_aliases() {
        for (table, code, rsp, valid, code_count) in [
            (0x3000, 0x1000, 0x8000, true, 1),
            (0x7000, 0x1000, 0x8000, false, 0),
            (0x3000, 0x7000, 0x8000, true, 0),
            (0x6000, 0x1000, 0x7004, false, 0),
        ] {
            let mut ctx = context();
            ctx.set_guest_rflags(0x202);
            ctx.vmcs_setup()
                .set_field32(VmcsField32::GuestCsAccessRights, 1 << 13);
            ctx.vmcs_setup()
                .set_field_natural(VmcsFieldNatural::GuestRsp, rsp);
            ctx.memory[0x1000] = 0x9c;
            let guard = &mut ctx.state_mut().svm_guard;
            guard.valid = true;
            guard.tables[0] = table;
            guard.count = 1;
            guard.code[0].page = code;
            guard.code_count = 1;
            assert_eq!(
                handle_flags(&mut ctx, &mut MockFrameAllocator::new(), true),
                ExitHandlerResult::Continue
            );
            assert_eq!(ctx.state().svm_guard.valid, valid);
            assert_eq!(ctx.state().svm_guard.code_count, code_count);
        }
        let mut ctx = context();
        ctx.set_guest_rflags(0x202);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestCsAccessRights, 1 << 13);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 1 << 31);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x3000);
        ctx.vmcs_setup()
            .write64(VmcsField64::GuestIa32Efer, 1 << 10)
            .unwrap();
        for (address, entry) in [
            (0x3000, 0x4007u64),
            (0x4000, 0x5007),
            (0x5000, 0x6007),
            (0x6008, 0x1007),
            (0x6038, 0x3007),
        ] {
            ctx.memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        ctx.memory[0x1000] = 0x9c;
        ctx.state_mut().svm_guard.valid = true;
        ctx.state_mut().svm_guard.tables[0] = 0x3000;
        ctx.state_mut().svm_guard.count = 1;
        // The virtual stack at 0x7000 aliases the active root at GPA 0x3000.
        assert_eq!(
            handle_flags(&mut ctx, &mut MockFrameAllocator::new(), true),
            ExitHandlerResult::Continue
        );
        assert!(!ctx.state().svm_guard.valid);
    }

    #[test]
    fn pushf_saves_logical_flags_and_updates_stack() {
        let mut ctx = context();
        ctx.memory[0x1000] = 0x9c;
        ctx.set_guest_rflags(0x202 | (1 << 16) | (1 << 17));
        assert_eq!(
            handle_flags(&mut ctx, &mut MockFrameAllocator::new(), true),
            ExitHandlerResult::Continue
        );
        assert_eq!(&ctx.memory[0x7ffe..0x8000], &0x202u16.to_le_bytes());
        assert_eq!(ctx.get_guest_rip(), Some(0x1001));
        assert_eq!(
            ctx.state()
                .vmcs
                .read_natural(VmcsFieldNatural::GuestRsp)
                .unwrap(),
            0x7ffe
        );
    }

    #[test]
    fn flags_operand_override_after_segment_prefix() {
        let mut ctx = context();
        ctx.memory[0x1000..0x1003].copy_from_slice(&[0x2e, 0x66, 0x9c]);
        ctx.set_instruction_len(3);
        ctx.set_guest_rflags(0x202);
        assert_eq!(
            handle_flags(&mut ctx, &mut MockFrameAllocator::new(), true),
            ExitHandlerResult::Continue
        );
        assert_eq!(&ctx.memory[0x7ffc..0x8000], &0x202u32.to_le_bytes());
        assert_eq!(ctx.get_guest_rip(), Some(0x1003));
    }

    #[test]
    fn oversized_flags_instruction_does_not_mutate_stack() {
        let mut ctx = context();
        ctx.memory[0x1000..0x100f].fill(0x66);
        ctx.memory[0x100f] = 0x9c;
        assert!(matches!(
            handle_flags(&mut ctx, &mut MockFrameAllocator::new(), true),
            ExitHandlerResult::Error(_)
        ));
        assert_eq!(
            ctx.state()
                .vmcs
                .read_natural(VmcsFieldNatural::GuestRsp)
                .unwrap(),
            0x8000
        );
        assert_eq!(&ctx.memory[0x7ff8..0x8000], &[0; 8]);
    }

    #[test]
    fn legacy_paging_cannot_bypass_rng_interception() {
        let ctx = context();
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 1 << 31);
        ctx.vmcs_setup()
            .write64(VmcsField64::GuestIa32Efer, 0)
            .unwrap();
        assert!(prepare_random_exit(&ctx).is_err());
    }

    #[test]
    fn user_popf_cannot_change_if_or_iopl() {
        let mut ctx = context();
        ctx.memory[0x1000] = 0x9d;
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestCsAccessRights, 0xfb);
        ctx.set_guest_rflags(0x202);
        ctx.memory[0x8000..0x8002].copy_from_slice(&0x3001u16.to_le_bytes());
        assert_eq!(
            handle_flags(&mut ctx, &mut MockFrameAllocator::new(), false),
            ExitHandlerResult::Continue
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read_natural(VmcsFieldNatural::GuestRflags)
                .unwrap(),
            0x203
        );
    }

    #[test]
    fn decode_rng_sizes_prefixes_and_high_registers() {
        let mut ctx = context();
        ctx.vmcs_setup()
            .set_field32(VmcsField32::VmEntryInterruptionInfo, 0);
        ctx.memory[0x1000..0x1004].copy_from_slice(&[0x66, 0x0f, 0xc7, 0xf3]);
        assert!(prepare_random_exit(&ctx).unwrap());
        assert_eq!(
            ctx.state().vmcs.read32(VmcsField32::VmExitReason).unwrap(),
            57
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read32(VmcsField32::VmExitInstructionInfo)
                .unwrap(),
            (1 << 11) | (3 << 3)
        );
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestCsAccessRights, 0x209b);
        ctx.memory[0x1000..0x1005].copy_from_slice(&[0x66, 0x49, 0x0f, 0xc7, 0xfa]);
        assert!(prepare_random_exit(&ctx).unwrap());
        assert_eq!(
            ctx.state().vmcs.read32(VmcsField32::VmExitReason).unwrap(),
            61
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read32(VmcsField32::VmExitInstructionLen)
                .unwrap(),
            5
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read32(VmcsField32::VmExitInstructionInfo)
                .unwrap(),
            (2 << 11) | (10 << 3)
        );
    }
}
