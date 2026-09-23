// SPDX-License-Identifier: GPL-2.0

//! Anonymous-inode FD creation for VMs.

use core::ffi::c_int;
use kernel::bindings;
use kernel::sync::Arc;

use super::super::c_helpers::bedrock_anon_inode_getfd;
use super::super::instruction_counter::LinuxInstructionCounter;
use super::super::page::{KernelGuestMemory, KernelPage};
use super::super::vmcs::RealVmcs;
use super::super::vmx::{ForkedVm, RootVm};
use super::super::HANDLER;
use super::core::{BedrockForkedVmFile, BedrockVmFile, ParentVmArc};
use super::forked::BEDROCK_FORKED_VM_FOPS;
use super::root::BEDROCK_VM_FOPS;

/// Wrap the VM in a `BedrockVmFile`, register it in the handler's vm_list, and
/// create its FD, which owns the VM. On failure the VM is freed.
#[inline(never)]
pub(crate) fn create_vm_fd(
    vm: RootVm<RealVmcs, KernelGuestMemory, LinuxInstructionCounter>,
    vm_id: u64,
) -> Result<i32, kernel::error::Error> {
    // The FD owns this Arc reference via private_data.
    let vm_file = Arc::new(
        BedrockVmFile::new(vm, vm_id),
        kernel::alloc::flags::GFP_KERNEL,
    )?;
    let handler_ref = ParentVmArc::Root(vm_file.clone());
    let vm_ptr = Arc::into_raw(vm_file).cast_mut();

    // The handler owns a strong reference while the VM is visible by ID.
    {
        let mut guard = HANDLER.lock();
        if let Some(handler) = guard.as_mut() {
            handler.add_vm(handler_ref);
        }
    }

    // SAFETY: The name is a valid C string, BEDROCK_VM_FOPS is a static
    // file_operations, and vm_ptr is a valid heap-allocated BedrockVmFile.
    let fd = unsafe {
        bedrock_anon_inode_getfd(
            c"bedrock-vm".as_ptr(),
            &BEDROCK_VM_FOPS.0,
            vm_ptr.cast::<core::ffi::c_void>(),
            bindings::O_RDWR as c_int | bindings::O_CLOEXEC as c_int,
        )
    };

    if fd < 0 {
        {
            let mut guard = HANDLER.lock();
            if let Some(handler) = guard.as_mut() {
                handler.remove_vm(vm_ptr);
            }
        }
        // SAFETY: vm_ptr came from Arc::into_raw above and was not handed to
        // the kernel, so we drop that Arc.
        let _ = unsafe { Arc::from_raw(vm_ptr) };
        return Err(kernel::error::Error::from_errno(fd));
    }

    Ok(fd)
}

/// Like [`create_vm_fd`] for a forked VM. The parent's children count is
/// already incremented; dropping the ForkedVm on FD close decrements it.
#[inline(never)]
pub(crate) fn create_forked_vm_fd(
    vm: ForkedVm<RealVmcs, KernelPage, LinuxInstructionCounter>,
    parent: ParentVmArc,
    vm_id: u64,
) -> Result<i32, kernel::error::Error> {
    let vm_file = Arc::new(
        BedrockForkedVmFile::new(vm, parent, vm_id),
        kernel::alloc::flags::GFP_KERNEL,
    )?;
    let handler_ref = ParentVmArc::Forked(vm_file.clone());
    let vm_ptr = Arc::into_raw(vm_file).cast_mut();

    // The handler owns a strong reference while the VM is visible by ID.
    {
        let mut guard = HANDLER.lock();
        if let Some(handler) = guard.as_mut() {
            handler.add_vm(handler_ref);
        }
    }

    // SAFETY: The name is a valid C string, BEDROCK_FORKED_VM_FOPS is a static
    // file_operations, and vm_ptr is a valid heap-allocated BedrockForkedVmFile.
    let fd = unsafe {
        bedrock_anon_inode_getfd(
            c"bedrock-forked-vm".as_ptr(),
            &BEDROCK_FORKED_VM_FOPS.0,
            vm_ptr.cast::<core::ffi::c_void>(),
            bindings::O_RDWR as c_int | bindings::O_CLOEXEC as c_int,
        )
    };

    if fd < 0 {
        {
            let mut guard = HANDLER.lock();
            if let Some(handler) = guard.as_mut() {
                handler.remove_vm(vm_ptr);
            }
        }
        // SAFETY: vm_ptr came from Arc::into_raw above and was not handed to
        // the kernel, so we drop that Arc.
        let _ = unsafe { Arc::from_raw(vm_ptr) };
        return Err(kernel::error::Error::from_errno(fd));
    }

    Ok(fd)
}
