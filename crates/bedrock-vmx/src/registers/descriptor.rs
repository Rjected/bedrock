use super::segment::SegmentSelector;

/// GDTR. Layout matches SGDT/LGDT: 2-byte limit then 8-byte base (SDM 3A §2.4.1).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Gdtr {
    /// Number of bytes in the table (limit = size - 1).
    pub limit: u16,
    /// Linear address of byte 0 of the GDT.
    pub base: u64,
}

impl Gdtr {
    pub fn new(base: u64, limit: u16) -> Self {
        Self { base, limit }
    }
}

/// IDTR. Layout matches SIDT/LIDT: 2-byte limit then 8-byte base (SDM 3A §2.4.3).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Idtr {
    /// Number of bytes in the table (limit = size - 1).
    pub limit: u16,
    /// Linear address of byte 0 of the IDT.
    pub base: u64,
}

impl Idtr {
    pub fn new(base: u64, limit: u16) -> Self {
        Self { base, limit }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DescriptorTableRegisters {
    pub gdtr: Gdtr,
    pub idtr: Idtr,
}

/// Read access to segment selectors, GDTR/IDTR, and the TR base.
///
/// See Intel SDM Vol 2B (MOV Sreg, SGDT/SIDT/STR) and Vol 3A §3.4.
///
/// # Example Implementation
///
/// For direct hardware access (unsafe, requires ring 0 for some operations):
/// ```ignore
/// fn read_cs() -> u16 {
///     let sel: u16;
///     unsafe {
///         core::arch::asm!("mov {:x}, cs", out(reg) sel, options(nomem, nostack));
///     }
///     sel
/// }
///
/// fn read_gdtr() -> Gdtr {
///     let mut gdtr = Gdtr { limit: 0, base: 0 };
///     unsafe {
///         core::arch::asm!("sgdt [{}]", in(reg) &mut gdtr, options(nostack));
///     }
///     gdtr
/// }
/// ```
pub trait DescriptorTableAccess {
    fn read_cs(&self) -> SegmentSelector;

    fn read_ss(&self) -> SegmentSelector;

    fn read_ds(&self) -> SegmentSelector;

    fn read_es(&self) -> SegmentSelector;

    fn read_fs(&self) -> SegmentSelector;

    fn read_gs(&self) -> SegmentSelector;

    fn read_tr(&self) -> SegmentSelector;

    /// TR base address; must point to a valid TSS. On Linux, taken from
    /// `this_cpu_ptr(&cpu_tss_rw)` rather than parsing the GDT descriptor.
    fn read_tr_base(&self) -> u64;

    fn read_gdtr(&self) -> Gdtr;

    fn read_idtr(&self) -> Idtr;
}
