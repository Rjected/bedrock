// SPDX-License-Identifier: GPL-2.0

//! EPT page table management.

use super::compat::{ept_vec_init, ept_vec_push, ept_vec_with_capacity, EptVec};

use super::entry::{EptEntry, EptMemoryType, EptPermissions, PageTableFormat};
use super::traits::{FrameAllocator, GuestPhysAddr, HostPhysAddr, VirtAddr};

/// Error type for EPT remap operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EptRemapError {
    /// The guest physical address is not mapped.
    NotMapped,
}

/// 4-level EPT hierarchy (PML4 -> PDPT -> PD -> PT). Owns its frames, which
/// are freed on drop.
pub struct EptPageTable<Frame> {
    format: PageTableFormat,
    /// Host physical address of the PML4 table (used for EPTP).
    pml4_phys: HostPhysAddr,
    /// All allocated EPT frames. vmalloc-backed in kernel builds since the list
    /// can outgrow contiguous kmalloc.
    frames: EptVec<Frame>,
}

/// Temporary AMD execute restriction. Restore before changing mappings or
/// letting another vCPU use these tables; invalidate translations on entry.
#[must_use]
pub struct NptExecutionGuard {
    tables: [HostPhysAddr; 8],
    table_count: usize,
    changed_nx: [[u64; 8]; 8],
    leaves: [(u8, u16, bool); 4],
    leaf_count: usize,
}

impl NptExecutionGuard {
    pub fn restore<Frame, A: FrameAllocator>(self, ept: &mut EptPageTable<Frame>, allocator: &A) {
        assert_eq!(ept.format, PageTableFormat::AmdNpt);
        for level in 0..self.table_count {
            let table = allocator
                .phys_to_virt(self.tables[level])
                .cast::<EptEntry>();
            for index in 0..512 {
                if self.changed_nx[level][index / 64] & (1 << (index % 64)) != 0 {
                    // SAFETY: these live tables are exclusively guarded.
                    // Preserve hardware A/D updates when restoring NX.
                    unsafe { (*table.add(index)).set_npt_nx(false) };
                }
            }
        }
        for &(table, index, writable) in &self.leaves[..self.leaf_count] {
            let leaf = allocator
                .phys_to_virt(self.tables[table as usize])
                .cast::<EptEntry>();
            // SAFETY: mappings remain fixed until this guard is restored.
            unsafe { (*leaf.add(index as usize)).set_npt_writable(writable) };
        }
    }
}

impl<Frame> EptPageTable<Frame> {
    /// Create an EPT with a zeroed PML4.
    pub fn new<A: FrameAllocator<Frame = Frame>>(allocator: &mut A) -> Result<Self, A::Error> {
        Self::new_with_format(allocator, PageTableFormat::IntelEpt)
    }

    pub fn new_with_format<A: FrameAllocator<Frame = Frame>>(
        allocator: &mut A,
        format: PageTableFormat,
    ) -> Result<Self, A::Error> {
        let pml4_frame = allocator.allocate_frame()?;
        let pml4_phys = A::frame_phys_addr(&pml4_frame);

        let pml4_virt = allocator.phys_to_virt(pml4_phys);
        // SAFETY: pml4_virt points to a freshly allocated 4KB-aligned frame, and we
        // zero the entire page to initialize the PML4 table entries.
        unsafe {
            core::ptr::write_bytes(pml4_virt, 0, 4096);
        }

        let frames = ept_vec_init(pml4_frame);

        Ok(Self {
            pml4_phys,
            frames,
            format,
        })
    }

    /// EPTP value for the VMCS (write-back, 4-level walk).
    pub fn eptp(&self) -> u64 {
        if self.format == PageTableFormat::AmdNpt {
            return self.pml4_phys.as_u64();
        }
        let mem_type = 6u64; // WB
        let page_walk_len = 3u64; // 4 levels - 1
        self.pml4_phys.as_u64() | (page_walk_len << 3) | mem_type
    }

    /// Host physical address and permissions for `gpa`, if mapped.
    ///
    /// Only uses the allocator's `phys_to_virt`.
    pub fn lookup<A: FrameAllocator>(
        &self,
        allocator: &A,
        guest_phys: GuestPhysAddr,
    ) -> Option<(HostPhysAddr, EptPermissions)> {
        let guest_virt = VirtAddr::new(guest_phys.as_u64());
        let mut effective = 7;

        let pml4 = allocator
            .phys_to_virt(self.pml4_phys)
            .cast::<EptEntry>()
            .cast_const();
        // SAFETY: pml4 points to a valid PML4 table and the index is masked to 0..511,
        // so the resulting pointer is within the 4KB page.
        let pml4e = unsafe { &*pml4.add(guest_virt.pml4_index()) };
        if !pml4e.is_present() {
            return None;
        }
        effective &= pml4e.permissions_with_format(self.format).bits();

        let pdpt = allocator
            .phys_to_virt(pml4e.addr())
            .cast::<EptEntry>()
            .cast_const();
        // SAFETY: pdpt points to a valid PDPT table (pml4e is present) and the index
        // is masked to 0..511, so the resulting pointer is within the 4KB page.
        let pdpte = unsafe { &*pdpt.add(guest_virt.pdpt_index()) };
        if !pdpte.is_present() {
            return None;
        }
        effective &= pdpte.permissions_with_format(self.format).bits();

        let pd = allocator
            .phys_to_virt(pdpte.addr())
            .cast::<EptEntry>()
            .cast_const();
        // SAFETY: pd points to a valid PD table (pdpte is present) and the index
        // is masked to 0..511, so the resulting pointer is within the 4KB page.
        let pde = unsafe { &*pd.add(guest_virt.pd_index()) };
        if !pde.is_present() {
            return None;
        }
        effective &= pde.permissions_with_format(self.format).bits();

        let pt = allocator
            .phys_to_virt(pde.addr())
            .cast::<EptEntry>()
            .cast_const();
        // SAFETY: pt points to a valid PT table (pde is present) and the index
        // is masked to 0..511, so the resulting pointer is within the 4KB page.
        let pte = unsafe { &*pt.add(guest_virt.pt_index()) };
        if !pte.is_present() {
            return None;
        }

        Some((
            pte.addr(),
            EptPermissions::from_bits(effective & pte.permissions_with_format(self.format).bits()),
        ))
    }

    /// Allow instruction fetch only from `page`, and prevent code writes.
    /// Sibling subtrees are disabled at each level, touching at most 2048
    /// entries independently of RAM size. Read/write access elsewhere stays
    /// available. Existing NX and copy-on-write permissions are preserved.
    pub fn restrict_execution_to_page<A: FrameAllocator>(
        &mut self,
        allocator: &A,
        page: GuestPhysAddr,
    ) -> Option<NptExecutionGuard> {
        self.restrict_execution_to_pages(allocator, &[page])
    }

    /// Permit up to four immutable code pages, blocking every other fetch.
    /// The bounded table union avoids allocation while the VM holds IRQs off.
    pub fn restrict_execution_to_pages<A: FrameAllocator>(
        &mut self,
        allocator: &A,
        pages: &[GuestPhysAddr],
    ) -> Option<NptExecutionGuard> {
        if self.format != PageTableFormat::AmdNpt || pages.is_empty() || pages.len() > 4 {
            return None;
        }
        let mut guard = NptExecutionGuard {
            tables: [self.pml4_phys; 8],
            table_count: 0,
            changed_nx: [[0; 8]; 8],
            leaves: [(0, 0, false); 4],
            leaf_count: pages.len(),
        };
        // First validate every path without mutating permissions. The masks
        // temporarily record selected edges, then become restoration masks.
        for (page_index, page) in pages.iter().enumerate() {
            let address = VirtAddr::new(page.as_u64());
            let indices = [
                address.pml4_index(),
                address.pdpt_index(),
                address.pd_index(),
                address.pt_index(),
            ];
            let mut physical = self.pml4_phys;
            for (level, index) in indices.into_iter().enumerate() {
                let slot = match guard.tables[..guard.table_count]
                    .iter()
                    .position(|&p| p == physical)
                {
                    Some(slot) => slot,
                    None => {
                        if guard.table_count == guard.tables.len() {
                            return None;
                        }
                        let slot = guard.table_count;
                        guard.tables[slot] = physical;
                        guard.table_count += 1;
                        slot
                    }
                };
                let table = allocator.phys_to_virt(physical).cast::<EptEntry>();
                // SAFETY: validated tables are owned by this EPT, index <512.
                let entry = unsafe { *table.add(index) };
                if !entry.is_present()
                    || entry.raw() & (1 << 63) != 0
                    || (level < 3 && entry.raw() & (1 << 7) != 0)
                {
                    return None;
                }
                guard.changed_nx[slot][index / 64] |= 1 << (index % 64);
                if level == 3 {
                    guard.leaves[page_index] = (slot as u8, index as u16, entry.raw() & 2 != 0);
                } else {
                    physical = entry.addr();
                }
            }
        }
        for slot in 0..guard.table_count {
            let table = allocator
                .phys_to_virt(guard.tables[slot])
                .cast::<EptEntry>();
            for index in 0..512 {
                let mask = 1 << (index % 64);
                let selected = guard.changed_nx[slot][index / 64] & mask != 0;
                guard.changed_nx[slot][index / 64] &= !mask;
                // SAFETY: exclusive EPT access; every path was validated above.
                let entry = unsafe { &mut *table.add(index) };
                if !selected && entry.is_present() && entry.raw() & (1 << 63) == 0 {
                    entry.set_npt_nx(true);
                    guard.changed_nx[slot][index / 64] |= mask;
                }
            }
        }
        for &(slot, index, _) in &guard.leaves[..guard.leaf_count] {
            let table = allocator
                .phys_to_virt(guard.tables[slot as usize])
                .cast::<EptEntry>();
            // SAFETY: all selected leaves were validated before mutation.
            unsafe { (*table.add(index as usize)).set_npt_writable(false) };
        }
        Some(guard)
    }

    /// Map a 4KB guest physical page to a host physical page.
    pub fn map_4k<A: FrameAllocator<Frame = Frame>>(
        &mut self,
        allocator: &mut A,
        guest_phys: GuestPhysAddr,
        host_phys: HostPhysAddr,
        perms: EptPermissions,
        mem_type: EptMemoryType,
    ) -> Result<(), A::Error> {
        let guest_virt = VirtAddr::new(guest_phys.as_u64());

        let pml4_entry =
            self.get_or_create_entry(allocator, self.pml4_phys, guest_virt.pml4_index())?;
        let pdpt_phys = self.ensure_table(allocator, pml4_entry, perms)?;

        let pdpt_entry = self.get_or_create_entry(allocator, pdpt_phys, guest_virt.pdpt_index())?;
        let pd_phys = self.ensure_table(allocator, pdpt_entry, perms)?;

        let pd_entry = self.get_or_create_entry(allocator, pd_phys, guest_virt.pd_index())?;
        let pt_phys = self.ensure_table(allocator, pd_entry, perms)?;

        let pt_entry = self.get_entry_mut(allocator, pt_phys, guest_virt.pt_index());
        // SAFETY: pt_entry points to a valid, aligned EptEntry within an allocated PT
        // page, obtained via get_entry_mut which ensures the pointer is in bounds.
        unsafe {
            *pt_entry = EptEntry::page_entry_with_format(host_phys, perms, mem_type, self.format);
        }

        Ok(())
    }

    /// Change the HPA and/or permissions of an already-mapped 4KB page (used for
    /// COW). Fails if the page is not mapped. Only uses the allocator's
    /// `phys_to_virt`.
    pub fn remap_4k<A: FrameAllocator>(
        &mut self,
        allocator: &A,
        guest_phys: GuestPhysAddr,
        new_host_phys: HostPhysAddr,
        perms: EptPermissions,
        mem_type: EptMemoryType,
    ) -> Result<(), EptRemapError> {
        let guest_virt = VirtAddr::new(guest_phys.as_u64());

        let pml4 = allocator
            .phys_to_virt(self.pml4_phys)
            .cast::<EptEntry>()
            .cast_const();
        // SAFETY: pml4 points to a valid PML4 table and the index is masked to 0..511,
        // so the resulting pointer is within the 4KB page.
        let pml4e = unsafe { &*pml4.add(guest_virt.pml4_index()) };
        if !pml4e.is_present() {
            return Err(EptRemapError::NotMapped);
        }

        let pdpt = allocator
            .phys_to_virt(pml4e.addr())
            .cast::<EptEntry>()
            .cast_const();
        // SAFETY: pdpt points to a valid PDPT table (pml4e is present) and the index
        // is masked to 0..511, so the resulting pointer is within the 4KB page.
        let pdpte = unsafe { &*pdpt.add(guest_virt.pdpt_index()) };
        if !pdpte.is_present() {
            return Err(EptRemapError::NotMapped);
        }

        let pd = allocator
            .phys_to_virt(pdpte.addr())
            .cast::<EptEntry>()
            .cast_const();
        // SAFETY: pd points to a valid PD table (pdpte is present) and the index
        // is masked to 0..511, so the resulting pointer is within the 4KB page.
        let pde = unsafe { &*pd.add(guest_virt.pd_index()) };
        if !pde.is_present() {
            return Err(EptRemapError::NotMapped);
        }

        let pt = allocator.phys_to_virt(pde.addr()).cast::<EptEntry>();
        // SAFETY: pt points to a valid PT table (pde is present) and the index
        // is masked to 0..511. We need a mutable reference to update the mapping.
        let pte = unsafe { &mut *pt.add(guest_virt.pt_index()) };
        if !pte.is_present() {
            return Err(EptRemapError::NotMapped);
        }

        *pte = EptEntry::page_entry_with_format(new_host_phys, perms, mem_type, self.format);
        Ok(())
    }

    // Pointer to entry `index` of the table at `table_phys`.
    fn get_entry_mut<A: FrameAllocator<Frame = Frame>>(
        &self,
        allocator: &A,
        table_phys: HostPhysAddr,
        index: usize,
    ) -> *mut EptEntry {
        let table_virt = allocator.phys_to_virt(table_phys).cast::<EptEntry>();
        // SAFETY: table_virt points to a valid EPT table page and index is in 0..511,
        // so the resulting pointer is within the allocated 4KB page.
        unsafe { table_virt.add(index) }
    }

    fn get_or_create_entry<A: FrameAllocator<Frame = Frame>>(
        &self,
        allocator: &A,
        table_phys: HostPhysAddr,
        index: usize,
    ) -> Result<*mut EptEntry, A::Error> {
        Ok(self.get_entry_mut(allocator, table_phys, index))
    }

    // Return the table `entry` points to, allocating a zeroed one if absent.
    fn ensure_table<A: FrameAllocator<Frame = Frame>>(
        &mut self,
        allocator: &mut A,
        entry: *mut EptEntry,
        perms: EptPermissions,
    ) -> Result<HostPhysAddr, A::Error> {
        // SAFETY: entry is a valid, aligned pointer to an EptEntry within an allocated
        // EPT page, obtained from get_entry_mut.
        let current = unsafe { *entry };

        if current.is_present() {
            Ok(current.addr())
        } else {
            let new_frame = allocator.allocate_frame()?;
            let new_phys = A::frame_phys_addr(&new_frame);

            let table_virt = allocator.phys_to_virt(new_phys);
            // SAFETY: table_virt points to a freshly allocated 4KB-aligned frame, and
            // we zero the entire page to initialize all table entries.
            unsafe {
                core::ptr::write_bytes(table_virt, 0, 4096);
            }

            // SAFETY: entry is a valid, aligned, writable pointer to an EptEntry
            // obtained from get_entry_mut. Writing the new table entry is safe.
            unsafe {
                *entry = EptEntry::table_entry_with_format(new_phys, perms, self.format);
            }

            ept_vec_push(&mut self.frames, new_frame);

            Ok(new_phys)
        }
    }

    /// Returns the number of allocated frames (page table nodes + PML4).
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Deep-copy this EPT for forking: new intermediate tables, leaves keep
    /// their HPAs but become R+X so writes fault into COW handling.
    pub fn clone_for_fork<A: FrameAllocator<Frame = Frame>>(
        &self,
        allocator: &mut A,
    ) -> Result<Self, A::Error> {
        let mut frames = ept_vec_with_capacity(self.frames.len());

        let new_pml4_frame = allocator.allocate_frame()?;
        let new_pml4_phys = A::frame_phys_addr(&new_pml4_frame);
        let new_pml4_virt = allocator.phys_to_virt(new_pml4_phys);
        // SAFETY: new_pml4_virt points to a freshly allocated 4KB-aligned frame, and
        // we zero the entire page to initialize all PML4 entries.
        unsafe {
            core::ptr::write_bytes(new_pml4_virt, 0, 4096);
        }

        ept_vec_push(&mut frames, new_pml4_frame);

        let src_pml4 = allocator
            .phys_to_virt(self.pml4_phys)
            .cast::<EptEntry>()
            .cast_const();
        let dst_pml4 = new_pml4_virt.cast::<EptEntry>();

        for pml4_idx in 0..512 {
            // SAFETY: src_pml4 points to a valid PML4 table and pml4_idx is in 0..511,
            // so the resulting pointer is within the 4KB page.
            let src_pml4e = unsafe { &*src_pml4.add(pml4_idx) };
            if !src_pml4e.is_present() {
                continue;
            }

            let new_pdpt_frame = allocator.allocate_frame()?;
            let new_pdpt_phys = A::frame_phys_addr(&new_pdpt_frame);
            let new_pdpt_virt = allocator.phys_to_virt(new_pdpt_phys);
            // SAFETY: new_pdpt_virt points to a freshly allocated 4KB-aligned frame,
            // and we zero the entire page to initialize all PDPT entries.
            unsafe {
                core::ptr::write_bytes(new_pdpt_virt, 0, 4096);
            }

            ept_vec_push(&mut frames, new_pdpt_frame);

            // Same permissions as source.
            // SAFETY: dst_pml4 points to a valid PML4 table and pml4_idx is in 0..511,
            // so the write target is within the allocated page.
            unsafe {
                *dst_pml4.add(pml4_idx) = EptEntry::table_entry_with_format(
                    new_pdpt_phys,
                    src_pml4e.permissions_with_format(self.format),
                    self.format,
                );
            }

            let src_pdpt = allocator
                .phys_to_virt(src_pml4e.addr())
                .cast::<EptEntry>()
                .cast_const();
            let dst_pdpt = new_pdpt_virt.cast::<EptEntry>();

            for pdpt_idx in 0..512 {
                // SAFETY: src_pdpt points to a valid PDPT table and pdpt_idx is in 0..511,
                // so the resulting pointer is within the 4KB page.
                let src_pdpte = unsafe { &*src_pdpt.add(pdpt_idx) };
                if !src_pdpte.is_present() {
                    continue;
                }

                let new_pd_frame = allocator.allocate_frame()?;
                let new_pd_phys = A::frame_phys_addr(&new_pd_frame);
                let new_pd_virt = allocator.phys_to_virt(new_pd_phys);
                // SAFETY: new_pd_virt points to a freshly allocated 4KB-aligned frame,
                // and we zero the entire page to initialize all PD entries.
                unsafe {
                    core::ptr::write_bytes(new_pd_virt, 0, 4096);
                }

                ept_vec_push(&mut frames, new_pd_frame);

                // SAFETY: dst_pdpt points to a valid PDPT table and pdpt_idx is in
                // 0..511, so the write target is within the allocated page.
                unsafe {
                    *dst_pdpt.add(pdpt_idx) = EptEntry::table_entry_with_format(
                        new_pd_phys,
                        src_pdpte.permissions_with_format(self.format),
                        self.format,
                    );
                }

                let src_pd = allocator
                    .phys_to_virt(src_pdpte.addr())
                    .cast::<EptEntry>()
                    .cast_const();
                let dst_pd = new_pd_virt.cast::<EptEntry>();

                for pd_idx in 0..512 {
                    // SAFETY: src_pd points to a valid PD table and pd_idx is in 0..511,
                    // so the resulting pointer is within the 4KB page.
                    let src_pde = unsafe { &*src_pd.add(pd_idx) };
                    if !src_pde.is_present() {
                        continue;
                    }

                    let new_pt_frame = allocator.allocate_frame()?;
                    let new_pt_phys = A::frame_phys_addr(&new_pt_frame);
                    let new_pt_virt = allocator.phys_to_virt(new_pt_phys);
                    // SAFETY: new_pt_virt points to a freshly allocated 4KB-aligned frame,
                    // and we zero the entire page to initialize all PT entries.
                    unsafe {
                        core::ptr::write_bytes(new_pt_virt, 0, 4096);
                    }

                    ept_vec_push(&mut frames, new_pt_frame);

                    // SAFETY: dst_pd points to a valid PD table and pd_idx is in 0..511,
                    // so the write target is within the allocated page.
                    unsafe {
                        *dst_pd.add(pd_idx) = EptEntry::table_entry_with_format(
                            new_pt_phys,
                            src_pde.permissions_with_format(self.format),
                            self.format,
                        );
                    }

                    let src_pt = allocator
                        .phys_to_virt(src_pde.addr())
                        .cast::<EptEntry>()
                        .cast_const();
                    let dst_pt = new_pt_virt.cast::<EptEntry>();

                    for pt_idx in 0..512 {
                        // SAFETY: src_pt points to a valid PT table and pt_idx is in 0..511,
                        // so the resulting pointer is within the 4KB page.
                        let src_pte = unsafe { *src_pt.add(pt_idx) };
                        if !src_pte.is_present() {
                            continue;
                        }

                        let host_phys = src_pte.addr();
                        let new_entry = EptEntry::page_entry_with_format(
                            host_phys,
                            EptPermissions::READ_EXECUTE,
                            EptMemoryType::WriteBack,
                            self.format,
                        );

                        // SAFETY: dst_pt points to a valid, newly allocated PT table and
                        // pt_idx is in 0..511, so the write target is within the page.
                        unsafe {
                            *dst_pt.add(pt_idx) = new_entry;
                        }
                    }
                }
            }
        }

        Ok(Self {
            format: self.format,
            pml4_phys: new_pml4_phys,
            frames,
        })
    }
}
