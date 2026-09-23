// SPDX-License-Identifier: GPL-2.0

//! EPT frame-list vectors: vmalloc-backed in kernel builds since the list can
//! outgrow physically contiguous allocation.

#[cfg(feature = "cargo")]
mod cargo_impl {
    extern crate alloc;

    pub type EptVec<T> = alloc::vec::Vec<T>;

    pub fn ept_vec_init<T>(val: T) -> EptVec<T> {
        alloc::vec![val]
    }

    pub fn ept_vec_with_capacity<T>(cap: usize) -> EptVec<T> {
        alloc::vec::Vec::with_capacity(cap)
    }

    pub fn ept_vec_push<T>(v: &mut EptVec<T>, val: T) {
        v.push(val);
    }
}

#[cfg(not(feature = "cargo"))]
mod kernel_impl {
    use kernel::alloc::{allocator::KVmalloc, flags::GFP_KERNEL, Vec};

    pub type EptVec<T> = Vec<T, KVmalloc>;

    pub fn ept_vec_init<T>(val: T) -> EptVec<T> {
        let mut v = Vec::new();
        let _ = v.push(val, GFP_KERNEL);
        v
    }

    pub fn ept_vec_with_capacity<T>(cap: usize) -> EptVec<T> {
        let mut v = Vec::new();
        let _ = v.reserve(cap, GFP_KERNEL);
        v
    }

    pub fn ept_vec_push<T>(v: &mut EptVec<T>, val: T) {
        let _ = v.push(val, GFP_KERNEL);
    }
}

#[cfg(feature = "cargo")]
pub use cargo_impl::*;
#[cfg(not(feature = "cargo"))]
pub use kernel_impl::*;
