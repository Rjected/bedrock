// SPDX-License-Identifier: GPL-2.0

//! RDRAND/RDSEED VM exit handlers, emulated per the VM's `RandomState` mode.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::helpers::{advance_rip, emit_randomness_event, ExitError, ExitHandlerResult};
use super::qualifications::{RdrandInstructionInfo, RdrandOperandSize};
use super::reasons::ExitReason;

/// Read the instruction information for RDRAND/RDSEED from VMCS.
fn read_instruction_info<C: VmContext>(ctx: &C) -> Result<RdrandInstructionInfo, ExitError> {
    let info = ctx
        .state()
        .vmcs
        .read32(VmcsField32::VmExitInstructionInfo)
        .map_err(|_| ExitError::Fatal("Failed to read VM-exit instruction info"))?;
    Ok(RdrandInstructionInfo::from(info))
}

/// Write `value` to GPR `index` with RDRAND operand-size semantics (16-bit
/// preserves the upper bits, 32-bit zero-extends).
fn write_gpr_by_index(
    gprs: &mut GeneralPurposeRegisters,
    index: u8,
    value: u64,
    size: RdrandOperandSize,
) {
    let masked_value = match size {
        RdrandOperandSize::Size16 => value & 0xFFFF,
        RdrandOperandSize::Size32 => value & 0xFFFF_FFFF,
        RdrandOperandSize::Size64 => value,
    };

    let reg = match index {
        0 => &mut gprs.rax,
        1 => &mut gprs.rcx,
        2 => &mut gprs.rdx,
        3 => &mut gprs.rbx,
        4 => &mut gprs.rsp,
        5 => &mut gprs.rbp,
        6 => &mut gprs.rsi,
        7 => &mut gprs.rdi,
        8 => &mut gprs.r8,
        9 => &mut gprs.r9,
        10 => &mut gprs.r10,
        11 => &mut gprs.r11,
        12 => &mut gprs.r12,
        13 => &mut gprs.r13,
        14 => &mut gprs.r14,
        15 => &mut gprs.r15,
        _ => return,
    };

    match size {
        RdrandOperandSize::Size16 => {
            *reg = (*reg & !0xFFFF) | masked_value;
        }
        RdrandOperandSize::Size32 => {
            *reg = masked_value;
        }
        RdrandOperandSize::Size64 => {
            *reg = masked_value;
        }
    }
}

/// Set RFLAGS.CF (RDRAND's success flag).
fn set_cf_flag<C: VmContext>(ctx: &mut C, cf: bool) -> Result<(), ExitError> {
    let rflags = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestRflags)
        .map_err(|_| ExitError::Fatal("Failed to read guest RFLAGS"))?;

    let new_rflags = if cf { rflags | 0x1 } else { rflags & !0x1 };

    ctx.state()
        .vmcs
        .write_natural(VmcsFieldNatural::GuestRflags, new_rflags)
        .map_err(|_| ExitError::Fatal("Failed to write guest RFLAGS"))?;

    Ok(())
}

/// Handle RDRAND VM exit. In ExitToUserspace mode without a pending value,
/// exits so userspace can provide one.
pub fn handle_rdrand<C: VmContext>(ctx: &mut C) -> ExitHandlerResult {
    handle_random(ctx, RandomSource::Rdrand)
}

/// Shared RDRAND/RDSEED emulation; `source` is recorded in the event.
fn handle_random<C: VmContext>(ctx: &mut C, source: RandomSource) -> ExitHandlerResult {
    let info = match read_instruction_info(ctx) {
        Ok(i) => i,
        Err(e) => return ExitHandlerResult::Error(e),
    };

    if ctx.state().devices.random.needs_rdrand_exit() {
        // Don't advance RIP: re-execute once userspace provides the value.
        return ExitHandlerResult::ExitToUserspace(ExitReason::Rdrand);
    }

    let value = match ctx.state_mut().devices.random.generate() {
        Some(v) => v,
        None => {
            return ExitHandlerResult::ExitToUserspace(ExitReason::Rdrand);
        }
    };

    // Record the served value on the randomness event stream (inline, no
    // trailing bytes).
    let width: u8 = match info.operand_size {
        RdrandOperandSize::Size16 => 2,
        RdrandOperandSize::Size32 => 4,
        RdrandOperandSize::Size64 => 8,
    };
    let payload = RandomPayload {
        value,
        source: source as u8,
        width,
        ..RandomPayload::default()
    };
    emit_randomness_event(ctx, &payload, &[]);

    write_gpr_by_index(
        &mut ctx.state_mut().gprs,
        info.dest_reg,
        value,
        info.operand_size,
    );

    if let Err(e) = set_cf_flag(ctx, true) {
        return ExitHandlerResult::Error(e);
    }

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}

/// Handle RDSEED VM exit; emulated identically to RDRAND except for the
/// recorded source.
pub fn handle_rdseed<C: VmContext>(ctx: &mut C) -> ExitHandlerResult {
    handle_random(ctx, RandomSource::Rdseed)
}

#[cfg(test)]
#[path = "rdrand_tests.rs"]
mod tests;
