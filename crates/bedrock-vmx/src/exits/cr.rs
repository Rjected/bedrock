// SPDX-License-Identifier: GPL-2.0

//! Control register access exit handler.

use super::helpers::{advance_rip, ExitError, ExitHandlerResult};
use super::qualifications::{CrAccessQualification, CrAccessType};

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// Handle CR access exit.
pub fn handle_cr_access<C: VmContext>(
    ctx: &mut C,
    qual: CrAccessQualification,
) -> ExitHandlerResult {
    let gpr_value = get_gpr_value(&ctx.state().gprs, qual.register);

    let vcpu = <C::V as Vmx>::current_vcpu();
    let caps = vcpu.capabilities();

    match qual.access_type {
        CrAccessType::MovToCr => {
            let result = match qual.cr_number {
                0 => {
                    // Apply VMX fixed bits (like bhyve); the read shadow keeps
                    // the guest's requested value.
                    let ones_mask = caps.cr0_fixed0 & caps.cr0_fixed1;
                    let zeros_mask = !caps.cr0_fixed0 & !caps.cr0_fixed1;
                    let cr0 = (gpr_value | ones_mask) & !zeros_mask;
                    ctx.state()
                        .vmcs
                        .write_natural(VmcsFieldNatural::GuestCr0, cr0)
                        .and_then(|()| {
                            ctx.state()
                                .vmcs
                                .write_natural(VmcsFieldNatural::Cr0ReadShadow, gpr_value)
                        })
                        .map_err(|_| ExitError::Fatal("Failed to write CR0"))
                }
                3 => {
                    // Clear bit 63 (PCID no-flush); the VMCS field must not set
                    // bits 63:MAXPHYADDR (SDM Vol 3C 28.3.1.1).
                    let cr3 = gpr_value & !(1u64 << 63);
                    let write_result = ctx
                        .state()
                        .vmcs
                        .write_natural(VmcsFieldNatural::GuestCr3, cr3)
                        .map_err(|_| ExitError::Fatal("Failed to write CR3"));

                    // VPID-tagged TLB entries survive VM entry/exit, so flush
                    // this VPID on CR3 writes (text_poke relies on it).
                    if caps.has_vpid {
                        if let Ok(vpid) = ctx.state().vmcs.read16(VmcsField16::VirtualProcessorId) {
                            let _ = <C::V as Vmx>::invvpid_single_context(vpid);
                        }
                    }
                    write_result
                }
                4 => {
                    // Same fixed-bit handling as CR0.
                    let ones_mask = caps.cr4_fixed0 & caps.cr4_fixed1;
                    let zeros_mask = !caps.cr4_fixed0 & !caps.cr4_fixed1;
                    let cr4 = (gpr_value | ones_mask) & !zeros_mask;
                    ctx.state()
                        .vmcs
                        .write_natural(VmcsFieldNatural::GuestCr4, cr4)
                        .and_then(|()| {
                            ctx.state()
                                .vmcs
                                .write_natural(VmcsFieldNatural::Cr4ReadShadow, gpr_value)
                        })
                        .map_err(|_| ExitError::Fatal("Failed to write CR4"))
                }
                8 => {
                    // CR8 (TPR) ignored.
                    Ok(())
                }
                _ => Err(ExitError::Fatal("Write to unsupported CR")),
            };

            if let Err(e) = result {
                return ExitHandlerResult::Error(e);
            }
        }
        CrAccessType::MovFromCr => {
            let value = match qual.cr_number {
                0 => ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr0),
                3 => ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr3),
                4 => ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr4),
                8 => Ok(0), // CR8 (TPR)
                _ => return ExitHandlerResult::Error(ExitError::Fatal("Read from unsupported CR")),
            };

            match value {
                Ok(v) => set_gpr_value(&mut ctx.state_mut().gprs, qual.register, v),
                Err(e) => return ExitHandlerResult::Error(ExitError::VmcsReadError(e)),
            }
        }
        CrAccessType::Clts => {
            let cr0 = match ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr0) {
                Ok(v) => v & !(1 << 3),
                Err(e) => return ExitHandlerResult::Error(ExitError::VmcsReadError(e)),
            };
            if ctx
                .state()
                .vmcs
                .write_natural(VmcsFieldNatural::GuestCr0, cr0)
                .is_err()
            {
                return ExitHandlerResult::Error(ExitError::Fatal("Failed to write CR0 for CLTS"));
            }
            if ctx
                .state()
                .vmcs
                .write_natural(VmcsFieldNatural::Cr0ReadShadow, cr0)
                .is_err()
            {
                return ExitHandlerResult::Error(ExitError::Fatal(
                    "Failed to write CR0 shadow for CLTS",
                ));
            }
        }
        CrAccessType::Lmsw => {
            let msw = qual.lmsw_source_data;
            let cr0 = match ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr0) {
                Ok(v) => (v & 0xFFFFFFF0) | (u64::from(msw) & 0xF),
                Err(e) => return ExitHandlerResult::Error(ExitError::VmcsReadError(e)),
            };
            if ctx
                .state()
                .vmcs
                .write_natural(VmcsFieldNatural::GuestCr0, cr0)
                .is_err()
            {
                return ExitHandlerResult::Error(ExitError::Fatal("Failed to write CR0 for LMSW"));
            }
            if ctx
                .state()
                .vmcs
                .write_natural(VmcsFieldNatural::Cr0ReadShadow, cr0)
                .is_err()
            {
                return ExitHandlerResult::Error(ExitError::Fatal(
                    "Failed to write CR0 shadow for LMSW",
                ));
            }
        }
    }

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}

/// Get value from a GPR by index.
pub fn get_gpr_value(gprs: &GeneralPurposeRegisters, index: u8) -> u64 {
    match index {
        0 => gprs.rax,
        1 => gprs.rcx,
        2 => gprs.rdx,
        3 => gprs.rbx,
        4 => gprs.rsp,
        5 => gprs.rbp,
        6 => gprs.rsi,
        7 => gprs.rdi,
        8 => gprs.r8,
        9 => gprs.r9,
        10 => gprs.r10,
        11 => gprs.r11,
        12 => gprs.r12,
        13 => gprs.r13,
        14 => gprs.r14,
        15 => gprs.r15,
        _ => 0,
    }
}

/// Set value of a GPR by index.
pub fn set_gpr_value(gprs: &mut GeneralPurposeRegisters, index: u8, value: u64) {
    match index {
        0 => gprs.rax = value,
        1 => gprs.rcx = value,
        2 => gprs.rdx = value,
        3 => gprs.rbx = value,
        4 => gprs.rsp = value,
        5 => gprs.rbp = value,
        6 => gprs.rsi = value,
        7 => gprs.rdi = value,
        8 => gprs.r8 = value,
        9 => gprs.r9 = value,
        10 => gprs.r10 = value,
        11 => gprs.r11 = value,
        12 => gprs.r12 = value,
        13 => gprs.r13 = value,
        14 => gprs.r14 = value,
        15 => gprs.r15 = value,
        _ => {}
    }
}
