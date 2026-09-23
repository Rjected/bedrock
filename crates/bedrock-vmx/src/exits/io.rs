// SPDX-License-Identifier: GPL-2.0

//! I/O instruction exit handler (serial port, CMOS RTC, PCI config space).

use super::helpers::{advance_rip, ExitHandlerResult};
use super::interrupts::ioapic_deliver_irq;
use super::qualifications::{IoDirection, IoQualification};
use super::reasons::ExitReason;

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// Handle I/O instruction exit.
pub fn handle_io<C: VmContext>(ctx: &mut C, qual: IoQualification) -> ExitHandlerResult {
    if qual.string {
        // String I/O not implemented.
        return ExitHandlerResult::ExitToUserspace(ExitReason::IoInstruction);
    }

    let size = qual.size as usize;

    // LCR.DLAB: ports 0x3F8/0x3F9 access the divisor latch when set.
    let dlab = (ctx.state().devices.serial.lcr & 0x80) != 0;

    match qual.direction {
        IoDirection::Out => {
            let value = ctx.state().gprs.rax as u32 & ((1u64 << (size * 8)) - 1) as u32;
            let byte = (value & 0xFF) as u8;

            match qual.port {
                0x3F8 => {
                    if dlab {
                        // Divisor Latch Low
                        ctx.state_mut().devices.serial.dll = byte;
                    } else {
                        // Transmit (earlyprintk per-byte path): the line
                        // accumulator emits one `Serial` event per line. A full
                        // event buffer is handled by the dispatcher.
                        let _ = ctx.state_mut().event_serial_byte(byte);
                    }
                }
                0x3F9 => {
                    if dlab {
                        // Divisor Latch High
                        ctx.state_mut().devices.serial.dlh = byte;
                    } else {
                        // Interrupt Enable Register
                        ctx.state_mut().devices.serial.ier = byte & 0x0F;
                        // THRE interrupt enabled and TX is always empty: IRQ 4.
                        if (byte & 0x02) != 0 {
                            ioapic_deliver_irq(ctx, 4);
                        }
                    }
                }
                0x3FB => {
                    // Line Control Register
                    ctx.state_mut().devices.serial.lcr = byte;
                }
                0x3FC => {
                    // Modem Control Register
                    ctx.state_mut().devices.serial.mcr = byte & 0x1F;
                }
                0x3FF => {
                    // Scratch Register
                    ctx.state_mut().devices.serial.scr = byte;
                }
                0x3FA => {
                    // FCR: FIFOs not emulated.
                }
                0x80 => {
                    // POST diagnostic port
                }
                0xCF8 => {
                    // PCI configuration address
                }
                0xCFC..=0xCFF => {
                    // PCI configuration data
                }
                0x70 => {
                    // CMOS RTC index (bit 7 is NMI disable).
                    ctx.state_mut().devices.rtc.index = byte & 0x7F;
                }
                _ => {}
            }
        }
        IoDirection::In => {
            let value: u32 = match qual.port {
                0x3F8 => {
                    if dlab {
                        // Divisor Latch Low
                        u32::from(ctx.state().devices.serial.dll)
                    } else {
                        // RBR: no host->guest serial input.
                        0
                    }
                }
                0x3F9 => {
                    if dlab {
                        // Divisor Latch High
                        u32::from(ctx.state().devices.serial.dlh)
                    } else {
                        // Interrupt Enable Register
                        u32::from(ctx.state().devices.serial.ier)
                    }
                }
                0x3FA => {
                    // IIR: FIFOs enabled (bits 7:6); THRE pending (0xC2) iff
                    // enabled, since TX is always empty; else none (0xC1).
                    let ier = ctx.state().devices.serial.ier;
                    if (ier & 0x02) != 0 {
                        0xC2
                    } else {
                        0xC1
                    }
                }
                0x3FB => {
                    // Line Control Register
                    u32::from(ctx.state().devices.serial.lcr)
                }
                0x3FC => {
                    // Modem Control Register
                    u32::from(ctx.state().devices.serial.mcr)
                }
                0x3FD => {
                    // LSR: THRE | TEMT; Data Ready never set.
                    0x60
                }
                0x3FE => {
                    // MSR: DCD | DSR | CTS, or tty open blocks.
                    0xB0
                }
                0x3FF => {
                    // Scratch Register
                    u32::from(ctx.state().devices.serial.scr)
                }
                0xCFC..=0xCFF => {
                    // PCI config data: no device.
                    0xFFFFFFFF >> ((4 - size) * 8)
                }
                0x60 => {
                    // Keyboard controller data
                    0
                }
                0x64 => {
                    // Keyboard controller status: 0xFF (absent) makes Linux
                    // i8042 probing fail fast instead of timing out.
                    0xFF
                }
                0x71 => {
                    // CMOS RTC data, derived from the emulated TSC.
                    let emulated_tsc = ctx.state().emulated_tsc;
                    let tsc_frequency = ctx.state().tsc_frequency;
                    u32::from(
                        ctx.state()
                            .devices
                            .rtc
                            .read_register_with_tsc(emulated_tsc, tsc_frequency),
                    )
                }
                _ => 0,
            };

            let mask = (1u64 << (size * 8)) - 1;
            let gprs = &mut ctx.state_mut().gprs;
            gprs.rax = (gprs.rax & !mask) | (u64::from(value) & mask);
        }
    }

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}
