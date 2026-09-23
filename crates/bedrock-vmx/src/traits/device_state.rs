// SPDX-License-Identifier: GPL-2.0

//! Grouped emulated device and MSR state.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// All emulated device state the exit handlers need.
#[derive(Clone)]
pub struct DeviceStates {
    pub apic: ApicState,
    /// 8250/16550 UART.
    pub serial: SerialState,
    pub ioapic: IoApicState,
    /// CMOS clock.
    pub rtc: RtcState,
    pub mtrr: MtrrState,
    /// Controlled-randomness device: RDRAND, RDSEED, and the
    /// `HYPERCALL_GET_RANDOM` (`/dev/urandom` / `getrandom()`) chokepoint.
    pub random: RandomState,
}

impl DeviceStates {
    pub fn new() -> Self {
        Self {
            apic: ApicState::default(),
            serial: SerialState::default(),
            ioapic: IoApicState::default(),
            rtc: RtcState::default(),
            mtrr: MtrrState::default(),
            random: RandomState::default(),
        }
    }
}

impl Default for DeviceStates {
    fn default() -> Self {
        Self::new()
    }
}

/// Guest MSRs emulated by the hypervisor rather than passed through.
#[derive(Clone, Copy)]
pub struct GuestMsrState {
    /// IA32_PAT (0x277) - Page Attribute Table.
    pub pat: u64,
    /// IA32_TSC_AUX (0xC0000103) - auxiliary value for RDTSCP.
    pub tsc_aux: u64,
    /// SYSCALL/SYSRET MSRs (STAR, LSTAR, CSTAR, FMASK).
    pub syscall: SyscallMsrs,
}

impl GuestMsrState {
    pub fn new() -> Self {
        Self {
            pat: 0x0007_0406_0007_0406, // reset default
            tsc_aux: 0,
            syscall: SyscallMsrs::default(),
        }
    }
}

impl Default for GuestMsrState {
    fn default() -> Self {
        Self::new()
    }
}
