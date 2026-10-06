// SPDX-License-Identifier: GPL-2.0
//! Deliver SVM guest events with logical flags, before starting the TF step.
//! Hardware delivery would clear TF and expose it in the saved guest frame.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
use super::helpers::{ExitError, ExitHandlerResult};
use super::svm::physical;
#[cfg(feature = "cargo")]
use crate::prelude::*;

fn read<C: VmContext>(ctx: &C, linear: u64, bytes: &mut [u8]) -> Result<(), ExitError> {
    for (i, byte) in bytes.iter_mut().enumerate() {
        ctx.read_guest_memory(
            physical(ctx, linear.wrapping_add(i as u64))?,
            core::slice::from_mut(byte),
        )
        .map_err(|_| ExitError::Fatal("SVM event state is not readable"))?;
    }
    Ok(())
}

fn read64<C: VmContext>(ctx: &C, linear: u64) -> Result<u64, ExitError> {
    let mut bytes = [0; 8];
    read(ctx, linear, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn write_frame<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &mut A,
    linear: u64,
    bytes: &[u8],
) -> Result<bool, ExitError> {
    // Resolve and COW the whole frame before writing any byte. A failed
    // allocation leaves the pending event available for the next RUN.
    let mut addresses = [GuestPhysAddr::new(0); 48];
    for (i, address) in addresses[..bytes.len()].iter_mut().enumerate() {
        *address = physical(ctx, linear.wrapping_add(i as u64))?;
    }
    if ctx.is_forked() {
        for i in 0..bytes.len() {
            if i == 0 || addresses[i].as_u64() >> 12 != addresses[i - 1].as_u64() >> 12 {
                match ctx.handle_cow_fault(addresses[i], allocator) {
                    Some(ExitHandlerResult::Continue) => {}
                    None | Some(ExitHandlerResult::ExitToUserspace(ExitReason::PoolExhausted)) => {
                        return Ok(false)
                    }
                    _ => return Err(ExitError::Fatal("SVM event stack COW failed")),
                }
            }
        }
    }
    for (i, byte) in bytes.iter().enumerate() {
        ctx.write_guest_memory(addresses[i], core::slice::from_ref(byte))
            .map_err(|_| ExitError::Fatal("SVM event stack write failed"))?;
    }
    Ok(true)
}

fn code_segment<C: VmContext>(ctx: &C, selector: u16) -> Result<(u32, u32), ExitError> {
    let v = &ctx.state().vmcs;
    if selector & 4 != 0 || selector & !7 == 0 {
        return Err(ExitError::Fatal("SVM event requires a GDT code selector"));
    }
    let offset = u64::from(selector & !7);
    if offset + 7 > u64::from(v.read32(VmcsField32::GuestGdtrLimit)?) {
        return Err(ExitError::Fatal("SVM event code selector exceeds GDT"));
    }
    let mut descriptor = [0; 8];
    read(
        ctx,
        v.read_natural(VmcsFieldNatural::GuestGdtrBase)? + offset,
        &mut descriptor,
    )?;
    if descriptor[5] & 0x98 != 0x98 || descriptor[6] & 0x20 == 0 {
        return Err(ExitError::Fatal(
            "SVM event requires a present 64-bit code segment",
        ));
    }
    let attributes = u32::from(descriptor[5] | 1) | (u32::from(descriptor[6] & 0xf0) << 8);
    let mut limit = u32::from(u16::from_le_bytes([descriptor[0], descriptor[1]]))
        | (u32::from(descriptor[6] & 15) << 16);
    if descriptor[6] & 0x80 != 0 {
        limit = (limit << 12) | 0xfff;
    }
    Ok((attributes, limit))
}

fn deliver<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &mut A,
    vector: u8,
    return_rip: u64,
    software: bool,
    error: Option<u32>,
    fault: bool,
) -> Result<bool, ExitError> {
    let v = &ctx.state().vmcs;
    let flags = v.read_natural(VmcsFieldNatural::GuestRflags)?;
    let old_cs = v.read16(VmcsField16::GuestCsSelector)?;
    let old_ss = v.read16(VmcsField16::GuestSsSelector)?;
    let old_rsp = v.read_natural(VmcsFieldNatural::GuestRsp)?;
    let idt_base = v.read_natural(VmcsFieldNatural::GuestIdtrBase)?;
    let idt_limit = u64::from(v.read32(VmcsField32::GuestIdtrLimit)?);
    let cr0 = v.read_natural(VmcsFieldNatural::GuestCr0)?;
    if cr0 & 1 == 0 {
        if error.is_some() {
            return Err(ExitError::Fatal("SVM real-mode event error code"));
        }
        let offset = u64::from(vector) * 4;
        if offset + 3 > idt_limit {
            return Err(ExitError::Fatal("SVM real-mode vector exceeds IDT"));
        }
        let mut gate = [0; 4];
        read(ctx, idt_base + offset, &mut gate)?;
        let target = u16::from_le_bytes([gate[0], gate[1]]);
        let selector = u16::from_le_bytes([gate[2], gate[3]]);
        let sp = (old_rsp as u16).wrapping_sub(6);
        // A wrapping frame needs separate writes; reject it before mutation.
        if sp > 0xfffa {
            return Err(ExitError::Fatal("SVM real-mode interrupt stack wraps"));
        }
        let mut frame = [0; 6];
        for (i, word) in [return_rip as u16, old_cs, flags as u16].iter().enumerate() {
            frame[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
        }
        let ss_base = v.read_natural(VmcsFieldNatural::GuestSsBase)?;
        if !write_frame(ctx, allocator, ss_base + u64::from(sp), &frame)? {
            return Ok(false);
        }
        let v = &ctx.state().vmcs;
        v.write16(VmcsField16::GuestCsSelector, selector)?;
        v.write_natural(VmcsFieldNatural::GuestCsBase, u64::from(selector) << 4)?;
        v.write_natural(VmcsFieldNatural::GuestRip, u64::from(target))?;
        v.write_natural(
            VmcsFieldNatural::GuestRsp,
            (old_rsp & !0xffff) | u64::from(sp),
        )?;
        v.write_natural(
            VmcsFieldNatural::GuestRflags,
            flags & !((1 << 8) | (1 << 9)),
        )?;
        return Ok(true);
    }
    if v.read64(VmcsField64::GuestIa32Efer)? & (1 << 10) == 0 {
        return Err(ExitError::Fatal(
            "SVM protected-mode event delivery requires long mode",
        ));
    }
    let offset = u64::from(vector) * 16;
    if offset + 15 > idt_limit {
        return Err(ExitError::Fatal("SVM event vector exceeds IDT"));
    }
    let mut gate = [0; 16];
    read(ctx, idt_base + offset, &mut gate)?;
    let gate_type = gate[5] & 0x1f;
    if gate[5] & 0x80 == 0 || !matches!(gate_type, 14 | 15) {
        return Err(ExitError::Fatal(
            "SVM event requires an interrupt or trap gate",
        ));
    }
    let old_cpl = (v.read32(VmcsField32::GuestCsAccessRights)? >> 5) & 3;
    if software && old_cpl > u32::from((gate[5] >> 5) & 3) {
        return Err(ExitError::Fatal("SVM software interrupt exceeds gate DPL"));
    }
    let selector = u16::from_le_bytes([gate[2], gate[3]]);
    let (attributes, limit) = code_segment(ctx, selector)?;
    let new_cpl = if attributes & 4 != 0 {
        old_cpl
    } else {
        (attributes >> 5) & 3
    };
    if new_cpl > old_cpl {
        return Err(ExitError::Fatal(
            "SVM event enters a less privileged code segment",
        ));
    }
    let target = u64::from(u16::from_le_bytes([gate[0], gate[1]]))
        | (u64::from(u16::from_le_bytes([gate[6], gate[7]])) << 16)
        | (u64::from(u32::from_le_bytes(gate[8..12].try_into().unwrap())) << 32);
    if target >> 47 != 0 && target >> 47 != 0x1ffff {
        return Err(ExitError::Fatal("SVM event target is noncanonical"));
    }
    let ist = gate[4] & 7;
    let mut stack = old_rsp;
    if ist != 0 || new_cpl < old_cpl {
        let tss = v.read_natural(VmcsFieldNatural::GuestTrBase)?;
        let stack_offset = if ist != 0 {
            36 + u64::from(ist - 1) * 8
        } else {
            4 + u64::from(new_cpl) * 8
        };
        stack = read64(ctx, tss + stack_offset)?;
    }
    let frame_size = if error.is_some() { 48 } else { 40 };
    let rsp = (stack & !15).wrapping_sub(frame_size as u64);
    let saved_flags = if fault { flags | (1 << 16) } else { flags };
    let mut frame = [0; 48];
    let words = [
        return_rip,
        u64::from(old_cs),
        saved_flags,
        old_rsp,
        u64::from(old_ss),
    ];
    let start = if let Some(code) = error {
        frame[..8].copy_from_slice(&u64::from(code).to_le_bytes());
        8
    } else {
        0
    };
    for (i, word) in words.iter().enumerate() {
        frame[start + i * 8..start + i * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    if !write_frame(ctx, allocator, rsp, &frame[..frame_size])? {
        return Ok(false);
    }
    let v = &ctx.state().vmcs;
    v.write16(
        VmcsField16::GuestCsSelector,
        (selector & !3) | new_cpl as u16,
    )?;
    v.write32(VmcsField32::GuestCsAccessRights, attributes)?;
    v.write32(VmcsField32::GuestCsLimit, limit)?;
    v.write_natural(VmcsFieldNatural::GuestCsBase, 0)?;
    if new_cpl < old_cpl {
        v.write16(VmcsField16::GuestSsSelector, 0)?;
        v.write32(VmcsField32::GuestSsAccessRights, 0xc093 | (new_cpl << 5))?;
        v.write32(VmcsField32::GuestSsLimit, u32::MAX)?;
        v.write_natural(VmcsFieldNatural::GuestSsBase, 0)?;
    }
    v.write_natural(VmcsFieldNatural::GuestRsp, rsp)?;
    v.write_natural(VmcsFieldNatural::GuestRip, target)?;
    let mut cleared = (1 << 8) | (1 << 14) | (1 << 16) | (1 << 17);
    if gate_type == 14 {
        cleared |= 1 << 9;
    }
    v.write_natural(VmcsFieldNatural::GuestRflags, flags & !cleared)?;
    Ok(true)
}

pub(super) fn pending_event<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &mut A,
) -> Result<bool, ExitError> {
    let info = ctx
        .state()
        .vmcs
        .read32(VmcsField32::VmEntryInterruptionInfo)?;
    if info & (1 << 31) == 0 {
        return Ok(false);
    }
    let kind = (info >> 8) & 7;
    if !matches!(kind, 0 | 3) {
        return Err(ExitError::Fatal("Unsupported SVM event type"));
    }
    let vector = info as u8;
    let error = if info & (1 << 11) != 0 {
        Some(
            ctx.state()
                .vmcs
                .read32(VmcsField32::VmEntryExceptionErrorCode)?,
        )
    } else {
        None
    };
    let rip = ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip)?;
    let fault = kind == 3 && !matches!(vector, 1 | 3 | 4);
    let delivered = deliver(ctx, allocator, vector, rip, false, error, fault)?;
    let v = &ctx.state().vmcs;
    if delivered {
        v.write32(VmcsField32::VmEntryInterruptionInfo, 0)?;
    }
    v.write32(
        VmcsField32::VmExitReason,
        if delivered {
            1
        } else {
            ExitReason::PoolExhausted as u32
        },
    )?;
    v.write_natural(VmcsFieldNatural::ExitQualification, 0)?;
    Ok(true)
}

pub(super) fn software_interrupt<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &mut A,
) -> ExitHandlerResult {
    let result = (|| -> Result<ExitHandlerResult, ExitError> {
        let rip = ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip)?;
        let base = ctx
            .state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestCsBase)?;
        let mut bytes = [0; 2];
        read(ctx, base.wrapping_add(rip), &mut bytes)?;
        let (vector, len) = match bytes[0] {
            0xcc => (3, 1),
            0xcd => (bytes[1], 2),
            0xce => {
                if ctx
                    .state()
                    .vmcs
                    .read_natural(VmcsFieldNatural::GuestRflags)?
                    & (1 << 11)
                    == 0
                {
                    ctx.state()
                        .vmcs
                        .write_natural(VmcsFieldNatural::GuestRip, rip + 1)?;
                    return Ok(ExitHandlerResult::Continue);
                }
                (4, 1)
            }
            _ => {
                return Err(ExitError::Fatal(
                    "SVM software interrupt encoding unsupported",
                ))
            }
        };
        if deliver(
            ctx,
            allocator,
            vector,
            rip.wrapping_add(len),
            true,
            None,
            false,
        )? {
            Ok(ExitHandlerResult::Continue)
        } else {
            Ok(ExitHandlerResult::ExitToUserspace(
                ExitReason::PoolExhausted,
            ))
        }
    })();
    result.unwrap_or_else(ExitHandlerResult::Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_mocks::MockFrameAllocator;
    use crate::tests::MockVmContext;

    #[test]
    fn real_mode_irq_saves_logical_flags_without_retiring_an_instruction() {
        let mut ctx = MockVmContext::new();
        ctx.memory[0x80..0x84].copy_from_slice(&[0, 0x20, 0, 0]);
        ctx.vmcs_setup()
            .write16(VmcsField16::GuestCsSelector, 0)
            .unwrap();
        ctx.vmcs_setup()
            .write16(VmcsField16::GuestSsSelector, 0)
            .unwrap();
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 0);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestIdtrBase, 0);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestSsBase, 0);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestRsp, 0x8000);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestIdtrLimit, 0x3ff);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::VmEntryInterruptionInfo, 0x80000020);
        ctx.set_guest_rip(0x1000);
        ctx.set_guest_rflags(0x202);
        assert!(pending_event(&mut ctx, &mut MockFrameAllocator::new()).unwrap());
        assert_eq!(&ctx.memory[0x7ffa..0x8000], &[0, 0x10, 0, 0, 2, 2]);
        assert_eq!(ctx.get_guest_rip(), Some(0x2000));
        assert_eq!(
            ctx.state()
                .vmcs
                .read_natural(VmcsFieldNatural::GuestRflags)
                .unwrap(),
            2
        );
        assert_eq!(
            ctx.state().vmcs.read32(VmcsField32::VmExitReason).unwrap(),
            1
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read32(VmcsField32::VmEntryInterruptionInfo)
                .unwrap(),
            0
        );
    }

    #[test]
    fn long_mode_user_page_fault_uses_tss_stack_and_saves_error_and_rf() {
        let mut ctx = MockVmContext::new();
        for (address, word) in [
            (0x3000, 0x4007u64),
            (0x4000, 0x5007),
            (0x5000, 0x87),
            (0x6008, 0x00af9b000000ffff),
            (0x9004, 0xb000),
        ] {
            ctx.memory[address..address + 8].copy_from_slice(&word.to_le_bytes());
        }
        ctx.memory[0x70e0..0x70f0]
            .copy_from_slice(&[0, 0x20, 8, 0, 0, 0x8e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let v = ctx.vmcs_setup();
        for (field, value) in [
            (VmcsFieldNatural::GuestCr0, 0x80000001),
            (VmcsFieldNatural::GuestCr3, 0x3000),
            (VmcsFieldNatural::GuestIdtrBase, 0x7000),
            (VmcsFieldNatural::GuestGdtrBase, 0x6000),
            (VmcsFieldNatural::GuestTrBase, 0x9000),
            (VmcsFieldNatural::GuestRsp, 0x8000),
        ] {
            v.set_field_natural(field, value);
        }
        v.write64(VmcsField64::GuestIa32Efer, 0x500).unwrap();
        v.write16(VmcsField16::GuestCsSelector, 0x33).unwrap();
        v.write16(VmcsField16::GuestSsSelector, 0x2b).unwrap();
        v.set_field32(VmcsField32::GuestCsAccessRights, 0xa0fb);
        v.set_field32(VmcsField32::GuestGdtrLimit, 0x17);
        v.set_field32(VmcsField32::GuestIdtrLimit, 0xfff);
        v.set_field32(VmcsField32::VmEntryInterruptionInfo, 0x80000b0e);
        v.set_field32(VmcsField32::VmEntryExceptionErrorCode, 6);
        ctx.set_guest_rip(0x1000);
        ctx.set_guest_rflags(0x202);
        assert!(pending_event(&mut ctx, &mut MockFrameAllocator::new()).unwrap());
        let words: [u64; 6] = core::array::from_fn(|i| {
            u64::from_le_bytes(
                ctx.memory[0xafd0 + i * 8..0xafd8 + i * 8]
                    .try_into()
                    .unwrap(),
            )
        });
        assert_eq!(words, [6, 0x1000, 0x33, 0x10202, 0x8000, 0x2b]);
        assert_eq!(
            ctx.state()
                .vmcs
                .read_natural(VmcsFieldNatural::GuestRsp)
                .unwrap(),
            0xafd0
        );
        assert_eq!(ctx.get_guest_rip(), Some(0x2000));
        assert_eq!(
            ctx.state()
                .vmcs
                .read_natural(VmcsFieldNatural::GuestRflags)
                .unwrap(),
            2
        );
        assert_eq!(
            ctx.state()
                .vmcs
                .read16(VmcsField16::GuestCsSelector)
                .unwrap(),
            8
        );
    }
}
