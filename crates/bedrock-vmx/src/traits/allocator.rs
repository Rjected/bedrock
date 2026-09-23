// SPDX-License-Identifier: GPL-2.0

//! Copy-on-write page allocator trait for forked VMs.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// `FrameAllocator` that can also hand out owned `Page`s (freed on drop).
pub trait CowAllocator<P: Page>: FrameAllocator {
    /// Allocate a zeroed, owned page for copy-on-write.
    fn allocate_cow_page(&mut self) -> Result<P, Self::Error>;
}
