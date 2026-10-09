// SPDX-License-Identifier: GPL-2.0

//! VM implementations: [`RootVm`] owns guest memory, [`ForkedVm`] shares its
//! parent's memory copy-on-write. Both implement [`VmContext`].
//!
//! ```text
//! RootVm (owns memory)
//!   └── ForkedVm (COW from RootVm)
//!         └── ForkedVm (COW from parent ForkedVm)
//! ```
//!
//! The [`ParentVm`] and [`ForkableVm`] traits enable this hierarchy.

mod forked;
mod root;
mod traits;

pub use forked::{ForkedVm, ForkedVmError};
pub use root::{RootVm, RootVmError};
pub use traits::{ForkableVm, ParentVm};

/// Exact comparison for the 4KB code images checked on SVM entries. The
/// kernel's scalar memcmp branches once per word; combine eight word results
/// before branching for the common unchanged-page case. Other lengths keep
/// the ordinary slice comparison.
#[inline(always)]
fn equal_guest_bytes(actual: &[u8], expected: &[u8]) -> bool {
    if actual.len() != 4096 || expected.len() != 4096 {
        return actual == expected;
    }
    for offset in (0..4096).step_by(64) {
        let mut difference = 0u64;
        for word in 0..8 {
            // SAFETY: both slices have 4096 bytes and the largest read ends
            // at offset 4095. The pointers need not be u64-aligned.
            unsafe {
                let left = (actual.as_ptr().add(offset + word * 8) as *const u64).read_unaligned();
                let right = (expected.as_ptr().add(offset + word * 8) as *const u64).read_unaligned();
                difference |= left ^ right;
            }
        }
        if difference != 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    extern crate std;

    #[cfg(not(feature = "cargo"))]
    use super::super::prelude::*;
    use super::*;
    #[cfg(feature = "cargo")]
    use crate::prelude::*;

    use crate::test_mocks::{MockMachine, MockPage, MockVmcs};
    use crate::traits::NullInstructionCounter;
    use memory::{HostPhysAddr, VirtAddr};
    use std::cell::RefCell;
    use std::vec::Vec;

    const PAGE_SIZE: usize = 4096;

    #[test]
    fn equal_guest_bytes_checks_entire_page_and_short_slices() {
        let mut actual = [0x5a; PAGE_SIZE];
        let expected = actual;
        assert!(super::equal_guest_bytes(&actual, &expected));
        for index in [0, 7, 8, 63, 64, 2047, 4095] {
            actual[index] ^= 1;
            assert!(!super::equal_guest_bytes(&actual, &expected), "byte {index}");
            actual[index] ^= 1;
        }
        assert!(super::equal_guest_bytes(&actual[1..10], &expected[1..10]));
        actual[9] ^= 1;
        assert!(!super::equal_guest_bytes(&actual[1..10], &expected[1..10]));
    }

    /// Mock guest memory: contiguous virtual memory posing as physical pages.
    struct MockGuestMemory {
        data: Vec<u8>,
    }

    impl MockGuestMemory {
        fn new(size: usize) -> Self {
            Self {
                data: std::vec![0u8; size],
            }
        }
    }

    impl GuestMemory for MockGuestMemory {
        fn size(&self) -> usize {
            self.data.len()
        }

        fn virt_addr(&self) -> VirtAddr {
            VirtAddr::new(self.data.as_ptr() as u64)
        }

        fn page_phys_addr(&self, page_offset: usize) -> Option<HostPhysAddr> {
            if page_offset >= self.data.len() {
                return None;
            }
            // In tests, use virtual address as fake physical address
            let virt = self.data.as_ptr() as u64 + page_offset as u64;
            Some(HostPhysAddr::new(virt))
        }
    }

    /// Mock frame allocator for testing.
    struct MockFrameAllocator {
        next_addr: RefCell<u64>,
    }

    impl MockFrameAllocator {
        fn new() -> Self {
            Self {
                next_addr: RefCell::new(0x1_0000_0000), // Start at 4GB
            }
        }
    }

    #[derive(Debug)]
    struct MockAllocError;

    impl FrameAllocator for MockFrameAllocator {
        type Error = MockAllocError;
        type Frame = MockPage;

        fn allocate_frame(&mut self) -> Result<MockPage, Self::Error> {
            let page = MockPage::new();
            let addr = page.physical_address().as_u64();
            *self.next_addr.borrow_mut() = addr + PAGE_SIZE as u64;

            Ok(page)
        }

        fn frame_phys_addr(frame: &MockPage) -> HostPhysAddr {
            frame.physical_address()
        }

        fn phys_to_virt(&self, phys: HostPhysAddr) -> *mut u8 {
            // MockPage's physical_address() is its data pointer.
            phys.as_u64() as *mut u8
        }
    }

    impl CowAllocator<MockPage> for MockFrameAllocator {
        fn allocate_cow_page(&mut self) -> Result<MockPage, Self::Error> {
            Ok(MockPage::new())
        }
    }

    #[test]
    fn test_rootvm_creation() {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x10000);
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;
        let vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("VM creation should succeed");

        assert_eq!(vm.state.gprs.rax, 0);
        assert_eq!(vm.state.gprs.rcx, 0);
        assert_eq!(vm.memory.size(), 0x10000);
        assert_ne!(vm.state.msr_bitmap.physical_address().as_u64(), 0);
        assert_eq!(vm.state.last_instruction_count, 0);
    }

    #[test]
    fn test_rootvm_gpr_access() {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x1000);
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;
        let mut vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("VM creation should succeed");

        vm.state.gprs.rax = 0x1234;
        vm.state.gprs.rbx = 0x5678;

        assert_eq!(vm.state.gprs.rax, 0x1234);
        assert_eq!(vm.state.gprs.rbx, 0x5678);
    }

    #[test]
    fn test_rootvm_vmcs_access() {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x1000);
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;
        let vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("VM creation should succeed");

        let vmcs_ref = &vm.state.vmcs;
        vmcs_ref.set_field32(crate::VmcsField32::VmExitReason, 10);
        assert_eq!(
            vmcs_ref.get_field32(crate::VmcsField32::VmExitReason),
            Some(10)
        );
    }

    #[test]
    fn test_rootvm_guest_memory_read_write() {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x10000);
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;
        let mut vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("VM creation should succeed");

        let data = [0xDE, 0xAD, 0xBE, 0xEF];
        let result = vm.write_guest_memory(GuestPhysAddr::new(0x1000), &data);
        assert!(result.is_ok());

        let mut buf = [0u8; 4];
        let result = vm.read_guest_memory(GuestPhysAddr::new(0x1000), &mut buf);
        assert!(result.is_ok());
        assert_eq!(buf, data);
    }

    #[test]
    fn test_rootvm_guest_memory_out_of_range() {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x1000); // 4KB
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;
        let mut vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("VM creation should succeed");

        let data = [0u8; 4];
        let result = vm.write_guest_memory(GuestPhysAddr::new(0x1000), &data);
        assert!(matches!(result, Err(MemoryError::OutOfRange)));

        let mut buf = [0u8; 4];
        let result = vm.read_guest_memory(GuestPhysAddr::new(0xFFF), &mut buf);
        assert!(matches!(result, Err(MemoryError::OutOfRange)));
    }

    #[test]
    fn test_rootvm_guest_memory_boundary() {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x1000); // 4KB
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;
        let mut vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("VM creation should succeed");

        let data = [0x11, 0x22, 0x33, 0x44];
        let result = vm.write_guest_memory(GuestPhysAddr::new(0xFFC), &data);
        assert!(result.is_ok());

        let mut buf = [0u8; 4];
        let result = vm.read_guest_memory(GuestPhysAddr::new(0xFFC), &mut buf);
        assert!(result.is_ok());
        assert_eq!(buf, data);
    }

    #[test]
    fn test_rootvm_eptp() {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x1000);
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;
        let vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("VM creation should succeed");

        // EPTP should be non-zero (contains PML4 address + control bits)
        let eptp = vm.state.ept.eptp();
        assert_ne!(eptp, 0);

        // Check memory type bits (should be WB = 6)
        assert_eq!(eptp & 0x7, 6);

        // Check page walk length (should be 3 for 4-level)
        assert_eq!((eptp >> 3) & 0x7, 3);
    }

    // =========================================================================
    // ForkedVm tests
    // =========================================================================

    /// Helper to create a RootVm for forking tests.
    fn create_test_root_vm() -> (
        RootVm<MockVmcs, MockGuestMemory, NullInstructionCounter>,
        MockFrameAllocator,
    ) {
        let vmcs = MockVmcs::new();
        let memory = MockGuestMemory::new(0x10000); // 64KB
        let machine = MockMachine;
        let mut allocator = MockFrameAllocator::new();
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        let vm = RootVm::new(
            vmcs,
            memory,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
            DEFAULT_TSC_FREQUENCY,
        )
        .expect("RootVm creation should succeed");

        (vm, allocator)
    }

    #[test]
    fn test_forkedvm_creation() {
        let (root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        let forked = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("ForkedVm creation should succeed");

        assert_eq!(forked.children_count(), 0);

        assert!(forked.cow_pages.is_empty());

        assert_eq!(root.children_count(), 1);
    }

    #[test]
    fn test_forkedvm_reads_from_parent() {
        let (mut root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        let test_data = [0xDE, 0xAD, 0xBE, 0xEF];
        root.write_guest_memory(GuestPhysAddr::new(0x1000), &test_data)
            .expect("Write to root should succeed");

        let forked = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("ForkedVm creation should succeed");

        // Read from forked - should see parent's data
        let mut buf = [0u8; 4];
        forked
            .read_guest_memory(GuestPhysAddr::new(0x1000), &mut buf)
            .expect("Read from forked should succeed");
        assert_eq!(buf, test_data);
    }

    #[test]
    fn test_forkedvm_cow_fault_creates_page() {
        let (mut root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        let original_data = [0x11, 0x22, 0x33, 0x44];
        root.write_guest_memory(GuestPhysAddr::new(0x1000), &original_data)
            .expect("Write to root should succeed");

        let mut forked = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("ForkedVm creation should succeed");

        assert!(forked.cow_pages.is_empty());

        let result = forked.handle_cow_fault(GuestPhysAddr::new(0x1000), &mut allocator);
        assert!(result.is_some());

        assert_eq!(forked.cow_pages.len(), 1);

        // Should be able to read the data (copied from parent)
        let mut buf = [0u8; 4];
        forked
            .read_guest_memory(GuestPhysAddr::new(0x1000), &mut buf)
            .expect("Read after COW should succeed");
        assert_eq!(buf, original_data);
    }

    #[test]
    fn test_forkedvm_memory_isolation() {
        let (mut root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        let original_data = [0xAA, 0xBB, 0xCC, 0xDD];
        root.write_guest_memory(GuestPhysAddr::new(0x2000), &original_data)
            .expect("Write to root should succeed");

        let mut forked = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("ForkedVm creation should succeed");

        // Trigger COW fault and write different data in fork
        forked
            .handle_cow_fault(GuestPhysAddr::new(0x2000), &mut allocator)
            .expect("COW fault should succeed");

        let new_data = [0x11, 0x22, 0x33, 0x44];
        forked
            .write_guest_memory(GuestPhysAddr::new(0x2000), &new_data)
            .expect("Write to forked should succeed");

        // Forked VM should see new data
        let mut fork_buf = [0u8; 4];
        forked
            .read_guest_memory(GuestPhysAddr::new(0x2000), &mut fork_buf)
            .expect("Read from forked should succeed");
        assert_eq!(fork_buf, new_data);

        // Root should still have original data
        let mut root_buf = [0u8; 4];
        root.read_guest_memory(GuestPhysAddr::new(0x2000), &mut root_buf)
            .expect("Read from root should succeed");
        assert_eq!(root_buf, original_data);
    }

    #[test]
    fn guest_memory_comparison_crosses_parent_and_cow_pages() {
        let (mut root, mut allocator) = create_test_root_vm();
        let address = GuestPhysAddr::new(0x1ffe);
        root.write_guest_memory(address, &[1, 2, 3, 4]).unwrap();
        assert_eq!(root.guest_memory_matches(address, &[1, 2, 3, 4]), Ok(true));

        let mut forked = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &MockMachine,
            &mut allocator,
            0xDEAD_BEEF_0000,
            NullInstructionCounter,
        )
        .unwrap();
        assert_eq!(
            forked.guest_memory_matches(address, &[1, 2, 3, 4]),
            Ok(true)
        );
        forked
            .handle_cow_fault(GuestPhysAddr::new(0x2000), &mut allocator)
            .unwrap();
        forked
            .write_guest_memory(GuestPhysAddr::new(0x2000), &[5, 6])
            .unwrap();
        assert_eq!(
            forked.guest_memory_matches(address, &[1, 2, 3, 4]),
            Ok(false)
        );
        assert_eq!(
            forked.guest_memory_matches(address, &[1, 2, 5, 6]),
            Ok(true)
        );
        assert_eq!(root.guest_memory_matches(address, &[1, 2, 3, 4]), Ok(true));
    }

    #[test]
    fn test_forkedvm_nested_fork() {
        let (mut root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        let root_data = [0x10, 0x20, 0x30, 0x40];
        root.write_guest_memory(GuestPhysAddr::new(0x3000), &root_data)
            .expect("Write to root should succeed");

        let mut fork1 = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("First fork should succeed");

        fork1
            .handle_cow_fault(GuestPhysAddr::new(0x3000), &mut allocator)
            .expect("COW fault in fork1 should succeed");
        let fork1_data = [0xA1, 0xB2, 0xC3, 0xD4];
        fork1
            .write_guest_memory(GuestPhysAddr::new(0x3000), &fork1_data)
            .expect("Write to fork1 should succeed");

        // Nested fork from fork1
        let fork2 = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &fork1,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("Nested fork should succeed");

        // fork2 should see fork1's modified data (via COW chain)
        let mut fork2_buf = [0u8; 4];
        fork2
            .read_guest_memory(GuestPhysAddr::new(0x3000), &mut fork2_buf)
            .expect("Read from fork2 should succeed");
        assert_eq!(fork2_buf, fork1_data);

        assert_eq!(fork1.children_count(), 1);

        assert_eq!(root.children_count(), 1);
    }

    #[test]
    fn test_forkedvm_nested_fork_unmodified_page() {
        let (mut root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        // Write data to root at different pages
        let page1_data = [0x11, 0x11, 0x11, 0x11];
        let page2_data = [0x22, 0x22, 0x22, 0x22];
        root.write_guest_memory(GuestPhysAddr::new(0x0000), &page1_data)
            .expect("Write page1 to root");
        root.write_guest_memory(GuestPhysAddr::new(0x1000), &page2_data)
            .expect("Write page2 to root");

        // First fork - only modifies page1
        let mut fork1 = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("First fork should succeed");

        fork1
            .handle_cow_fault(GuestPhysAddr::new(0x0000), &mut allocator)
            .expect("COW fault");
        let modified = [0xFF, 0xFF, 0xFF, 0xFF];
        fork1
            .write_guest_memory(GuestPhysAddr::new(0x0000), &modified)
            .expect("Write to fork1");

        let fork2 = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &fork1,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("Nested fork should succeed");

        // fork2 reads modified page from fork1
        let mut buf = [0u8; 4];
        fork2
            .read_guest_memory(GuestPhysAddr::new(0x0000), &mut buf)
            .expect("Read");
        assert_eq!(buf, modified);

        // fork2 reads unmodified page from root (via fork1 -> root chain)
        fork2
            .read_guest_memory(GuestPhysAddr::new(0x1000), &mut buf)
            .expect("Read");
        assert_eq!(buf, page2_data);
    }

    #[test]
    fn test_forkedvm_children_count_tracking() {
        let (root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        assert_eq!(root.children_count(), 0);

        let fork1 = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("Fork should succeed");
        assert_eq!(root.children_count(), 1);

        let fork2 = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("Fork should succeed");
        assert_eq!(root.children_count(), 2);

        drop(fork1);
        assert_eq!(root.children_count(), 1);

        drop(fork2);
        assert_eq!(root.children_count(), 0);
    }

    #[test]
    fn test_forkedvm_state_copied_from_parent() {
        let (mut root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        root.state.gprs.rax = 0x1234567890ABCDEF;
        root.state.gprs.rbx = 0xFEDCBA0987654321;
        root.state.emulated_tsc = 12345678;

        let forked = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("Fork should succeed");

        assert_eq!(forked.state.gprs.rax, 0x1234567890ABCDEF);
        assert_eq!(forked.state.gprs.rbx, 0xFEDCBA0987654321);
        assert_eq!(forked.state.emulated_tsc, 12345678);
    }

    #[test]
    fn test_forkedvm_state_isolation() {
        let (mut root, mut allocator) = create_test_root_vm();
        let machine = MockMachine;
        let exit_handler_rip = 0xDEAD_BEEF_0000;

        root.state.gprs.rax = 100;

        let mut forked = ForkedVm::<MockVmcs, MockPage, NullInstructionCounter>::new(
            &root,
            &machine,
            &mut allocator,
            exit_handler_rip,
            NullInstructionCounter,
        )
        .expect("Fork should succeed");

        forked.state.gprs.rax = 200;

        assert_eq!(root.state.gprs.rax, 100);
        assert_eq!(forked.state.gprs.rax, 200);
    }
}
