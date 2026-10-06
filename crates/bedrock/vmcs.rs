// SPDX-License-Identifier: GPL-2.0

//! VMCS (Virtual Machine Control Structure) implementation.

use core::arch::asm;

use super::machine::LinuxMachine;
use super::memory::HostPhysAddr;
use super::page::KernelPage;
use super::vmx::traits::Page as PageTrait;
use super::vmx::{
    VirtualMachineControlStructure, VmcsField16, VmcsField32, VmcsField64, VmcsFieldNatural,
    VmcsReadError, VmcsReadResult, VmcsWriteError, VmcsWriteResult,
};

/// Real VMCS implementation backed by a kernel page.
pub(crate) struct RealVmcs {
    /// Backing page for the VMCS region; freed on drop.
    page: Option<KernelPage>,
    svm: bool,
    svm_bitmaps: Option<super::svm::Bitmaps>,
}

impl RealVmcs {
    /// Get the physical address, panicking if the VMCS is uninitialized.
    pub(crate) fn svm_phys_addr(&self) -> u64 {
        self.phys_addr().as_u64()
    }

    fn phys_addr(&self) -> HostPhysAddr {
        self.page
            .as_ref()
            .expect("VMCS is uninitialized")
            .physical_address()
    }

    fn vmread(&self, field: u64) -> VmcsReadResult<u64> {
        if self.svm {
            // VM lock excludes hardware execution and concurrent mutation.
            let v = unsafe { &*(self.vmcs_region_ptr().cast::<super::svm_core::vmcb::Vmcb>()) };
            return super::svm_core::fields::read(v, field as u32)
                .map_err(|_| VmcsReadError::InvalidField);
        }
        let value: u64;
        let rflags: u64;
        // SAFETY: VMREAD is valid when a VMCS is loaded; caller ensures the VMCS is active.
        unsafe {
            asm!(
                "vmread {0}, {1}",
                "pushfq",
                "pop {2}",
                out(reg) value,
                in(reg) field,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 {
            Err(VmcsReadError::VmcsNotLoaded)
        } else if zf == 1 {
            Err(VmcsReadError::InvalidField)
        } else {
            Ok(value)
        }
    }

    fn vmwrite(&self, field: u64, value: u64) -> VmcsWriteResult {
        if self.svm {
            let v = unsafe { &mut *(self.vmcs_region_ptr().cast::<super::svm_core::vmcb::Vmcb>()) };
            return super::svm_core::fields::write(v, field as u32, value)
                .map_err(|_| VmcsWriteError::InvalidField);
        }
        let rflags: u64;
        // SAFETY: VMWRITE is valid when a VMCS is loaded; caller ensures the VMCS is active.
        unsafe {
            asm!(
                "vmwrite {0}, {1}",
                "pushfq",
                "pop {2}",
                in(reg) field,
                in(reg) value,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 {
            Err(VmcsWriteError::VmcsNotLoaded)
        } else if zf == 1 {
            Err(VmcsWriteError::InvalidField)
        } else {
            Ok(())
        }
    }
}

impl VirtualMachineControlStructure for RealVmcs {
    type P = KernelPage;
    type M = LinuxMachine;

    fn clear(&self) -> Result<(), &'static str> {
        if self.svm {
            return Ok(());
        }
        let addr = self.phys_addr().as_u64();

        let rflags: u64;
        // SAFETY: VMCLEAR with a valid physical address clears the VMCS launch state.
        unsafe {
            asm!(
                "vmclear [{0}]",
                "pushfq",
                "pop {1}",
                in(reg) &addr,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 || zf == 1 {
            let vm_err: u64 = if zf == 1 {
                let err: u64;
                // SAFETY: VMREAD of the error field is valid after a failed VM instruction.
                unsafe {
                    asm!(
                        "vmread {0}, {1}",
                        out(reg) err,
                        in(reg) 0x4400u64, // VM_INSTRUCTION_ERROR field
                        options(nostack)
                    );
                }
                err
            } else {
                0
            };
            log_err!(
                "VMCLEAR failed: addr={:#x} CF={} ZF={} vm_err={}\n",
                addr,
                cf,
                zf,
                vm_err
            );
            Err("VMCLEAR failed")
        } else {
            Ok(())
        }
    }

    fn load(&self) -> Result<(), &'static str> {
        if self.svm {
            // Rebind after a fork copies its parent's hardware page.
            let b = self
                .svm_bitmaps
                .as_ref()
                .ok_or("SVM bitmap allocation failed")?;
            let v = unsafe { &mut *(self.vmcs_region_ptr().cast::<super::svm_core::vmcb::Vmcb>()) };
            b.bind(v);
            return Ok(());
        }
        let addr = self.phys_addr().as_u64();

        let rflags: u64;
        // SAFETY: VMPTRLD with a valid physical address loads the VMCS as active.
        unsafe {
            asm!(
                "vmptrld [{0}]",
                "pushfq",
                "pop {1}",
                in(reg) &addr,
                out(reg) rflags,
                options(nostack)
            );
        }

        let cf = rflags & 1;
        let zf = (rflags >> 6) & 1;

        if cf == 1 || zf == 1 {
            let vm_err: u64 = if zf == 1 {
                let err: u64;
                // SAFETY: VMREAD of the error field is valid after a failed VM instruction.
                unsafe {
                    asm!(
                        "vmread {0}, {1}",
                        out(reg) err,
                        in(reg) 0x4400u64, // VM_INSTRUCTION_ERROR field
                        options(nostack)
                    );
                }
                err
            } else {
                0
            };
            log_err!(
                "VMPTRLD failed: addr={:#x} CF={} ZF={} vm_err={}\n",
                addr,
                cf,
                zf,
                vm_err
            );
            Err("VMPTRLD failed")
        } else {
            Ok(())
        }
    }

    fn read16(&self, field: VmcsField16) -> VmcsReadResult<u16> {
        self.vmread(field as u64).map(|v| v as u16)
    }

    fn read32(&self, field: VmcsField32) -> VmcsReadResult<u32> {
        self.vmread(field as u64).map(|v| v as u32)
    }

    fn read64(&self, field: VmcsField64) -> VmcsReadResult<u64> {
        self.vmread(field as u64)
    }

    fn read_natural(&self, field: VmcsFieldNatural) -> VmcsReadResult<u64> {
        self.vmread(field as u64)
    }

    fn write16(&self, field: VmcsField16, value: u16) -> VmcsWriteResult {
        self.vmwrite(field as u64, u64::from(value))
    }

    fn write32(&self, field: VmcsField32, value: u32) -> VmcsWriteResult {
        self.vmwrite(field as u64, u64::from(value))
    }

    fn write64(&self, field: VmcsField64, value: u64) -> VmcsWriteResult {
        self.vmwrite(field as u64, value)
    }

    fn write_natural(&self, field: VmcsFieldNatural, value: u64) -> VmcsWriteResult {
        self.vmwrite(field as u64, value)
    }

    fn vmcs_region_ptr(&self) -> *mut u8 {
        self.page
            .as_ref()
            .expect("VMCS is uninitialized")
            .virtual_address()
            .as_u64() as *mut u8
    }

    fn from_parts(page: KernelPage, _revision_id: u32) -> Self {
        let svm = super::svm::supported();
        let svm_bitmaps = if svm {
            super::svm::Bitmaps::new()
        } else {
            None
        };
        if svm {
            let v = unsafe {
                &mut *(page.virtual_address().as_u64() as *mut super::svm_core::vmcb::Vmcb)
            };
            v.initialize();
        }
        Self {
            page: Some(page),
            svm,
            svm_bitmaps,
        }
    }
}
