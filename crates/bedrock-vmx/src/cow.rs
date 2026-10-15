// SPDX-License-Identifier: GPL-2.0

//! Copy-on-write page tracking for forked VMs (BTreeMap in cargo builds,
//! kernel RBTree in kernel builds).

/// Error returned when inserting a COW page fails (e.g., allocation failure).
#[derive(Debug, Clone, Copy)]
pub struct CowInsertError;

use core::sync::atomic::{AtomicU64, Ordering};

const NEGATIVE_CACHE_CAPACITY: usize = 256;

/// A missed private COW lookup stays a miss until this VM inserts that page.
/// Exact page keys make collisions an efficiency loss only.
struct NegativeCache {
    pages: [AtomicU64; NEGATIVE_CACHE_CAPACITY],
}

impl NegativeCache {
    fn new() -> Self {
        Self { pages: [const { AtomicU64::new(0) }; NEGATIVE_CACHE_CAPACITY] }
    }

    fn slot(page: u64) -> usize {
        ((page >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize
    }

    fn contains(&self, page: u64) -> bool {
        self.pages[Self::slot(page)].load(Ordering::Relaxed) == page.wrapping_add(1)
    }

    fn remember(&self, page: u64) {
        self.pages[Self::slot(page)].store(page.wrapping_add(1), Ordering::Relaxed);
    }

    fn invalidate(&self, page: u64) {
        self.pages[Self::slot(page)].store(0, Ordering::Relaxed);
    }
}

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
        negative_cache: super::NegativeCache,
    }

    impl<P: Page> CowPageMap<P> {
        pub fn new() -> Self {
            Self {
                pages: BTreeMap::new(),
                count: 0,
                negative_cache: super::NegativeCache::new(),
            }
        }

        /// COW page containing `gpa`, if this VM has copied it.
        pub fn get(&self, gpa: GuestPhysAddr) -> Option<&P> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            if self.negative_cache.contains(page_aligned) {
                return None;
            }
            let page = self.pages.get(&page_aligned);
            if page.is_none() {
                self.negative_cache.remember(page_aligned);
            }
            page
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
            self.negative_cache.invalidate(page_aligned);
            Ok(())
        }

        pub fn contains(&self, gpa: GuestPhysAddr) -> bool {
            self.get(gpa).is_some()
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
        negative_cache: super::NegativeCache,
    }

    impl<P: Page> CowPageMap<P> {
        pub fn new() -> Self {
            Self {
                pages: RBTree::new(),
                count: 0,
                negative_cache: super::NegativeCache::new(),
            }
        }

        /// COW page containing `gpa`, if this VM has copied it.
        pub fn get(&self, gpa: GuestPhysAddr) -> Option<&P> {
            let page_aligned = gpa.as_u64() & !0xFFF;
            if self.negative_cache.contains(page_aligned) {
                return None;
            }
            let page = self.pages.get(&page_aligned);
            if page.is_none() {
                self.negative_cache.remember(page_aligned);
            }
            page
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
                    self.negative_cache.invalidate(page_aligned);
                    Ok(())
                }
                Err(_) => Err(super::CowInsertError),
            }
        }

        pub fn contains(&self, gpa: GuestPhysAddr) -> bool {
            self.get(gpa).is_some()
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

#[cfg(all(test, feature = "cargo"))]
mod tests {
    use super::{CowPageMap, NegativeCache};
    use crate::test_mocks::MockPage;
    use memory::GuestPhysAddr;

    #[test]
    fn negative_lookup_is_revoked_by_insert_and_collision_is_exact() {
        let mut map = CowPageMap::<MockPage>::new();
        let present = GuestPhysAddr::new(0x3000);
        assert!(map.get(present).is_none());
        assert!(map.get(present).is_none());

        let colliding_page = (4..10_000)
            .map(|page| page << 12)
            .find(|&page| NegativeCache::slot(page) == NegativeCache::slot(0x3000))
            .unwrap();
        map.insert(present, MockPage::new()).unwrap();
        assert!(map.get(present).is_some());

        // A colliding miss may evict the hint, but cannot hide an owned page.
        assert!(map.get(GuestPhysAddr::new(colliding_page)).is_none());
        assert!(map.get(present).is_some());
    }
}
