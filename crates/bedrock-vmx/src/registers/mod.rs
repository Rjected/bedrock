// =============================================================================
// Register Abstractions for x86-64 Virtualization
// =============================================================================

mod cr;
mod descriptor;
mod dr;
mod gpr;
mod msr_defs;
mod segment;
mod syscall;
mod xcr;

pub use guest::GuestRegisters;
mod guest;

pub use gpr::GeneralPurposeRegisters;

pub use cr::{ControlRegisters, Cr0, Cr2, Cr3, Cr4, Cr8, CrAccess, CrError, CrResult};

pub use dr::DebugRegisters;

pub use segment::{SegmentAccessRights, SegmentRegister, SegmentRegisters, SegmentSelector};

pub use descriptor::{DescriptorTableAccess, DescriptorTableRegisters, Gdtr, Idtr};

pub use msr_defs::{msr, MsrAccess, MsrError, MsrResult};

pub use syscall::{Cstar, Efer, ExtendedControlRegisters, Fmask, Lstar, MiscEnable, Star};

pub use xcr::xcr0;
