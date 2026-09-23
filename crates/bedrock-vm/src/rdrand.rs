// SPDX-License-Identifier: GPL-2.0

//! RDRAND emulation configuration types: a seeded (non-cryptographic) PRNG, or
//! exit to userspace for each value.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RdrandMode {
    /// Use a seeded PRNG (xorshift64).
    SeededRng = 0,
    /// Userspace provides each value.
    ExitToUserspace = 1,
}

impl TryFrom<u32> for RdrandMode {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(RdrandMode::SeededRng),
            1 => Ok(RdrandMode::ExitToUserspace),
            _ => Err(()),
        }
    }
}

/// SET_RDRAND_CONFIG ioctl payload.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RdrandConfig {
    /// [`RdrandMode`] as u32.
    pub mode: u32,
    pub _reserved: u32,
    /// PRNG seed (ignored for ExitToUserspace).
    pub value: u64,
}

impl RdrandConfig {
    pub fn seeded_rng(seed: u64) -> Self {
        Self {
            mode: RdrandMode::SeededRng as u32,
            _reserved: 0,
            value: seed,
        }
    }

    pub fn exit_to_userspace() -> Self {
        Self {
            mode: RdrandMode::ExitToUserspace as u32,
            _reserved: 0,
            value: 0,
        }
    }
}

impl Default for RdrandConfig {
    fn default() -> Self {
        Self::seeded_rng(0x12345678_deadbeef)
    }
}

/// The trapping RDRAND in ExitToUserspace mode.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RdrandExitInfo {
    /// Destination register index (0=RAX, 1=RCX, 2=RDX, 3=RBX, 4=RSP, 5=RBP, 6=RSI, 7=RDI, 8-15=R8-R15).
    pub dest_reg: u8,
    /// Operand size: 0=16-bit, 1=32-bit, 2=64-bit.
    pub operand_size: u8,
    pub _reserved: [u8; 6],
}

/// SET_RDRAND_VALUE payload, set before resuming.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RdrandValue {
    pub value: u64,
}
