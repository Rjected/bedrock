// SPDX-License-Identifier: GPL-2.0

//! Legacy SVM VMCB layout. Offsets follow AMD APM volume 2, chapter 15,
//! and Linux 6.18's arch/x86/include/asm/svm.h. No SEV/AVIC state is enabled.

pub mod offset {
    pub const INTERCEPT_CR_READ: usize = 0x000;
    pub const INTERCEPT_CR_WRITE: usize = 0x002;
    pub const INTERCEPT_EXCEPTIONS: usize = 0x008;
    pub const INTERCEPT_MISC1: usize = 0x00c;
    pub const INTERCEPT_MISC2: usize = 0x010;
    pub const IOPM_BASE: usize = 0x040;
    pub const MSRPM_BASE: usize = 0x048;
    pub const TSC_OFFSET: usize = 0x050;
    pub const ASID: usize = 0x058;
    pub const TLB_CONTROL: usize = 0x05c;
    pub const INT_CONTROL: usize = 0x060;
    pub const INT_VECTOR: usize = 0x064;
    pub const INT_SHADOW: usize = 0x068;
    pub const EXIT_CODE: usize = 0x070;
    pub const EXIT_INFO1: usize = 0x078;
    pub const EXIT_INFO2: usize = 0x080;
    pub const EXIT_INT_INFO: usize = 0x088;
    pub const NESTED_CONTROL: usize = 0x090;
    pub const EVENT_INJECTION: usize = 0x0a8;
    pub const NESTED_CR3: usize = 0x0b0;
    pub const VIRT_EXT: usize = 0x0b8;
    pub const CLEAN_BITS: usize = 0x0c0;
    pub const NEXT_RIP: usize = 0x0c8;
    pub const INSTRUCTION_LEN: usize = 0x0d0;
    pub const INSTRUCTION_BYTES: usize = 0x0d1;
    pub const ES: usize = 0x400;
    pub const CS: usize = 0x410;
    pub const SS: usize = 0x420;
    pub const DS: usize = 0x430;
    pub const FS: usize = 0x440;
    pub const GS: usize = 0x450;
    pub const GDTR: usize = 0x460;
    pub const LDTR: usize = 0x470;
    pub const IDTR: usize = 0x480;
    pub const TR: usize = 0x490;
    pub const CPL: usize = 0x4cb;
    pub const EFER: usize = 0x4d0;
    pub const PERF_CTL0: usize = 0x4e0;
    pub const PERF_CTR0: usize = 0x4e8;
    pub const CR4: usize = 0x548;
    pub const CR3: usize = 0x550;
    pub const CR0: usize = 0x558;
    pub const DR7: usize = 0x560;
    pub const DR6: usize = 0x568;
    pub const RFLAGS: usize = 0x570;
    pub const RIP: usize = 0x578;
    pub const INSTR_RETIRED_CTR: usize = 0x5c0;
    pub const PERF_GLOBAL_STATUS: usize = 0x5c8;
    pub const PERF_GLOBAL_CONTROL: usize = 0x5d0;
    pub const RSP: usize = 0x5d8;
    pub const RAX: usize = 0x5f8;
    pub const STAR: usize = 0x600;
    pub const LSTAR: usize = 0x608;
    pub const CSTAR: usize = 0x610;
    pub const SFMASK: usize = 0x618;
    pub const KERNEL_GS_BASE: usize = 0x620;
    pub const SYSENTER_CS: usize = 0x628;
    pub const SYSENTER_ESP: usize = 0x630;
    pub const SYSENTER_EIP: usize = 0x638;
    pub const CR2: usize = 0x640;
    pub const PAT: usize = 0x668;
    pub const DEBUGCTL: usize = 0x670;
}

/// VMCB physical address must be 4KB aligned. Hardware owns this page only
/// during VMRUN; callers must not retain references across a VM entry.
#[repr(C, align(4096))]
pub struct Vmcb {
    bytes: [u8; 4096],
}

impl Default for Vmcb {
    fn default() -> Self {
        Self::new()
    }
}

impl Vmcb {
    pub const fn new() -> Self {
        Self { bytes: [0; 4096] }
    }

    pub fn read(&self, offset: usize, width: usize) -> u64 {
        assert!(matches!(width, 1 | 2 | 4 | 8));
        let mut result = 0;
        for (i, byte) in self.bytes[offset..offset + width].iter().enumerate() {
            result |= u64::from(*byte) << (8 * i);
        }
        result
    }

    pub fn write(&mut self, offset: usize, width: usize, value: u64) {
        assert!(matches!(width, 1 | 2 | 4 | 8));
        for (i, byte) in self.bytes[offset..offset + width].iter_mut().enumerate() {
            *byte = (value >> (8 * i)) as u8;
        }
        // Always reload state and intercepts, including after a fork.
        self.bytes[offset::CLEAN_BITS..offset::CLEAN_BITS + 4].fill(0);
    }

    /// Intercept numbers are bit indices starting at byte zero of the VMCB.
    pub fn intercept(&mut self, bit: usize, enabled: bool) {
        assert!(bit < 192);
        let byte = &mut self.bytes[bit / 8];
        if enabled {
            *byte |= 1 << (bit % 8);
        } else {
            *byte &= !(1 << (bit % 8));
        }
        self.bytes[offset::CLEAN_BITS..offset::CLEAN_BITS + 4].fill(0);
    }

    /// Enable the traps used by Bedrock. Bitmap addresses are supplied by the
    /// kernel backend; I/O and MSR bitmaps initially intercept every access.
    pub fn initialize(&mut self) {
        // External interrupts, NMI, CPUID, HLT, IOIO, MSR, VMRUN, VMMCALL,
        // VMLOAD, VMSAVE, STGI, CLGI, RDTSC, RDTSCP, RDPMC, MONITOR, MWAIT,
        // XSETBV, RDPRU, INVD, WBINVD, SKINIT. SVM instructions must never execute in the guest.
        for bit in [
            96, 97, 110, 111, 112, 113, 114, 117, 118, 120, 123, 124, 128, 129, 130, 131, 132, 133,
            134, 135, 137, 138, 139, 140, 141, 142,
        ] {
            self.intercept(bit, true);
        }
        // #DB for hypervisor single-stepping; #MC for host error reporting.
        self.write(offset::INTERCEPT_EXCEPTIONS, 4, u32::MAX as u64);
        // Guest debug-register access must not change host breakpoint state.
        self.write(0x004, 4, u32::MAX as u64);
        // CR3 and CR8 are already emulated by the common CR handler.
        self.write(offset::INTERCEPT_CR_READ, 2, (1 << 3) | (1 << 8));
        self.write(offset::INTERCEPT_CR_WRITE, 2, (1 << 3) | (1 << 8));
        self.write(offset::ASID, 4, 1);
        self.write(offset::TLB_CONTROL, 1, 1); // flush all ASIDs on entry
        self.write(offset::NESTED_CONTROL, 8, 1);
        // V_INTR_MASKING: guest IF must not mask host physical interrupts.
        self.write(offset::INT_CONTROL, 4, 1 << 24);
        self.write(offset::DR6, 8, 0xffff0ff0);
        self.write(offset::DR7, 8, 0x400);
    }

    /// AMD APM 15.25.5: ROGPT avoids treating every guest page-table walk as
    /// a nested write. Actual guest A/D updates still fault on protected tables.
    pub fn configure_nested_features(&mut self, features: u32) {
        let control = self.read(offset::NESTED_CONTROL, 8) & !(1 << 6);
        self.write(
            offset::NESTED_CONTROL,
            8,
            control | if features & (1 << 21) != 0 { 1 << 6 } else { 0 },
        );
    }
}

/// Intel access-rights bits 15:12 are packed at bits 11:8 in the VMCB.
/// Intel's unusable bit has no SVM encoding; an unusable segment has attr=0.
pub const fn segment_attributes_from_vmx(value: u32) -> u16 {
    if value & (1 << 16) != 0 {
        0
    } else {
        ((value & 0xff) | ((value >> 4) & 0xf00)) as u16
    }
}

pub const fn segment_attributes_to_vmx(value: u16) -> u32 {
    if value == 0 {
        1 << 16
    } else {
        (value as u32 & 0xff) | ((value as u32 & 0xf00) << 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_page_layout_and_intercepts() {
        assert_eq!(core::mem::size_of::<Vmcb>(), 4096);
        assert_eq!(core::mem::align_of::<Vmcb>(), 4096);
        let mut v = Vmcb::new();
        v.initialize();
        assert_ne!(v.read(0x10, 4) & 3, 0); // VMRUN and VMMCALL
        assert_ne!(v.read(0x0c, 4) & (1 << 22), 0); // INVD
        assert_ne!(v.read(0x10, 4) & (1 << 6), 0); // SKINIT
        assert_ne!(v.read(0x10, 4) & (1 << 9), 0); // WBINVD
        assert_eq!(v.read(0x90, 8), 1); // NPT enabled
        assert_eq!(v.read(0x58, 4), 1); // ASID must not be zero
        assert_eq!(v.read(0x60, 4), 1 << 24);
        v.write(offset::RIP, 8, 0xffff_ffff_8100_0000);
        assert_eq!(v.read(0x578, 8), 0xffff_ffff_8100_0000);
        assert_eq!(v.read(0x570, 8), 0); // no adjacent register corruption
    }

    #[test]
    fn segment_attributes_pack_granularity_and_long_mode() {
        // Linux long-mode CS, writable data, and unusable LDTR.
        for (vmx, svm) in [(0xa09b, 0xa9b), (0xc093, 0xc93), (0x10000, 0)] {
            assert_eq!(segment_attributes_from_vmx(vmx), svm);
            assert_eq!(segment_attributes_to_vmx(svm), vmx);
        }
    }

    #[test]
    fn readonly_guest_tables_require_the_cpuid_feature() {
        let mut v = Vmcb::new();
        v.initialize();
        v.configure_nested_features(1 << 21);
        assert_eq!(v.read(offset::NESTED_CONTROL, 8), 1 | (1 << 6));
        v.configure_nested_features(1 << 6); // FlushByAsid is a different feature.
        assert_eq!(v.read(offset::NESTED_CONTROL, 8), 1);
    }
}
