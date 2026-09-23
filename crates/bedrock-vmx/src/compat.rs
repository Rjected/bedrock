// SPDX-License-Identifier: GPL-2.0

//! Allocation helpers abstracting over cargo and kernel builds. All
//! allocation cfg gates live here.

/// Error returned when heap allocation fails.
#[derive(Debug, Clone, Copy)]
pub struct AllocError;

#[cfg(feature = "cargo")]
mod cargo_impl {
    extern crate alloc;

    pub type HeapBox<T> = alloc::boxed::Box<T>;

    /// Same as HeapBox in cargo builds.
    pub type VmallocBox<T> = alloc::boxed::Box<T>;

    pub type HeapVec<T> = alloc::vec::Vec<T>;

    pub fn heap_box<T>(val: T) -> HeapBox<T> {
        alloc::boxed::Box::new(val)
    }

    /// Never fails in cargo builds (the allocator aborts on OOM).
    pub fn heap_box_try<T>(val: T) -> Result<HeapBox<T>, super::AllocError> {
        Ok(alloc::boxed::Box::new(val))
    }

    /// Box a heap copy of `*src`. See the kernel impl.
    pub fn heap_box_copy_from<T: Copy>(src: &T) -> Result<HeapBox<T>, super::AllocError> {
        use core::mem::MaybeUninit;
        let mut boxed: alloc::boxed::Box<MaybeUninit<T>> =
            alloc::boxed::Box::new(MaybeUninit::uninit());
        // SAFETY: `boxed` is a fresh, aligned, T-sized slot that doesn't overlap
        // `src`; after the copy it is fully initialized, so the cast to `Box<T>`
        // is sound. `T: Copy` means leaving `*src` in place can't double-free.
        unsafe {
            core::ptr::copy_nonoverlapping(src, boxed.as_mut_ptr(), 1);
            Ok(alloc::boxed::Box::from_raw(
                alloc::boxed::Box::into_raw(boxed) as *mut T,
            ))
        }
    }

    pub fn heap_vec_with_capacity<T>(cap: usize) -> Result<HeapVec<T>, super::AllocError> {
        Ok(alloc::vec::Vec::with_capacity(cap))
    }

    /// Never fails in cargo builds (the allocator aborts on OOM).
    pub fn heap_vec_push<T>(v: &mut HeapVec<T>, val: T) -> Result<(), super::AllocError> {
        v.push(val);
        Ok(())
    }

    /// Pop the front element. O(n); fine for the small FIFO queues it serves.
    pub fn heap_vec_remove_front<T>(v: &mut HeapVec<T>) -> Option<T> {
        if v.is_empty() {
            None
        } else {
            Some(v.remove(0))
        }
    }
}

#[cfg(not(feature = "cargo"))]
mod kernel_impl {
    /// kmalloc, GFP_KERNEL.
    pub type HeapBox<T> = kernel::alloc::KBox<T>;

    /// kvmalloc: falls back to vmalloc for large allocations.
    pub type VmallocBox<T> = kernel::alloc::KVBox<T>;

    /// kmalloc, GFP_KERNEL.
    pub type HeapVec<T> = kernel::alloc::KVec<T>;

    /// Panics on allocation failure.
    pub fn heap_box<T>(val: T) -> HeapBox<T> {
        kernel::alloc::KBox::new(val, kernel::alloc::flags::GFP_KERNEL)
            .expect("Failed to allocate HeapBox")
    }

    /// Fallible `heap_box`. Use on guest-controlled paths (e.g. feedback-buffer
    /// registration) where OOM must be reported to the guest, not panic.
    pub fn heap_box_try<T>(val: T) -> Result<HeapBox<T>, super::AllocError> {
        kernel::alloc::KBox::new(val, kernel::alloc::flags::GFP_KERNEL)
            .map_err(|_| super::AllocError)
    }

    /// Box a heap copy of `*src` without materializing it on the stack.
    ///
    /// `heap_box_try(*src)` would go through a stack temporary, which for
    /// kilobyte-sized POD like `FeedbackBufferInfo` can blow the 8KB kernel
    /// stack on deep call chains.
    pub fn heap_box_copy_from<T: Copy>(src: &T) -> Result<HeapBox<T>, super::AllocError> {
        let mut boxed: kernel::alloc::KBox<core::mem::MaybeUninit<T>> =
            kernel::alloc::KBox::new_uninit(kernel::alloc::flags::GFP_KERNEL)
                .map_err(|_| super::AllocError)?;
        // SAFETY: `boxed` is a fresh, aligned, T-sized slot that doesn't overlap
        // `src`; after the copy it is fully initialized, so `assume_init` is
        // sound. `T: Copy` means leaving `*src` in place can't double-free.
        unsafe {
            core::ptr::copy_nonoverlapping(src, boxed.as_mut_ptr(), 1);
            Ok(boxed.assume_init())
        }
    }

    pub fn heap_vec_with_capacity<T>(cap: usize) -> Result<HeapVec<T>, super::AllocError> {
        kernel::alloc::KVec::with_capacity(cap, kernel::alloc::flags::GFP_KERNEL)
            .map_err(|_| super::AllocError)
    }

    /// Callers must propagate ENOMEM on failure rather than drop the value.
    pub fn heap_vec_push<T>(v: &mut HeapVec<T>, val: T) -> Result<(), super::AllocError> {
        v.push(val, kernel::alloc::flags::GFP_KERNEL)
            .map_err(|_| super::AllocError)
    }

    /// Pop the front element, or `None` if empty.
    pub fn heap_vec_remove_front<T>(v: &mut HeapVec<T>) -> Option<T> {
        v.remove(0).ok()
    }
}

#[cfg(feature = "cargo")]
pub use cargo_impl::*;
#[cfg(not(feature = "cargo"))]
pub use kernel_impl::*;
