// SPDX-License-Identifier: GPL-2.0

//! Platform-agnostic Extended Page Tables (EPT), abstracted over frame
//! allocation and address translation.

#![no_std]

extern crate alloc;

mod compat;
mod entry;
mod table;
mod traits;

#[cfg(test)]
mod tests;

pub use entry::{EptEntry, EptMemoryType, EptPermissions};
pub use table::{EptPageTable, EptRemapError};
pub use traits::{FrameAllocator, PhysAddr, VirtAddr};
