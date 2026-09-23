// SPDX-License-Identifier: GPL-2.0

//! Exit-record structure: the payload of an `EventKind::Exit` event.

/// Size of an exit record (`ExitRecord`) in bytes.
pub const EXIT_RECORD_SIZE: usize = 512;

/// Flag bit: entry represents a deterministic exit.
pub const EXIT_RECORD_FLAG_DETERMINISTIC: u32 = 1;

/// The payload of an `EventKind::Exit` event: a 512-byte snapshot of one VM
/// exit.
#[cfg_attr(
    feature = "cargo",
    derive(zerocopy::FromBytes, zerocopy::Immutable, zerocopy::KnownLayout)
)]
#[cfg_attr(feature = "cargo", derive(serde::Serialize, serde::Deserialize))]
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ExitRecord {
    // Exit info (24 bytes)
    /// TSC value at time of exit (from emulated_tsc).
    pub tsc: u64,
    /// Exit reason (ExitReason as u32).
    pub exit_reason: u32,
    /// Flags bitfield. Bit 0 = deterministic exit.
    pub flags: u32,
    /// Exit qualification value.
    pub exit_qualification: u64,

    // Guest registers (144 bytes)
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,

    // Device state hashes (56 bytes)
    /// Hash of APIC state.
    pub apic_hash: u64,
    /// Hash of serial port state.
    pub serial_hash: u64,
    /// Hash of I/O APIC state.
    pub ioapic_hash: u64,
    /// Hash of RTC state.
    pub rtc_hash: u64,
    /// Hash of MTRR state.
    pub mtrr_hash: u64,
    /// Hash of RDRAND state.
    pub rdrand_hash: u64,
    /// Hash of guest memory.
    pub memory_hash: u64,

    // Additional guest state (80 bytes)
    /// FS base address from VMCS.
    pub fs_base: u64,
    /// GS base address from VMCS.
    pub gs_base: u64,
    /// Kernel GS base (IA32_KERNEL_GS_BASE MSR).
    pub kernel_gs_base: u64,
    /// CR3 (page table root) from VMCS.
    pub cr3: u64,
    /// CS base address from VMCS.
    pub cs_base: u64,
    /// DS base address from VMCS.
    pub ds_base: u64,
    /// ES base address from VMCS.
    pub es_base: u64,
    /// SS base address from VMCS.
    pub ss_base: u64,
    /// Pending debug exceptions from VMCS.
    pub pending_dbg_exceptions: u64,
    /// Guest interruptibility state from VMCS.
    pub interruptibility_state: u32,
    /// Number of COW pages at time of exit.
    pub cow_page_count: u32,

    // The pebs_* fields below are non-zero only on EPT_VIOLATION_PEBS entries.
    /// Retired guest instructions past the PEBS firing target
    /// (`target_tsc - PEBS_MARGIN`). Usually 0 with PDist.
    pub pebs_skid: i64,
    /// Guest INST_RETIRED gain between arming and firing.
    pub pebs_inst_delta: i64,
    /// tsc_offset gain (HLT/MWAIT clamps) between arming and firing; should be 0.
    pub pebs_tsc_offset_delta: i64,
    /// Run-loop iterations the arming persisted across (0 = armed fresh).
    pub pebs_iters_since_arm: u32,
    /// Firing target minus TSC at arming time, in retired guest instructions.
    pub pebs_arm_delta: u64,

    // Determinism debugging fields (mirrored from bedrock-vm).
    /// `last_instruction_count` at exit time (fresh PMC0 read).
    pub last_instruction_count: u64,
    /// `apic.timer_deadline` at exit time. 0 if no timer pending.
    pub apic_timer_deadline: u64,
    /// `io_channel.request_target_tsc` at exit time.
    pub io_channel_target_tsc: u64,
    /// `pebs.armed_target_tsc` at exit time. 0 if PEBS not armed.
    pub pebs_armed_target_tsc: u64,
    /// Packed VMX state flags: bit 0 = mtf_enabled, bit 1 = last_exit_deterministic.
    pub vmx_state_flags: u64,

    /// Padding to reach 512 bytes.
    #[cfg_attr(feature = "cargo", serde(skip))]
    pub _padding: [u64; 16],
}

const _: () = assert!(core::mem::size_of::<ExitRecord>() == EXIT_RECORD_SIZE);

impl ExitRecord {
    pub const fn new() -> Self {
        Self {
            tsc: 0,
            exit_reason: 0,
            flags: 0,
            exit_qualification: 0,
            rax: 0,
            rcx: 0,
            rdx: 0,
            rbx: 0,
            rsp: 0,
            rbp: 0,
            rsi: 0,
            rdi: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags: 0,
            apic_hash: 0,
            serial_hash: 0,
            ioapic_hash: 0,
            rtc_hash: 0,
            mtrr_hash: 0,
            rdrand_hash: 0,
            memory_hash: 0,
            fs_base: 0,
            gs_base: 0,
            kernel_gs_base: 0,
            cr3: 0,
            cs_base: 0,
            ds_base: 0,
            es_base: 0,
            ss_base: 0,
            pending_dbg_exceptions: 0,
            interruptibility_state: 0,
            cow_page_count: 0,
            pebs_skid: 0,
            pebs_inst_delta: 0,
            pebs_tsc_offset_delta: 0,
            pebs_iters_since_arm: 0,
            pebs_arm_delta: 0,
            last_instruction_count: 0,
            apic_timer_deadline: 0,
            io_channel_target_tsc: 0,
            pebs_armed_target_tsc: 0,
            vmx_state_flags: 0,
            _padding: [0; 16],
        }
    }

    pub fn is_deterministic(&self) -> bool {
        self.flags & EXIT_RECORD_FLAG_DETERMINISTIC != 0
    }
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
