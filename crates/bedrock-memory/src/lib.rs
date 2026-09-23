// SPDX-License-Identifier: GPL-2.0

//! Type-safe physical and virtual address wrappers for x86-64 virtualization.

#![no_std]

mod addr;

#[cfg(test)]
mod tests;

pub use addr::{GuestPhysAddr, HostPhysAddr, PhysAddr, VirtAddr};
