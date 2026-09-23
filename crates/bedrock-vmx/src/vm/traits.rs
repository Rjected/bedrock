// SPDX-License-Identifier: GPL-2.0

//! Traits for VMs participating in copy-on-write fork hierarchies.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// Object-safe parent access for ForkedVm: page reads through the COW chain
/// and child-count tracking.
///
/// Pointers from `read_page` are valid only while the parent exists; the
/// children counter guarantees the parent outlives its children.
pub trait ParentVm {
    /// Pointer to the page containing `gpa`, or None if out of range. ForkedVm
    /// checks its COW pages first, then delegates to its parent.
    fn read_page(&self, gpa: GuestPhysAddr) -> Option<*const u8>;

    fn memory_size(&self) -> usize;

    /// Decrement children count; called when a child ForkedVm is dropped.
    fn remove_child(&self);
}

/// VM types that can be forked. Implemented by both `RootVm` and `ForkedVm`,
/// allowing nested forks.
pub trait ForkableVm<V: VirtualMachineControlStructure, I: InstructionCounter>: ParentVm {
    type Page: Page;

    fn vm_state(&self) -> &VmState<V, I>;

    fn vm_state_mut(&mut self) -> &mut VmState<V, I>;

    /// Increment children count when a child ForkedVm is created.
    fn add_child(&self);

    /// Decrement children count when a child ForkedVm is dropped.
    fn remove_child(&self);

    fn children_count(&self) -> usize;

    /// A VM with children must not run.
    fn can_run(&self) -> bool {
        self.children_count() == 0
    }
}
