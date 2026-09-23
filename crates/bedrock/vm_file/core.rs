// SPDX-License-Identifier: GPL-2.0

//! Per-VM state stored in file descriptors.

use core::sync::atomic::AtomicBool;

use kernel::sync::Arc;

use super::super::instruction_counter::LinuxInstructionCounter;
use super::super::page::{EventBuffer, KernelGuestMemory, KernelPage, PagePool};
use super::super::vmcs::RealVmcs;
use super::super::vmx::{ForkedVm, RootVm};

/// Type discriminant; must be the first field of both BedrockVmFile and
/// BedrockForkedVmFile so the type can be identified through a raw pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum VmFileType {
    /// Root VM that owns its memory.
    Root = 0,
    /// Forked VM using copy-on-write from a parent.
    Forked = 1,
}

/// Per-VM state in `file->private_data`; dropped (freeing the VM) on close.
#[repr(C)]
pub(crate) struct BedrockVmFile {
    /// Type discriminant - MUST be first field for safe type identification.
    pub vm_file_type: VmFileType,
    pub vm: RootVm<RealVmcs, KernelGuestMemory, LinuxInstructionCounter>,
    pub vm_id: u64,
    /// Set while a RUN ioctl is in progress, to detect concurrent RUN.
    pub running: AtomicBool,
    /// Event-stream buffer; present while SET_EVENT_CONFIG has it enabled.
    pub event_buffer: Option<EventBuffer>,
    /// COW page pool for the run loop. Root VMs don't COW, so target=0.
    pub page_pool: PagePool,
}

impl BedrockVmFile {
    pub(crate) fn new(
        vm: RootVm<RealVmcs, KernelGuestMemory, LinuxInstructionCounter>,
        vm_id: u64,
    ) -> Self {
        Self {
            vm_file_type: VmFileType::Root,
            vm,
            vm_id,
            running: AtomicBool::new(false),
            event_buffer: None,
            page_pool: PagePool::new(0),
        }
    }
}

/// Strong reference to any VM file that can be used as a fork parent.
#[derive(Clone)]
pub(crate) enum ParentVmArc {
    /// Root VM parent.
    Root(Arc<BedrockVmFile>),
    /// Forked VM parent for nested forks.
    Forked(Arc<BedrockForkedVmFile>),
}

impl ParentVmArc {
    /// Return the VM's unique identifier.
    pub(crate) fn vm_id(&self) -> u64 {
        match self {
            Self::Root(vm_file) => vm_file.vm_id,
            Self::Forked(vm_file) => vm_file.vm_id,
        }
    }

    /// Return a stable data pointer for removing this VM from the handler.
    pub(crate) fn as_ptr(&self) -> *const () {
        match self {
            Self::Root(vm_file) => core::ptr::from_ref::<BedrockVmFile>(vm_file.as_ref()).cast(),
            Self::Forked(vm_file) => {
                core::ptr::from_ref::<BedrockForkedVmFile>(vm_file.as_ref()).cast()
            }
        }
    }

    /// Return the VM file type.
    pub(crate) fn file_type(&self) -> VmFileType {
        match self {
            Self::Root(_) => VmFileType::Root,
            Self::Forked(_) => VmFileType::Forked,
        }
    }
}

// SAFETY: Concurrency on the VM files is controlled by the VM state atomics,
// per-file operation serialization, and the global handler mutex; the Arc only
// keeps the allocation alive across those externally synchronized paths.
unsafe impl Send for ParentVmArc {}

// SAFETY: Shared access is only for parent reads during fork and lifetime
// management; mutation goes through the file callbacks' synchronization.
unsafe impl Sync for ParentVmArc {}

/// Per-forked-VM state in `file->private_data`. Dropping it frees the VM and
/// decrements the parent's children count.
#[repr(C)]
pub(crate) struct BedrockForkedVmFile {
    /// Type discriminant - MUST be first field for safe type identification.
    pub vm_file_type: VmFileType,
    /// The forked VM with COW memory.
    pub vm: ForkedVm<RealVmcs, KernelPage, LinuxInstructionCounter>,
    /// Strong parent reference that keeps inherited memory alive.
    _parent: ParentVmArc,
    pub vm_id: u64,
    /// Flag to detect concurrent access to RUN ioctl.
    pub running: AtomicBool,
    /// Optional unified event-stream buffer (see [`BedrockVmFile::event_buffer`]).
    pub event_buffer: Option<EventBuffer>,
    /// Pre-allocated page pool for COW during the run loop.
    pub page_pool: PagePool,
}

/// COW pool target for forked VMs (512 pages = 2MB); refilled below 5%.
pub(crate) const COW_POOL_SIZE: usize = 512;

impl BedrockForkedVmFile {
    pub(crate) fn new(
        vm: ForkedVm<RealVmcs, KernelPage, LinuxInstructionCounter>,
        parent: ParentVmArc,
        vm_id: u64,
    ) -> Self {
        Self {
            vm_file_type: VmFileType::Forked,
            vm,
            _parent: parent,
            vm_id,
            running: AtomicBool::new(false),
            event_buffer: None,
            page_pool: PagePool::new(COW_POOL_SIZE),
        }
    }
}
