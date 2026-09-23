// SPDX-License-Identifier: GPL-2.0

//! Traits for platform-agnostic EPT implementation.

// Kernel builds: sibling module. Cargo builds: the bedrock-memory crate.
#[cfg(not(feature = "cargo"))]
pub use crate::memory::{GuestPhysAddr, HostPhysAddr, VirtAddr};

#[cfg(feature = "cargo")]
pub use memory::{GuestPhysAddr, HostPhysAddr, PhysAddr, VirtAddr};

/// Trait for allocating physical memory frames for EPT structures.
pub trait FrameAllocator {
    type Error;

    /// Allocated frame; the EPT owns these and frees them on drop.
    type Frame;

    /// Allocate a zeroed 4KB-aligned physical frame.
    fn allocate_frame(&mut self) -> Result<Self::Frame, Self::Error>;

    fn frame_phys_addr(frame: &Self::Frame) -> HostPhysAddr;

    /// Host virtual address for `phys`, used to access EPT structures.
    fn phys_to_virt(&self, phys: HostPhysAddr) -> *mut u8;
}
