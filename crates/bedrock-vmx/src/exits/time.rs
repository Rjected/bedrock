// SPDX-License-Identifier: GPL-2.0

//! Deterministic time exit handlers (RDTSC, RDTSCP, RDPMC, MWAIT, HLT).

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::helpers::{advance_rip, ExitHandlerResult};

/// Handle RDTSC VM exit: return the emulated TSC in EDX:EAX.
pub fn handle_rdtsc<C: VmContext>(ctx: &mut C) -> ExitHandlerResult {
    let tsc = ctx.state().emulated_tsc;

    let gprs = &mut ctx.state_mut().gprs;
    gprs.rax = tsc & 0xFFFF_FFFF;
    gprs.rdx = tsc >> 32;

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}

/// Handle RDTSCP VM exit: emulated TSC in EDX:EAX, TSC_AUX in ECX.
pub fn handle_rdtscp<C: VmContext>(ctx: &mut C) -> ExitHandlerResult {
    let tsc = ctx.state().emulated_tsc;
    let tsc_aux = ctx.state().msr_state.tsc_aux;

    let gprs = &mut ctx.state_mut().gprs;
    gprs.rax = tsc & 0xFFFF_FFFF;
    gprs.rdx = tsc >> 32;
    gprs.rcx = tsc_aux & 0xFFFF_FFFF;

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}

/// Handle RDPMC VM exit by returning 0. With no PMU in CPUID.0AH this should
/// really inject #GP(0).
pub fn handle_rdpmc<C: VmContext>(ctx: &mut C) -> ExitHandlerResult {
    let gprs = &mut ctx.state_mut().gprs;
    gprs.rax = 0;
    gprs.rdx = 0;

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}

/// Handle HLT/MWAIT VM exit.
///
/// Advances the emulated TSC to the next wake source so it fires on the next
/// entry (no instructions retire while idle, so PEBS can't help). Only an
/// armed APIC timer is a wake source; the I/O-channel deadline and
/// `stop_at_tsc` only bound it. With no timer armed we don't advance: in the
/// window after a one-shot fires but before re-arm, jumping to a far I/O
/// deadline would overshoot the timer the guest is about to set.
pub fn handle_idle<C: VmContext>(ctx: &mut C) -> ExitHandlerResult {
    let current_tsc = ctx.state().emulated_tsc;
    let timer_deadline = ctx.state().devices.apic.timer_deadline;
    let io_channel_deadline = super::next_io_channel_target_tsc(ctx);
    let stop_at_tsc = ctx.state().stop_at_tsc;

    let wake = (timer_deadline > 0).then(|| match io_channel_deadline {
        Some(i) => timer_deadline.min(i),
        None => timer_deadline,
    });
    if let Some(wake) = wake {
        let target = match stop_at_tsc {
            Some(s) => wake.min(s),
            None => wake,
        };
        if target > current_tsc {
            let delta = target - current_tsc;
            ctx.state_mut().tsc_offset += delta;
            ctx.state_mut().emulated_tsc = target;
        }
    }

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}

#[cfg(test)]
#[path = "time_tests.rs"]
mod tests;
