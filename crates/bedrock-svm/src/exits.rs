// SPDX-License-Identifier: GPL-2.0

//! Convert SVM exits to the existing Bedrock exit-handler ABI. Unknown exits
//! fail explicitly instead of being mistaken for a successful guest step.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub reason: u32,
    pub qualification: u64,
    pub interruption_info: u32,
    pub interruption_error: u32,
    pub guest_physical_address: u64,
    pub instruction_len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    UnknownExit(u64),
    InvalidIoSize(u64),
    MissingCrDecode,
}

/// GuestOnly retired-instruction accounting includes VMRUN and the physical
/// NMI transition. Intercepted ordinary IRQs/faults do not retire a guest
/// instruction. REP iteration accounting must use RCX instead of this event.
pub fn retired_instructions(before: u64, after: u64, code: u64) -> Option<u64> {
    after
        .checked_sub(before)?
        .checked_sub(1 + u64::from(code == 0x61))
}

pub fn decode(
    code: u64,
    info1: u64,
    info2: u64,
    rip: u64,
    next_rip: u64,
    hypervisor_step: bool,
) -> Result<Exit, DecodeError> {
    let mut e = Exit {
        reason: 0,
        qualification: 0,
        interruption_info: 0,
        interruption_error: 0,
        guest_physical_address: 0,
        instruction_len: 0,
    };
    let default_len = match code {
        0x000..=0x01f => {
            if info1 & (1 << 63) == 0 {
                return Err(DecodeError::MissingCrDecode);
            }
            e.reason = 28;
            e.qualification = (code & 0xf) | ((u64::from(code < 0x10)) << 4) | ((info1 & 0xf) << 8);
            3
        }
        0x041 if hypervisor_step => {
            e.reason = 37;
            0
        }
        // SVM reports trapping INT3/INTO at the instruction's RIP. Treat
        // them as software interrupts so the saved return RIP advances.
        0x043 | 0x044 => {
            e.reason = 514;
            1
        }
        0x040..=0x05f => {
            let vector = (code - 0x40) as u32;
            e.reason = 0;
            e.interruption_info = (1 << 31) | (3 << 8) | vector;
            if matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30) {
                e.interruption_info |= 1 << 11;
                e.interruption_error = info1 as u32;
            }
            if vector == 14 {
                e.qualification = info2;
            }
            0
        }
        0x060 => {
            e.reason = 1;
            0
        }
        0x061 => {
            e.reason = 0;
            e.interruption_info = (1 << 31) | (2 << 8) | 2;
            0
        }
        0x064 => {
            e.reason = 7;
            0
        }
        0x06e => {
            e.reason = 16;
            2
        }
        0x06f | 0x08e => {
            e.reason = 15;
            2
        }
        0x070 | 0x071 => {
            e.reason = if code == 0x070 { 512 } else { 513 };
            1
        }
        0x075 => {
            e.reason = 514;
            0
        }
        0x072 => {
            e.reason = 10;
            2
        }
        0x076 | 0x089 => {
            e.reason = if code == 0x076 { 13 } else { 54 };
            2
        }
        0x078 => {
            e.reason = 12;
            1
        }
        0x07b => {
            e.reason = 30;
            let size = match (info1 >> 4) & 7 {
                1 => 0,
                2 => 1,
                4 => 3,
                _ => return Err(DecodeError::InvalidIoSize(info1)),
            };
            e.qualification = size
                | ((info1 & 1) << 3)
                | ((info1 & 4) << 2)
                | ((info1 & 8) << 2)
                | (info1 & 0xffff_0000);
            // IOIO's EXITINFO2 supplies the next RIP even without NRIPS.
            if let Some(len @ 1..=15) = info2.checked_sub(rip) {
                e.instruction_len = len as u32;
            }
            0
        }
        0x07c => {
            e.reason = if info1 & 1 == 0 { 31 } else { 32 };
            2
        }
        0x07f => {
            e.reason = 2;
            0
        }
        0x080 | 0x082..=0x086 => {
            e.reason = 27;
            3
        }
        0x081 => {
            e.reason = 18;
            3
        }
        0x087 => {
            e.reason = 51;
            3
        }
        0x08a => {
            e.reason = 39;
            3
        }
        0x08b | 0x08c => {
            e.reason = 36;
            3
        }
        0x08d => {
            e.reason = 55;
            3
        }
        0x400 => {
            e.reason = 48;
            e.guest_physical_address = info2;
            let write = info1 & 2 != 0;
            let execute = info1 & 16 != 0;
            let present = info1 & 1 != 0;
            e.qualification = u64::from(!write && !execute)
                | (u64::from(write) << 1)
                | (u64::from(execute) << 2)
                | (u64::from(present) << 3)
                | (u64::from(present && !write) << 4)
                | (u64::from(present && !execute) << 5);
            0
        }
        u64::MAX => {
            e.reason = 33;
            0
        }
        _ => return Err(DecodeError::UnknownExit(code)),
    };
    if e.instruction_len == 0 {
        e.instruction_len = next_rip
            .checked_sub(rip)
            .filter(|len| (1..=15).contains(len))
            .map_or(default_len, |len| len as u32);
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_invalidation_exits_match_shared_reasons() {
        for (code, reason) in [(0x76, 13), (0x89, 54)] {
            let exit = decode(code, 0, 0, 0x1000, 0x1002, false).unwrap();
            assert_eq!(exit.reason, reason);
            assert_eq!(exit.instruction_len, 2);
        }
    }

    #[test]
    fn retired_counter_removes_entry_and_physical_nmi_ticks() {
        for code in [0x41, 0x60, 0x400, 0x4e, 0x72, 0x81] {
            assert_eq!(retired_instructions(100, 111, code), Some(10));
            assert_eq!(retired_instructions(100, 101, code), Some(0));
        }
        assert_eq!(retired_instructions(100, 112, 0x61), Some(10));
        assert_eq!(retired_instructions(100, 102, 0x61), Some(0));
        assert_eq!(retired_instructions(100, 101, 0x61), None);
        assert_eq!(retired_instructions(100, 99, 0x41), None);
    }
    #[test]
    fn io_decode_preserves_direction_size_port_and_string_flags() {
        let e = decode(
            0x7b,
            (0x3f8 << 16) | (4 << 4) | 1 | 4 | 8,
            0x102,
            0x100,
            0,
            false,
        )
        .unwrap();
        assert_eq!(e.reason, 30);
        assert_eq!(e.qualification, (0x3f8 << 16) | 3 | 8 | 16 | 32);
        assert_eq!(e.instruction_len, 2);
        assert!(decode(0x7b, 0, 0, 0, 0, false).is_err());
    }
    #[test]
    fn cow_fault_is_a_write_to_a_readable_nonwritable_page() {
        let e = decode(0x400, 3, 0x123000, 0x100, 0, false).unwrap();
        assert_eq!(e.reason, 48);
        assert_ne!(e.qualification & 2, 0);
        assert_ne!(e.qualification & 8, 0);
        assert_eq!(e.qualification & 16, 0);
        assert_eq!(e.guest_physical_address, 0x123000);
        assert_eq!(e.instruction_len, 0); // retry faulting instruction
    }
    #[test]
    fn guest_debug_exception_is_distinct_from_hypervisor_step() {
        let guest = decode(0x41, 0, 0, 0x100, 0, false).unwrap();
        assert_eq!(guest.reason, 0);
        assert_eq!(guest.interruption_info & 0xff, 1);
        assert_eq!(decode(0x41, 0, 0, 0x100, 0, true).unwrap().reason, 37);
    }
    #[test]
    fn cr_decode_and_unknown_exits_fail_closed() {
        let e = decode(0x13, (1 << 63) | 9, 0, 0x100, 0x104, false).unwrap();
        assert_eq!(e.qualification, 3 | (9 << 8));
        assert_eq!(e.instruction_len, 4);
        assert_eq!(
            decode(0x13, 9, 0, 0, 0, false),
            Err(DecodeError::MissingCrDecode)
        );
        assert_eq!(
            decode(0xdead, 0, 0, 0, 0, false),
            Err(DecodeError::UnknownExit(0xdead))
        );
    }
}
