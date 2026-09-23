// SPDX-License-Identifier: GPL-2.0

//! Bedrock handler: manages VMX state and tracks active VMs by ID. VMs are
//! owned by anon_inode file descriptors; the handler also holds a reference.

#[cfg(not(feature = "cargo"))]
use super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

#[cfg(feature = "cargo")]
use core::ptr::NonNull;
#[cfg(feature = "cargo")]
/// Opaque VM reference used by cargo tests and userland crates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VmRef(NonNull<()>);

#[cfg(feature = "cargo")]
impl VmRef {
    pub fn new<T>(ptr: NonNull<T>) -> Self {
        VmRef(ptr.cast())
    }

    pub fn as_ptr(&self) -> *const () {
        self.0.as_ptr()
    }
}

#[cfg(feature = "cargo")]
// SAFETY: VmRef is only used for pointer comparison and tracking in cargo
// builds. The actual VM data access is synchronized elsewhere.
unsafe impl Send for VmRef {}

#[cfg(feature = "cargo")]
type VmHandle = VmRef;
#[cfg(not(feature = "cargo"))]
type VmHandle = ParentVmArc;

/// VM entry in the tracking list.
pub struct VmEntry {
    /// Unique VM identifier.
    pub vm_id: u64,
    /// Strong reference to the VM while it is open and visible by ID.
    pub vm_ref: VmHandle,
}

impl VmEntry {
    pub fn new(vm_id: u64, vm_ref: VmHandle) -> Self {
        Self { vm_id, vm_ref }
    }
}

/// The bedrock hypervisor handler: VMX on/off, active VM list, VM ID allocation.
///
/// Removing a VM drops the handler's strong reference; forked children may
/// still hold references to their parent.
pub struct BedrockHandler<'a, X: Vmx, const MAX_VMS: usize = 64> {
    /// Strong references to all active VMs while they are visible by ID.
    vm_list: HeapVec<VmEntry>,
    /// Next VM ID to assign (monotonically increasing).
    next_vm_id: u64,
    _marker: core::marker::PhantomData<&'a X>,
}

impl<'a, X: Vmx, const MAX_VMS: usize> BedrockHandler<'a, X, MAX_VMS> {
    /// Create a new handler, initializing VMX on all processors.
    pub fn new(machine: &'a X::M) -> Result<Self, VmxInitError> {
        X::initialize(machine)?;

        let vm_list =
            heap_vec_with_capacity(MAX_VMS).map_err(|_| VmxInitError::MemoryAllocationFailed)?;

        Ok(Self {
            vm_list,
            next_vm_id: 1,
            _marker: core::marker::PhantomData,
        })
    }

    pub fn can_create_vm(&self) -> bool {
        self.vm_list.len() < MAX_VMS
    }

    /// Allocate a unique VM ID, or `None` if at MAX_VMS.
    pub fn alloc_vm_id(&mut self) -> Option<u64> {
        if !self.can_create_vm() {
            return None;
        }
        let id = self.next_vm_id;
        self.next_vm_id += 1;
        Some(id)
    }

    /// Register a VM in the tracking list, taking a strong reference.
    #[cfg(not(feature = "cargo"))]
    pub fn add_vm(&mut self, vm: ParentVmArc) {
        let entry = VmEntry::new(vm.vm_id(), vm);
        let _ = heap_vec_push(&mut self.vm_list, entry);
    }

    #[cfg(feature = "cargo")]
    pub fn add_vm<T>(&mut self, vm: NonNull<T>, vm_id: u64) {
        let entry = VmEntry::new(vm_id, VmRef::new(vm));
        let _ = heap_vec_push(&mut self.vm_list, entry);
    }

    /// Drop the handler's reference to a VM; called when its fd is closed.
    pub fn remove_vm<T>(&mut self, vm: *const T) {
        let vm_ptr = vm.cast::<()>();
        self.vm_list.retain(|e| e.vm_ref.as_ptr() != vm_ptr);
    }

    /// Find a VM by ID, returning a cloned strong reference.
    #[cfg(not(feature = "cargo"))]
    pub fn find_vm_by_id(&self, vm_id: u64) -> Option<ParentVmArc> {
        self.vm_list
            .iter()
            .find(|e| e.vm_id == vm_id)
            .map(|e| e.vm_ref.clone())
    }

    #[cfg(feature = "cargo")]
    pub fn find_vm_by_id(&self, vm_id: u64) -> Option<VmRef> {
        self.vm_list
            .iter()
            .find(|e| e.vm_id == vm_id)
            .map(|e| e.vm_ref)
    }
}
