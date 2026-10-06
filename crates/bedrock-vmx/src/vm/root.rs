// SPDX-License-Identifier: GPL-2.0

//! RootVm - a VM that owns its guest memory.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::{ForkableVm, ParentVm};
use core::sync::atomic::{AtomicUsize, Ordering};

const PAGE_SIZE: usize = 4096;

/// A root VM owning its guest physical memory and EPT.
#[repr(C)]
pub struct RootVm<V: VirtualMachineControlStructure, G: GuestMemory, I: InstructionCounter> {
    /// VM state. Boxed to reduce stack usage.
    pub state: VmStateBox<V, I>,
    /// Guest physical memory. Owned by this VM and freed on drop.
    pub memory: G,
    /// Number of child forks. While non-zero this VM must not run (children
    /// reference our memory). Atomic because remove_child takes &self.
    children_count: AtomicUsize,
}

/// Error type for RootVm creation.
#[derive(Debug)]
pub enum RootVmError<E> {
    /// EPT page table creation failed.
    EptCreation(E),
    /// EPT mapping failed.
    EptMapping(E),
    /// Guest memory page has no physical address.
    NoPhysAddr(usize),
    /// VmState creation failed.
    VmState(VmStateError<E>),
}

impl<V: VirtualMachineControlStructure, G: GuestMemory, I: InstructionCounter> RootVm<V, G, I> {
    /// Create a new RootVm, mapping all guest memory into a fresh EPT
    /// (GPA = offset into `memory`). `vmcs` must already carry the revision ID;
    /// `tsc_frequency` is in Hz.
    #[inline(never)]
    pub fn new<A: FrameAllocator<Frame = V::P>>(
        vmcs: V,
        memory: G,
        machine: &V::M,
        allocator: &mut A,
        exit_handler_rip: u64,
        instruction_counter: I,
        tsc_frequency: u64,
    ) -> Result<Self, RootVmError<A::Error>> {
        let format = if <V::M as Machine>::V::uses_nested_paging() {
            PageTableFormat::AmdNpt
        } else {
            PageTableFormat::IntelEpt
        };
        let mut ept: EptPageTable<V::P> =
            EptPageTable::new_with_format(allocator, format).map_err(RootVmError::EptCreation)?;

        // Skip the LAPIC (0xFEE00000) and IOAPIC (0xFEC00000) MMIO pages so
        // accesses EPT-fault into handle_apic_access / handle_ioapic_access.
        // Otherwise guests with >~4GB RAM map them as regular memory.
        let mem_size = memory.size();
        let num_pages = mem_size.div_ceil(PAGE_SIZE);

        for page_idx in 0..num_pages {
            let page_offset = page_idx * PAGE_SIZE;
            let guest_phys_u64 = page_offset as u64;

            if (APIC_BASE..APIC_BASE + APIC_SIZE).contains(&guest_phys_u64)
                || (IOAPIC_BASE..IOAPIC_BASE + IOAPIC_SIZE).contains(&guest_phys_u64)
            {
                continue;
            }

            let guest_phys = GuestPhysAddr::new(guest_phys_u64);
            let host_phys = memory
                .page_phys_addr(page_offset)
                .ok_or(RootVmError::NoPhysAddr(page_offset))?;

            ept.map_4k(
                allocator,
                guest_phys,
                host_phys,
                EptPermissions::READ_WRITE_EXECUTE,
                EptMemoryType::WriteBack,
            )
            .map_err(RootVmError::EptMapping)?;
        }

        let state = VmState::new::<A>(
            vmcs,
            ept,
            machine,
            exit_handler_rip,
            instruction_counter,
            tsc_frequency,
        )
        .map_err(RootVmError::VmState)?;

        Ok(Self {
            state: box_vm_state(state),
            memory,
            children_count: AtomicUsize::new(0),
        })
    }
}

impl<V: VirtualMachineControlStructure, G: GuestMemory, I: InstructionCounter> VmContext
    for RootVm<V, G, I>
{
    type Vmcs = V;
    type V = <V::M as Machine>::V;
    type I = I;
    type CowPage = V::P; // RootVm doesn't use COW, but type is needed for trait

    fn state(&self) -> &VmState<Self::Vmcs, Self::I> {
        &self.state
    }

    fn state_mut(&mut self) -> &mut VmState<Self::Vmcs, Self::I> {
        &mut self.state
    }

    fn read_guest_memory(&self, gpa: GuestPhysAddr, buf: &mut [u8]) -> Result<(), MemoryError> {
        let offset = gpa.as_u64() as usize;
        let end = offset
            .checked_add(buf.len())
            .ok_or(MemoryError::OutOfRange)?;

        if end > self.memory.size() {
            return Err(MemoryError::OutOfRange);
        }

        // SAFETY: We've verified the offset and length are within bounds above.
        let src = unsafe { self.memory.as_ptr().add(offset) };
        // SAFETY: src points within guest memory, buf is a valid mutable slice,
        // and we verified offset + buf.len() <= memory.size() above.
        unsafe {
            core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    fn write_guest_memory(&mut self, gpa: GuestPhysAddr, buf: &[u8]) -> Result<(), MemoryError> {
        let offset = gpa.as_u64() as usize;
        let end = offset
            .checked_add(buf.len())
            .ok_or(MemoryError::OutOfRange)?;

        if end > self.memory.size() {
            return Err(MemoryError::OutOfRange);
        }

        // SAFETY: We've verified the offset and length are within bounds above.
        let dst = unsafe { self.memory.as_mut_ptr().add(offset) };
        // SAFETY: dst points within guest memory, buf is a valid slice,
        // and we verified offset + buf.len() <= memory.size() above.
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len());
        }
        Ok(())
    }

    fn finalize_exit_record<K: Kernel>(&mut self, _kernel: &K) {
        // Nothing to do unless an `Exit` event awaits its deferred memory hash.
        if self.state.pending_exit_loc.is_none() {
            return;
        }

        let mem_ptr = self.memory.as_ptr();
        let mem_size = self.memory.size();

        let memory_hash = if self.state.skip_memory_hash {
            0
        } else {
            match self.state.exit_trigger {
                ExitTrigger::AtTsc
                | ExitTrigger::AtShutdown
                | ExitTrigger::AllExits
                | ExitTrigger::Checkpoints
                | ExitTrigger::TscRange => {
                    let mut hasher = Xxh64Hasher::new();
                    // SAFETY: mem_ptr is valid and mem_size is the correct size
                    let memory = unsafe { core::slice::from_raw_parts(mem_ptr, mem_size) };
                    hasher.write_bytes(memory);
                    hasher.finish()
                }
                ExitTrigger::Disabled => 0,
            }
        };

        // Root VMs have no COW pages, so cow_page_count is 0.
        self.state.finalize_exit_memory_hash(memory_hash, 0);
    }
}

/// Clear the VMCS on drop: its page must not be freed while still loaded on a CPU.
impl<V: VirtualMachineControlStructure, G: GuestMemory, I: InstructionCounter> Drop
    for RootVm<V, G, I>
{
    fn drop(&mut self) {
        if let Err(_e) = self.state.vmcs.clear() {
            log_err!("Failed to clear VMCS during drop\n");
        }
        deallocate_vpid(self.state.vpid);
    }
}

impl<V: VirtualMachineControlStructure, G: GuestMemory, I: InstructionCounter> ParentVm
    for RootVm<V, G, I>
{
    fn read_page(&self, gpa: GuestPhysAddr) -> Option<*const u8> {
        let page_gpa = gpa.as_u64() & !0xFFF;
        let offset = page_gpa as usize;

        if offset + PAGE_SIZE <= self.memory.size() {
            // SAFETY: We verified offset + PAGE_SIZE is within the guest memory bounds.
            Some(unsafe { self.memory.as_ptr().add(offset) })
        } else {
            None
        }
    }

    fn memory_size(&self) -> usize {
        self.memory.size()
    }

    fn remove_child(&self) {
        self.children_count.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<V: VirtualMachineControlStructure, G: GuestMemory, I: InstructionCounter> ForkableVm<V, I>
    for RootVm<V, G, I>
{
    type Page = V::P;

    fn vm_state(&self) -> &VmState<V, I> {
        &self.state
    }

    fn vm_state_mut(&mut self) -> &mut VmState<V, I> {
        &mut self.state
    }

    fn add_child(&self) {
        self.children_count.fetch_add(1, Ordering::SeqCst);
    }

    fn remove_child(&self) {
        self.children_count.fetch_sub(1, Ordering::SeqCst);
    }

    fn children_count(&self) -> usize {
        self.children_count.load(Ordering::SeqCst)
    }
}
