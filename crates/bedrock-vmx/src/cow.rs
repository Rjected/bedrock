// SPDX-License-Identifier: GPL-2.0

//! Copy-on-write page tracking for forked VMs (BTreeMap in cargo builds,
//! kernel RBTree in kernel builds).

/// Error returned when inserting a COW page fails (e.g., allocation failure).
#[derive(Debug, Clone, Copy)]
pub struct CowInsertError;

// Cargo build: Use alloc::collections::BTreeMap

#[cfg(feature = "cargo")]
mod cargo_impl {
    extern crate alloc;

    use alloc::collections::BTreeMap;
    use memory::GuestPhysAddr;

    use crate::traits::Page;

    /// Tracks copy-on-write pages for a forked VM.
    ///
    /// Only stores pages THIS VM has modified; ancestor pages are reached via
    /// the EPT, which already points at the right host frames.
    pub struct CowPageMap<P: Page> {
        /// Maps page-aligned GPAs to owned pages.
        pages: BTreeMap<u64, P>,
        count: usize,
    }

    impl<P: Page> CowPageMap<P> {
        pub fn new() -> Self {
            Self {
                pages: BTreeMap::new(),
                count: 0,
            }
        }

        /// COW page containing `gpa`, if this VM has copied it.
        pub fn get(&self, gpa: GuestPhysAddr) -> Option<&P> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            self.pages.get(&page_aligned)
        }

        pub fn get_mut(&mut self, gpa: GuestPhysAddr) -> Option<&mut P> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            self.pages.get_mut(&page_aligned)
        }

        /// Insert a COW page for the page containing `gpa`.
        pub fn insert(&mut self, gpa: GuestPhysAddr, page: P) -> Result<(), super::CowInsertError> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            if self.pages.insert(page_aligned, page).is_none() {
                self.count += 1;
            }
            Ok(())
        }

        pub fn contains(&self, gpa: GuestPhysAddr) -> bool {
            let page_aligned = gpa.as_u64() & !0xFFF;
            self.pages.contains_key(&page_aligned)
        }

        pub fn len(&self) -> usize {
            self.count
        }

        pub fn is_empty(&self) -> bool {
            self.count == 0
        }

        /// Iterate over (page-aligned GPA, page) pairs.
        pub fn iter(&self) -> impl Iterator<Item = (GuestPhysAddr, &P)> {
            self.pages
                .iter()
                .map(|(&gpa, page)| (GuestPhysAddr::new(gpa), page))
        }
    }

    impl<P: Page> Default for CowPageMap<P> {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(feature = "cargo")]
pub use cargo_impl::CowPageMap;

// Kernel build: Use kernel::rbtree::RBTree

#[cfg(not(feature = "cargo"))]
mod kernel_impl {
    use kernel::alloc::flags::GFP_ATOMIC;
    use kernel::rbtree::RBTree;

    use crate::memory::GuestPhysAddr;
    use crate::vmx::traits::Page;

    /// Tracks copy-on-write pages for a forked VM. See the cargo impl.
    pub struct CowPageMap<P: Page> {
        /// Maps page-aligned GPAs to owned pages.
        pages: RBTree<u64, P>,
        count: usize,
    }

    impl<P: Page> CowPageMap<P> {
        pub fn new() -> Self {
            Self {
                pages: RBTree::new(),
                count: 0,
            }
        }

        /// COW page containing `gpa`, if this VM has copied it.
        pub fn get(&self, gpa: GuestPhysAddr) -> Option<&P> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            self.pages.get(&page_aligned)
        }

        pub fn get_mut(&mut self, gpa: GuestPhysAddr) -> Option<&mut P> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            self.pages.get_mut(&page_aligned)
        }

        /// Insert a COW page for the page containing `gpa`. Fails on allocation failure.
        pub fn insert(&mut self, gpa: GuestPhysAddr, page: P) -> Result<(), super::CowInsertError> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            match self
                .pages
                .try_create_and_insert(page_aligned, page, GFP_ATOMIC)
            {
                Ok(_) => {
                    self.count += 1;
                    Ok(())
                }
                Err(_) => Err(super::CowInsertError),
            }
        }

        pub fn contains(&self, gpa: GuestPhysAddr) -> bool {
            let page_aligned = gpa.as_u64() & !0xFFF;
            self.pages.get(&page_aligned).is_some()
        }

        pub fn len(&self) -> usize {
            self.count
        }

        pub fn is_empty(&self) -> bool {
            self.count == 0
        }

        /// Iterate over (page-aligned GPA, page) pairs.
        pub fn iter(&self) -> impl Iterator<Item = (GuestPhysAddr, &P)> {
            self.pages
                .iter()
                .map(|(gpa, page)| (GuestPhysAddr::new(*gpa), page))
        }
    }

    impl<P: Page> Default for CowPageMap<P> {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(not(feature = "cargo"))]
pub use kernel_impl::CowPageMap;
