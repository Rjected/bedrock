// =============================================================================
// Control Registers
// See Intel SDM Vol 3A, Section 2.5 - Control Registers
// =============================================================================

/// CR0 - system control flags (operating mode and states).
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Cr0(u64);

impl Cr0 {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn bits(&self) -> u64 {
        self.0
    }
}

/// CR2 - page-fault linear address.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Cr2(pub u64);

impl Cr2 {
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}

/// CR3 - paging-structure hierarchy base address and flags.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Cr3(u64);

impl Cr3 {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn bits(&self) -> u64 {
        self.0
    }
}

/// CR4 - architectural extension enable flags.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Cr4(u64);

impl Cr4 {
    /// VMX Enable - enables VMX operation.
    pub const VMXE: u64 = 1 << 13;

    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn bits(&self) -> u64 {
        self.0
    }

    pub fn set(&mut self, flag: u64) {
        self.0 |= flag;
    }

    pub fn clear(&mut self, flag: u64) {
        self.0 &= !flag;
    }
}

/// CR8 - access to local APIC TPR bits 7:4 (64-bit mode only; bits 3:0 used).
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Cr8(u64);

impl Cr8 {
    /// Mask for valid TPR bits.
    const TPR_MASK: u64 = 0xF;

    pub fn new(value: u64) -> Self {
        Self(value & Self::TPR_MASK)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ControlRegisters {
    pub cr0: Cr0,
    pub cr2: Cr2,
    pub cr3: Cr3,
    pub cr4: Cr4,
    pub cr8: Cr8,
}

// =============================================================================
// Control Register Access
// See Intel SDM Vol 3A, Section 2.5 - Control Registers
// See Intel SDM Vol 2B, MOV—Move to/from Control Registers
// =============================================================================

/// Error returned by control register read/write operations (SDM Vol 2B MOV CR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrError {
    /// Invalid control register (CR1, CR5-CR7, CR9-CR15). #UD.
    InvalidRegister,
    /// Requires CPL 0. #GP(0).
    PrivilegeViolation,
    /// Invalid CR0 combination (PG=1 with PE=0, or CD=0 with NW=1). #GP(0).
    InvalidCr0Combination,
    /// Reserved bits set in CR0[63:32]. #GP(0).
    Cr0ReservedBits,
    /// Reserved bits set in CR4. #GP(0).
    Cr4ReservedBits,
    /// Reserved bits set in CR8[63:4]. #GP(0).
    Cr8ReservedBits,
    /// Reserved bits set in CR3[63:MAXPHYADDR]. #GP(0).
    Cr3ReservedBits,
    /// CR8 access outside 64-bit mode.
    Cr8NotAvailable,
    /// CR4.PCIDE 0->1 while CR3[11:0] != 0. #GP(0).
    PcidEnableWithNonZeroCr3,
    /// Clearing CR0.PG in 64-bit mode. #GP(0).
    CannotDisablePaging64Bit,
    /// Clearing CR4.PAE in IA-32e mode. #GP(0).
    CannotDisablePae,
    /// Platform-specific error.
    PlatformError(u32),
}

/// Result type for control register operations.
pub type CrResult<T> = Result<T, CrError>;

/// Read/write access to control registers via MOV CRn (0F 20 / 0F 22).
///
/// Requires CPL 0; invalid registers #UD, invalid values #GP(0).
/// See Intel SDM Vol 3A §2.5 and Vol 2B (MOV CR).
///
/// # Example Implementation
///
/// For direct hardware access (unsafe, requires ring 0):
/// ```ignore
/// unsafe fn read_cr0() -> u64 {
///     let value: u64;
///     core::arch::asm!(
///         "mov {}, cr0",
///         out(reg) value,
///         options(nomem, nostack)
///     );
///     value
/// }
///
/// unsafe fn write_cr0(value: u64) {
///     core::arch::asm!(
///         "mov cr0, {}",
///         in(reg) value,
///         options(nomem, nostack)
///     );
/// }
/// ```
pub trait CrAccess {
    fn read_cr0(&self) -> CrResult<Cr0>;

    fn read_cr3(&self) -> CrResult<Cr3>;

    fn read_cr4(&self) -> CrResult<Cr4>;

    fn write_cr4(&self, value: &Cr4) -> CrResult<()>;

    /// Set CR4.VMXE (bit 13).
    ///
    /// Must also update the kernel's CR4 shadow (cpu_tlbstate.cr4); a raw MOV
    /// would desync it and the kernel would later drop VMXE on context switch.
    fn set_vmxe(&self) -> CrResult<()>;

    /// Clear CR4.VMXE. Only after VMXOFF; must also update the CR4 shadow.
    fn clear_vmxe(&self) -> CrResult<()>;
}
