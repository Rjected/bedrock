// SPDX-License-Identifier: GPL-2.0

//! Local APIC (Advanced Programmable Interrupt Controller) emulation.
//!
//! This module provides the state for emulating the Local APIC, which is accessed
//! by the guest at physical address 0xFEE00000-0xFEE01000. All registers are
//! 32-bit aligned at 16-byte boundaries per Intel SDM Vol 3A, Table 12-1.

#[cfg(not(feature = "cargo"))]
use super::super::exit_record::{StateHash, Xxh64Hasher};
#[cfg(feature = "cargo")]
use crate::exit_record::{StateHash, Xxh64Hasher};

use super::random::{nonzero_seed, xorshift64};

/// Default IA32_APIC_BASE value: APIC enabled, BSP, base at 0xFEE00000.
pub const APIC_BASE_DEFAULT: u64 = 0xFEE0_0900;

/// Local APIC register state for software emulation.
///
/// This struct holds the state of the emulated Local APIC, which is accessed
/// by the guest at physical address 0xFEE00000-0xFEE01000. All registers are
/// 32-bit aligned at 16-byte boundaries per Intel SDM Vol 3A, Table 12-1.
#[derive(Clone)]
pub struct ApicState {
    /// IA32_APIC_BASE MSR (0x1B) - APIC base address and enable bits.
    /// Bit 8: BSP flag, Bit 10: x2APIC enable, Bit 11: APIC enable.
    pub base: u64,
    /// APIC ID (offset 0x020) - bits 31:24 contain the ID
    pub id: u32,
    /// Version register (offset 0x030)
    pub version: u32,
    /// Task Priority Register (offset 0x080)
    pub tpr: u32,
    /// Logical Destination Register (offset 0x0D0)
    pub ldr: u32,
    /// Destination Format Register (offset 0x0E0)
    pub dfr: u32,
    /// Spurious Interrupt Vector Register (offset 0x0F0)
    pub svr: u32,
    /// In-Service Register (offsets 0x100-0x170, 8 x 32-bit)
    pub isr: [u32; 8],
    /// Trigger Mode Register (offsets 0x180-0x1F0, 8 x 32-bit)
    pub tmr: [u32; 8],
    /// Interrupt Request Register (offsets 0x200-0x270, 8 x 32-bit)
    pub irr: [u32; 8],
    /// Error Status Register (offset 0x280)
    pub esr: u32,
    /// Interrupt Command Register low (offset 0x300)
    pub icr_lo: u32,
    /// Interrupt Command Register high (offset 0x310)
    pub icr_hi: u32,
    /// LVT Timer Register (offset 0x320)
    pub lvt_timer: u32,
    /// LVT Thermal Sensor Register (offset 0x330)
    pub lvt_thermal: u32,
    /// LVT Performance Monitoring Register (offset 0x340)
    pub lvt_perf: u32,
    /// LVT LINT0 Register (offset 0x350)
    pub lvt_lint0: u32,
    /// LVT LINT1 Register (offset 0x360)
    pub lvt_lint1: u32,
    /// LVT Error Register (offset 0x370)
    pub lvt_error: u32,
    /// Timer Initial Count Register (offset 0x380)
    pub timer_initial: u32,
    /// Timer Divide Configuration Register (offset 0x3E0)
    pub timer_divide: u32,
    /// TSC value when timer should fire (0 = timer not running).
    /// This is internal state, not a real APIC register.
    pub timer_deadline: u64,
    /// Instructions between deterministic forced preemptions (0 = disabled).
    ///
    /// Not an APIC register. Drives instruction-granular preemption: the LVT
    /// timer vector is raised once roughly `preempt_period` guest instructions
    /// have retired since the last forced preemption, handing the guest
    /// scheduler a preemption point it would not otherwise have (only its
    /// natural tick/syscall/yield entries). See `check_preempt`.
    pub preempt_period: u64,
    /// xorshift64 state for per-interval preemption jitter. A dedicated stream,
    /// separate from the RDRAND PRNG, so forced preemptions never perturb the
    /// randomness the guest observes. Never 0 while enabled.
    pub preempt_rng: u64,
    /// Emulated TSC of the next forced preemption (0 = not yet armed). Shifted
    /// along with `tsc_offset` when HLT/MWAIT skips idle time, so it counts
    /// retired guest instructions only.
    pub preempt_deadline: u64,
}

impl Default for ApicState {
    fn default() -> Self {
        Self {
            base: APIC_BASE_DEFAULT,
            id: 0,
            // Version: bits 7:0 = version (0x14), bits 23:16 = max LVT entry (5)
            version: 0x0005_0014,
            tpr: 0,
            ldr: 0,
            dfr: 0xFFFF_FFFF,
            // SVR: APIC disabled (bit 8 = 0), vector = 0xFF
            svr: 0x0000_00FF,
            isr: [0; 8],
            tmr: [0; 8],
            irr: [0; 8],
            esr: 0,
            icr_lo: 0,
            icr_hi: 0,
            // LVT registers: masked (bit 16 = 1)
            lvt_timer: 0x0001_0000,
            lvt_thermal: 0x0001_0000,
            lvt_perf: 0x0001_0000,
            lvt_lint0: 0x0001_0000,
            lvt_lint1: 0x0001_0000,
            lvt_error: 0x0001_0000,
            timer_initial: 0,
            timer_divide: 0,
            timer_deadline: 0,
            preempt_period: 0,
            preempt_rng: 0,
            preempt_deadline: 0,
        }
    }
}

impl ApicState {
    /// Configure deterministic preemption: raise the LVT timer vector after a
    /// number of retired instructions drawn from `[period, 2*period)`, the gap
    /// re-drawn after each one from the VM's xorshift64 PRNG
    /// ([`xorshift64`]) on its own stream seeded by `seed`, separate from the
    /// RDRAND stream so preemptions never shift guest-visible randomness.
    /// `period == 0` disables it. The first deadline is
    /// armed lazily on the next injection pass (`check_preempt`), so this
    /// needs no knowledge of the current emulated TSC.
    pub fn configure_preempt(&mut self, period: u64, seed: u64) {
        self.preempt_period = period;
        self.preempt_rng = if period == 0 { 0 } else { nonzero_seed(seed) };
        self.preempt_deadline = 0;
    }

    /// Advance the jitter stream and return the gap, in retired instructions,
    /// to the next forced preemption: `[period, 2*period)`. Returns 0 when
    /// preemption is disabled.
    pub fn next_preempt_interval(&mut self) -> u64 {
        if self.preempt_period == 0 {
            return 0;
        }
        let x = xorshift64(&mut self.preempt_rng);
        self.preempt_period.saturating_add(x % self.preempt_period)
    }
}

impl StateHash for ApicState {
    fn state_hash(&self) -> u64 {
        let mut h = Xxh64Hasher::new();
        h.write_u64(self.base);
        h.write_u32(self.id);
        h.write_u32(self.version);
        h.write_u32(self.tpr);
        h.write_u32(self.ldr);
        h.write_u32(self.dfr);
        h.write_u32(self.svr);
        for &val in &self.isr {
            h.write_u32(val);
        }
        for &val in &self.tmr {
            h.write_u32(val);
        }
        for &val in &self.irr {
            h.write_u32(val);
        }
        h.write_u32(self.esr);
        h.write_u32(self.icr_lo);
        h.write_u32(self.icr_hi);
        h.write_u32(self.lvt_timer);
        h.write_u32(self.lvt_thermal);
        h.write_u32(self.lvt_perf);
        h.write_u32(self.lvt_lint0);
        h.write_u32(self.lvt_lint1);
        h.write_u32(self.lvt_error);
        h.write_u32(self.timer_initial);
        h.write_u32(self.timer_divide);
        h.write_u64(self.timer_deadline);
        h.write_u64(self.preempt_period);
        h.write_u64(self.preempt_rng);
        h.write_u64(self.preempt_deadline);
        h.finish()
    }
}
