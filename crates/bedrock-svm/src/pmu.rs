// SPDX-License-Identifier: GPL-2.0

//! Guest counter autoswap (AMD APM volume 2, sections 15.39 and 15.21.10).

use super::vmcb::{offset as o, Vmcb};

/// Temporary state for an entry with no guest event awaiting delivery.
pub struct PmcEntry {
    int_control: u64,
    misc: u64,
    virt_ext: u64,
    idt_limit: u64,
    entry_rip: u64,
    entry_rf: bool,
    period: u64,
}

impl PmcEntry {
    /// Native PUSHF observes logical flags only when neither stepping TF nor
    /// debug-resume RF is active. The caller must guard code/table writes.
    /// finish restores the original PUSHF intercept on every counted exit.
    pub fn allow_native_pushf(&self, v: &mut Vmcb) -> bool {
        if v.read(o::RFLAGS, 8) & ((1 << 8) | (1 << 16)) != 0 {
            return false;
        }
        v.intercept(112, false);
        true
    }

    /// The caller must check PMC virtualization, VNMI, and enabled IRPERF.
    /// Hardware swaps all six programmable counters and IRPERF, keeping host
    /// perf events separate from the guest's overflow counter and clock.
    pub fn prepare(v: &mut Vmcb, period: u64) -> Option<Self> {
        if !(1..=1 << 30).contains(&period)
            || v.read(o::EVENT_INJECTION, 4) & (1 << 31) != 0
            || (v.read(o::INT_CONTROL, 4) & (1 << 8) != 0
                && v.read(o::INTERCEPT_MISC1, 4) & (1 << 4) == 0)
        {
            return None;
        }
        let saved = Self {
            int_control: v.read(o::INT_CONTROL, 4),
            misc: v.read(o::INTERCEPT_MISC1, 4),
            virt_ext: v.read(o::VIRT_EXT, 8),
            idt_limit: v.read(o::IDTR + 4, 4),
            entry_rip: v.read(o::RIP, 8),
            entry_rf: v.read(o::RFLAGS, 8) & (1 << 16) != 0,
            period,
        };
        v.write(o::VIRT_EXT, 8, saved.virt_ext | (1 << 3));
        v.write(
            o::INT_CONTROL,
            4,
            (saved.int_control & !((1 << 11) | (1 << 12))) | (1 << 26),
        );
        // Virtual PMC NMIs bypass the physical NMI intercept. Trap delivery
        // with #GP at the IDT limit check, before an interrupt frame is written.
        // SIDT/LIDT must stop the batch before observing/changing this shadow.
        v.write(o::IDTR + 4, 4, 0);
        // Stop before IRET: its retirement is not reliably counted inside a
        // native region on the validation host. Replay it with exact stepping.
        v.write(
            o::INTERCEPT_MISC1,
            4,
            saved.misc | (1 << 6) | (1 << 10) | (1 << 20),
        );
        for i in 0..6 {
            v.write(o::PERF_CTL0 + i * 16, 8, 0);
            v.write(o::PERF_CTR0 + i * 16, 8, 0);
        }
        // Retired instructions, all guest privilege levels, enable + interrupt.
        v.write(o::PERF_CTL0, 8, 0x5300c0);
        v.write(o::PERF_CTR0, 8, 0u64.wrapping_sub(period) & ((1 << 48) - 1));
        v.write(o::INSTR_RETIRED_CTR, 8, 0);
        v.write(o::PERF_GLOBAL_STATUS, 8, 0);
        v.write(o::PERF_GLOBAL_CONTROL, 8, 1);
        Some(saved)
    }

    /// Restore guest-visible controls before the exit handler observes state.
    /// IRPERF in the VMCB includes one VMRUN tick after entry, excluding host
    /// instructions and interrupt-delivery ticks. An immediate execution
    /// breakpoint can stop entry before that tick. The programmable event only
    /// drives PMI.
    pub fn finish(self, v: &mut Vmcb) -> Option<u64> {
        let raw = v.read(o::INSTR_RETIRED_CTR, 8);
        let code = v.read(o::EXIT_CODE, 8);
        // An ordinary exit can occasionally save IRPERF one tick behind the
        // programmable retired-instruction counter. The latter has the same
        // VMRUN tick on this host. Correct only a single-tick disagreement
        // before overflow; after overflow the counter may be reloaded and
        // cannot independently establish the number of guest retirements.
        let pmc_start = 0u64.wrapping_sub(self.period) & ((1 << 48) - 1);
        let pmc_elapsed = v.read(o::PERF_CTR0, 8).wrapping_sub(pmc_start) & ((1 << 48) - 1);
        let raw = if v.read(o::PERF_GLOBAL_STATUS, 8) & 1 == 0
            && pmc_elapsed < self.period
            && pmc_elapsed == raw.saturating_add(1)
        {
            raw.saturating_add(1)
        } else {
            raw
        };
        let overflow_nmi = code == 0x4d
            && v.read(o::EXIT_INT_INFO, 4) & 0x800007ff == 0x80000202
            && v.read(o::PERF_GLOBAL_STATUS, 8) & 1 != 0;
        let immediate_breakpoint = raw == 0
            && !self.entry_rf
            && code == 0x41
            && v.read(o::DR6, 8) & 15 != 0
            && v.read(o::RIP, 8) == self.entry_rip;
        let immediate_interrupt = raw == 0
            && (matches!(code, 0x60 | 0x61 | 0x64) || overflow_nmi)
            && v.read(o::RIP, 8) == self.entry_rip;
        let count = if immediate_breakpoint || immediate_interrupt {
            Some(0)
        } else {
            raw.checked_sub(1)
        };
        v.write(o::VIRT_EXT, 8, self.virt_ext);
        v.write(o::INT_CONTROL, 4, self.int_control);
        v.write(o::IDTR + 4, 4, self.idt_limit);
        v.write(o::INTERCEPT_MISC1, 4, self.misc);
        if overflow_nmi {
            // A host boundary, with no guest #GP/NMI to deliver or replay.
            v.write(o::EXIT_CODE, 8, 0x60);
            v.write(o::EXIT_INT_INFO, 4, 0);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_pushf_requires_logical_flags_and_restores_intercepts_on_failure() {
        for flags in [2, 2 | (1 << 8), 2 | (1 << 16)] {
            let mut v = Vmcb::new();
            v.initialize();
            v.write(o::RFLAGS, 8, flags);
            let misc = v.read(o::INTERCEPT_MISC1, 4);
            let entry = PmcEntry::prepare(&mut v, 100).unwrap();
            assert_eq!(entry.allow_native_pushf(&mut v), flags == 2);
            assert_eq!(v.read(o::INTERCEPT_MISC1, 4) & (1 << 16) != 0, flags != 2);
            assert_ne!(v.read(o::INTERCEPT_MISC1, 4) & (1 << 17), 0); // POPF stays intercepted.
            v.write(o::INSTR_RETIRED_CTR, 8, 0);
            v.write(o::EXIT_CODE, 8, 0x400);
            assert_eq!(entry.finish(&mut v), None);
            assert_eq!(v.read(o::INTERCEPT_MISC1, 4), misc);
            assert_eq!(v.read(o::RFLAGS, 8), flags);
        }
    }

    #[test]
    fn rejects_pending_guest_events_before_mutating_state() {
        let mut v = Vmcb::new();
        v.initialize();
        v.write(o::IDTR + 4, 4, 4095);
        v.write(o::EVENT_INJECTION, 4, 0x80000020);
        assert!(PmcEntry::prepare(&mut v, 100).is_none());
        v.write(o::EVENT_INJECTION, 4, 0);
        v.write(o::INT_CONTROL, 4, (1 << 24) | (1 << 8));
        assert!(PmcEntry::prepare(&mut v, 100).is_none());
        assert_eq!(v.read(o::IDTR + 4, 4), 4095);
        assert_eq!(v.read(o::VIRT_EXT, 8), 0);
    }

    #[test]
    fn recognizes_only_a_counter_overflow_during_virtual_nmi_delivery() {
        for (code, info, status, boundary) in [
            (0x4d, 0x80000202, 1, true),
            (0x4d, 0, 1, false),
            (0x4d, 0x80000202, 0, false),
            (0x4d, 0x8000030d, 1, false),
            (0x400, 0x80000202, 1, false),
            (0x61, 0, 0, false),
        ] {
            let mut v = Vmcb::new();
            v.initialize();
            v.write(o::IDTR + 4, 4, 4095);
            v.write(o::IDTR + 8, 8, 0xffff800000001000);
            let misc = v.read(o::INTERCEPT_MISC1, 4);
            let entry = PmcEntry::prepare(&mut v, 100).unwrap();
            assert_eq!(v.read(o::PERF_CTR0, 8), (1 << 48) - 100);
            assert_ne!(v.read(o::INTERCEPT_MISC1, 4) & (1 << 20), 0);
            assert_eq!(v.read(o::IDTR + 4, 4), 0);
            v.write(o::INSTR_RETIRED_CTR, 8, 114);
            v.write(o::EXIT_CODE, 8, code);
            v.write(o::EXIT_INT_INFO, 4, info);
            v.write(o::PERF_GLOBAL_STATUS, 8, status);
            assert_eq!(entry.finish(&mut v), Some(113));
            assert_eq!(v.read(o::EXIT_CODE, 8), if boundary { 0x60 } else { code });
            assert_eq!(v.read(o::EXIT_INT_INFO, 4), if boundary { 0 } else { info });
            assert_eq!(v.read(o::IDTR + 4, 4), 4095);
            assert_eq!(v.read(o::IDTR + 8, 8), 0xffff800000001000);
            assert_eq!(v.read(o::INTERCEPT_MISC1, 4), misc);
            assert_eq!(v.read(o::INT_CONTROL, 4), 1 << 24);
            assert_eq!(v.read(o::VIRT_EXT, 8), 0);
        }
    }

    #[test]
    fn rejects_missing_entry_tick_after_restoring_controls() {
        let mut v = Vmcb::new();
        v.initialize();
        v.write(o::IDTR + 4, 4, 65535);
        let entry = PmcEntry::prepare(&mut v, 1 << 30).unwrap();
        assert_eq!(entry.finish(&mut v), None);
        assert_eq!(v.read(o::IDTR + 4, 4), 65535);
    }

    #[test]
    fn permits_an_intercepted_interrupt_window_without_delivering_it() {
        let mut v = Vmcb::new();
        v.initialize();
        v.intercept(100, true);
        v.write(o::INT_CONTROL, 4, (1 << 24) | (1 << 8) | (15 << 16));
        let entry = PmcEntry::prepare(&mut v, 100).unwrap();
        v.write(o::INSTR_RETIRED_CTR, 8, 1);
        v.write(o::EXIT_CODE, 8, 0x64);
        assert_eq!(entry.finish(&mut v), Some(0));
        assert_ne!(v.read(o::INTERCEPT_MISC1, 4) & (1 << 4), 0);
        assert_ne!(v.read(o::INT_CONTROL, 4) & (1 << 8), 0);
    }

    #[test]
    fn accepts_a_zero_work_breakpoint_only_without_resume_flag_or_rip_progress() {
        for (flags, rip, dr6, expected) in [
            (2, 0x1000, 1, Some(0)),
            (2 | (1 << 16), 0x1000, 1, None),
            (2, 0x1001, 1, None),
            (2, 0x1000, 0x4000, None),
        ] {
            let mut v = Vmcb::new();
            v.initialize();
            v.write(o::RIP, 8, 0x1000);
            v.write(o::RFLAGS, 8, flags);
            let entry = PmcEntry::prepare(&mut v, 100).unwrap();
            v.write(o::EXIT_CODE, 8, 0x41);
            v.write(o::RIP, 8, rip);
            v.write(o::DR6, 8, dr6);
            assert_eq!(entry.finish(&mut v), expected);
        }
    }

    #[test]
    fn accepts_zero_work_interrupts_without_accepting_rip_progress() {
        for code in [0x60, 0x61, 0x64] {
            for (rip, expected) in [(0x1000, Some(0)), (0x1001, None)] {
                let mut v = Vmcb::new();
                v.initialize();
                v.write(o::RIP, 8, 0x1000);
                let entry = PmcEntry::prepare(&mut v, 100).unwrap();
                v.write(o::EXIT_CODE, 8, code);
                v.write(o::RIP, 8, rip);
                assert_eq!(entry.finish(&mut v), expected);
            }
        }
    }

    #[test]
    fn cross_checks_a_missing_irperf_tick_only_before_overflow() {
        for (raw, pmc_ticks, status, expected) in [
            (16, 16, 0, Some(15)),
            (15, 16, 0, Some(15)),
            (15, 17, 0, Some(14)),
            (15, 16, 1, Some(14)),
        ] {
            let mut v = Vmcb::new();
            v.initialize();
            let entry = PmcEntry::prepare(&mut v, 1000).unwrap();
            let start = v.read(o::PERF_CTR0, 8);
            v.write(o::PERF_CTR0, 8, start + pmc_ticks);
            v.write(o::INSTR_RETIRED_CTR, 8, raw);
            v.write(o::PERF_GLOBAL_STATUS, 8, status);
            v.write(o::EXIT_CODE, 8, 0x72);
            assert_eq!(entry.finish(&mut v), expected);
        }
    }
}
