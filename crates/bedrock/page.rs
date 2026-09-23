// SPDX-License-Identifier: GPL-2.0

//! Kernel page and guest memory allocation.

use kernel::alloc::flags::{__GFP_ZERO, GFP_KERNEL};
use kernel::alloc::{allocator::KVmalloc, Vec as KVec};
use kernel::page::Page;

use super::c_helpers;
use super::memory::{HostPhysAddr, VirtAddr};
use super::vmx::traits::{GuestMemory, Page as PageTrait};

/// A kernel-allocated page wrapping the kernel crate's Page type.
pub(crate) struct KernelPage {
    /// Owning handle, kept only to free the page on drop.
    _page: Page,
    pub(crate) phys: HostPhysAddr,
    pub(crate) virt: VirtAddr,
}

impl PageTrait for KernelPage {
    fn physical_address(&self) -> HostPhysAddr {
        self.phys
    }

    fn virtual_address(&self) -> VirtAddr {
        self.virt
    }
}

/// Guest memory from vmalloc_user: virtually contiguous, mappable to
/// userspace, freed on drop.
pub(crate) struct KernelGuestMemory {
    ptr: *mut u8,
    size: usize,
}

// SAFETY: The pointed-to memory is owned exclusively by this struct.
unsafe impl Send for KernelGuestMemory {}
// SAFETY: KernelGuestMemory only provides shared access through &self methods
// that don't allow mutation of the underlying memory without &mut self.
unsafe impl Sync for KernelGuestMemory {}

impl KernelGuestMemory {
    /// Allocate guest memory of the given size.
    pub(crate) fn new(size: usize) -> Option<Self> {
        log_info!(
            "KernelGuestMemory::new: calling vmalloc_user({} bytes)\n",
            size
        );
        // SAFETY: bedrock_vmalloc_user allocates zeroed memory that can be mapped to userspace.
        let ptr = unsafe { c_helpers::bedrock_vmalloc_user(size as core::ffi::c_ulong) };
        log_info!("KernelGuestMemory::new: vmalloc_user returned {:p}\n", ptr);
        if ptr.is_null() {
            return None;
        }

        Some(Self {
            ptr: ptr.cast::<u8>(),
            size,
        })
    }
}

impl GuestMemory for KernelGuestMemory {
    fn size(&self) -> usize {
        self.size
    }

    fn virt_addr(&self) -> VirtAddr {
        VirtAddr::new(self.ptr as u64)
    }

    fn page_phys_addr(&self, page_offset: usize) -> Option<HostPhysAddr> {
        if page_offset >= self.size {
            return None;
        }
        // SAFETY: ptr + page_offset is within the allocated vmalloc region.
        let page_ptr = unsafe { self.ptr.add(page_offset) };
        // SAFETY: page_ptr is within the allocated vmalloc region (checked above).
        let phys =
            unsafe { c_helpers::bedrock_vmalloc_to_phys(page_ptr.cast::<core::ffi::c_void>()) };
        if phys == 0 {
            return None;
        }
        Some(HostPhysAddr::new(phys))
    }
}

impl Drop for KernelGuestMemory {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: ptr was allocated by bedrock_vmalloc_user and has not been freed yet.
            unsafe {
                c_helpers::bedrock_vfree(self.ptr.cast::<core::ffi::c_void>());
            }
        }
    }
}

/// 1MB vmalloc'd event-stream buffer mapped to userspace, holding the TLV
/// records of `bedrock_vmx::events`. Allocated on SET_EVENT_CONFIG enable,
/// freed on disable or file close.
pub(crate) struct EventBuffer {
    ptr: *mut u8,
}

/// Event buffer size: 1MB. Must match `bedrock_vmx::events::EVENT_BUFFER_SIZE`.
pub(crate) const EVENT_BUFFER_SIZE: usize = 1024 * 1024;

// SAFETY: EventBuffer owns its vmalloc'd region exclusively.
unsafe impl Send for EventBuffer {}
// SAFETY: EventBuffer only provides shared access through &self methods.
unsafe impl Sync for EventBuffer {}

impl EventBuffer {
    /// Allocate a new event buffer.
    pub(crate) fn new() -> Option<Self> {
        log_info!("EventBuffer::new: allocating {} bytes\n", EVENT_BUFFER_SIZE);
        // SAFETY: bedrock_vmalloc_user allocates zeroed memory mappable to userspace.
        let ptr =
            unsafe { c_helpers::bedrock_vmalloc_user(EVENT_BUFFER_SIZE as core::ffi::c_ulong) };
        if ptr.is_null() {
            log_err!("EventBuffer::new: vmalloc_user failed\n");
            return None;
        }
        log_info!("EventBuffer::new: allocated at {:p}\n", ptr);
        Some(Self {
            ptr: ptr.cast::<u8>(),
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for EventBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            log_info!("EventBuffer::drop: freeing {:p}\n", self.ptr);
            // SAFETY: ptr was allocated by bedrock_vmalloc_user and not yet freed.
            unsafe {
                c_helpers::bedrock_vfree(self.ptr.cast::<core::ffi::c_void>());
            }
        }
    }
}

/// Allocate a zeroed kernel page.
pub(crate) fn alloc_zeroed_page() -> Option<KernelPage> {
    let page = Page::alloc_page(GFP_KERNEL | __GFP_ZERO).ok()?;

    // SAFETY: page.as_ptr() returns a valid struct page pointer.
    let phys_addr = unsafe { c_helpers::bedrock_page_to_phys(page.as_ptr()) };

    // SAFETY: page.as_ptr() returns a valid struct page pointer.
    let virt_addr = unsafe { c_helpers::bedrock_page_address(page.as_ptr()) as u64 };

    Some(KernelPage {
        _page: page,
        phys: HostPhysAddr::new(phys_addr),
        virt: VirtAddr::new(virt_addr),
    })
}

/// Page pool filled with `GFP_KERNEL` in sleepable context (ioctl handlers) and
/// drained by the run loop with preemption disabled. When exhausted mid-run,
/// the run loop exits to sleepable context to refill.
pub(crate) struct PagePool {
    pages: KVec<KernelPage, KVmalloc>,
    target: usize,
}

impl PagePool {
    pub(crate) fn new(target: usize) -> Self {
        Self {
            pages: KVec::new(),
            target,
        }
    }

    /// Refill to target; sleepable context only. No-op unless below 5% of target.
    pub(crate) fn refill(&mut self) -> bool {
        let threshold = self.target / 20; // 5%
        if self.pages.len() >= threshold {
            return true;
        }
        while self.pages.len() < self.target {
            match alloc_zeroed_page() {
                Some(page) => {
                    if self.pages.push(page, GFP_KERNEL).is_err() {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }

    pub(crate) fn take(&mut self) -> Option<KernelPage> {
        self.pages.pop()
    }
}
