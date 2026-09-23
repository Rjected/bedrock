// SPDX-License-Identifier: GPL-2.0

//! Aggregate guest register state.

use super::{
    ControlRegisters, DebugRegisters, DescriptorTableRegisters, ExtendedControlRegisters,
    GeneralPurposeRegisters, SegmentRegisters,
};

/// Complete guest register state, used by `VmContext` register methods.
///
/// Same layout as the userspace `Regs` and kernel `BedrockRegs` ioctl structs.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GuestRegisters {
    pub gprs: GeneralPurposeRegisters,
    pub control_regs: ControlRegisters,
    pub debug_regs: DebugRegisters,
    pub segment_regs: SegmentRegisters,
    pub descriptor_tables: DescriptorTableRegisters,
    /// EFER.
    pub extended_control_regs: ExtendedControlRegisters,
    pub rip: u64,
    pub rflags: u64,
}
