// SPDX-License-Identifier: GPL-2.0
//! Bounded instruction sequences for AMD hardware execution.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
use super::super::traits::{CowAllocator, InstructionBatch, RepeatBatch};
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// Unknown instructions and control transfers terminate a batch. The flags
/// distinguish memory access from stores, including implicit stack accesses.
fn safe_len(bytes: &[u8], long: bool, default32: bool) -> Option<(usize, bool, bool)> {
    let mut p = 0;
    let mut operand_override = false;
    let mut address_override = false;
    let mut rex = 0;
    loop {
        let b = *bytes.get(p)?;
        if matches!(b, 0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67)
            || (long && (0x40..=0x4f).contains(&b))
        {
            operand_override |= b == 0x66;
            address_override |= b == 0x67;
            rex = if long && (0x40..=0x4f).contains(&b) {
                b
            } else {
                0
            };
            p += 1;
            if p >= 15 {
                return None;
            }
        } else {
            break;
        }
    }
    let operand = if (long && rex & 8 != 0) || ((default32 || long) ^ operand_override) {
        4
    } else {
        2
    };
    let op = *bytes.get(p)?;
    p += 1;
    let mut modrm = false;
    let mut addressing = false;
    let mut immediate = 0;
    let mut group = 0;
    let mut memory = false;
    let mut writes = false;
    match op {
        0x90..=0x99 | 0x9e | 0x9f | 0xf5 | 0xf8 | 0xf9 | 0xfc | 0xfd => {}
        0x40..=0x4f if !long => {}
        0x50..=0x5f | 0xa4..=0xa7 | 0xaa..=0xaf => {
            memory = true;
            writes = matches!(op, 0x50..=0x57 | 0xa4 | 0xa5 | 0xaa | 0xab);
        }
        0xa0..=0xa3 => {
            memory = true;
            writes = op >= 0xa2;
            immediate = if long {
                if address_override {
                    4
                } else {
                    8
                }
            } else if (default32 || long) ^ address_override {
                4
            } else {
                2
            };
        }
        0xb0..=0xb7 => immediate = 1,
        0xb8..=0xbf => immediate = if long && rex & 8 != 0 { 8 } else { operand },
        0x00..=0x3d if op & 7 <= 3 => modrm = true,
        0x00..=0x3d if op & 7 == 4 => immediate = 1,
        0x00..=0x3d if op & 7 == 5 => immediate = operand,
        0x80 | 0x83 | 0xc0 | 0xc1 => {
            modrm = true;
            immediate = 1;
        }
        0x81 => {
            modrm = true;
            immediate = operand;
        }
        0x69 | 0x6b => {
            modrm = true;
            immediate = if op == 0x6b { 1 } else { operand };
        }
        0x84..=0x8b | 0xd0..=0xd3 => modrm = true,
        0x8d => {
            modrm = true;
            addressing = true;
            group = 1;
        }
        0xc6 | 0xc7 => {
            modrm = true;
            group = 2;
            immediate = if op == 0xc6 { 1 } else { operand };
        }
        0xfe | 0xff => {
            modrm = true;
            group = 3;
        }
        0xf6 | 0xf7 => {
            modrm = true;
            group = 4;
        }
        0x0f => {
            let second = *bytes.get(p)?;
            p += 1;
            match second {
                0xc8..=0xcf => {}
                0x40..=0x4f | 0x90..=0x9f | 0xaf | 0xb6 | 0xb7 | 0xbe | 0xbf => modrm = true,
                0x1f => {
                    modrm = true;
                    addressing = true;
                    group = 2;
                }
                _ => return None,
            }
        }
        _ => return None,
    }
    if modrm {
        let m = *bytes.get(p)?;
        p += 1;
        let mode = m >> 6;
        let reg = (m >> 3) & 7;
        match group {
            1 if mode == 3 => return None, // LEA requires an address.
            2 if reg != 0 => return None,
            3 if reg > 1 => return None, // No call/jump/push.
            4 if reg == 1 => return None,
            4 if reg == 0 => immediate = if op == 0xf6 { 1 } else { operand },
            _ => {}
        }
        if mode != 3 {
            memory |= !addressing;
            writes = match op {
                0x0f => matches!(bytes[p - 2], 0x90..=0x9f),
                0x00..=0x3d => op < 0x38 && op & 7 <= 1,
                0x80 | 0x81 | 0x83 => reg != 7,
                0x86..=0x89 | 0xc0 | 0xc1 | 0xc6 | 0xc7 | 0xd0..=0xd3 | 0xfe | 0xff => true,
                0xf6 | 0xf7 => matches!(reg, 2 | 3),
                _ => false,
            };
            let rm = m & 7;
            let address16 = !long && (!default32 ^ address_override);
            if address16 {
                p += match mode {
                    0 if rm == 6 => 2,
                    1 => 1,
                    2 => 2,
                    _ => 0,
                };
            } else {
                if rm == 4 {
                    let sib = *bytes.get(p)?;
                    p += 1;
                    if mode == 0 && sib & 7 == 5 {
                        p += 4;
                    }
                }
                p += match mode {
                    0 if rm == 5 => 4,
                    1 => 1,
                    2 => 4,
                    _ => 0,
                };
            }
        }
    }
    p += immediate;
    (p <= 15 && p <= bytes.len()).then_some((p, memory, writes))
}

fn repeat_len(bytes: &[u8]) -> Option<usize> {
    let mut p = 0;
    let mut repeated = false;
    loop {
        let b = *bytes.get(p)?;
        match b {
            0xf3 => repeated = true,
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x40..=0x4f => {}
            0xa4 | 0xa5 | 0xaa | 0xab if repeated => return Some(p + 1),
            _ => return None, // No address-size override or conditional repetition.
        }
        p += 1;
        if p >= 15 {
            return None;
        }
    }
}

pub(crate) fn prepare<C: VmContext>(
    ctx: &C,
    can_loop: bool,
    window: &super::svm::InstructionWindow,
) -> Option<InstructionBatch> {
    let state = ctx.state();
    let v = &state.vmcs;
    let flags = v.read_natural(VmcsFieldNatural::GuestRflags).ok()?;
    if flags & ((1 << 8) | (1 << 16)) != 0
        || state.mtf_enabled
        || v.read_natural(VmcsFieldNatural::GuestDr7).ok()? & 0xff != 0
    {
        return None;
    }
    let cs = v.read32(VmcsField32::GuestCsAccessRights).ok()?;
    let long = cs & (1 << 13) != 0;
    let default32 = cs & (1 << 14) != 0;
    let rip = v.read_natural(VmcsFieldNatural::GuestRip).ok()?;
    let linear = rip.checked_add(v.read_natural(VmcsFieldNatural::GuestCsBase).ok()?)?;
    // A single code page is enough; fetching another page may itself fault.
    // Leave enough code bytes for the endpoint instruction to be fetched
    // without crossing an unvalidated page before its execution breakpoint.
    let mut available = (4096 - (linear & 4095) as usize)
        .saturating_sub(15)
        .min(256);
    if !long {
        let maximum = if default32 {
            u32::MAX as u64
        } else {
            u16::MAX as u64
        };
        available = available.min(maximum.checked_sub(rip)? as usize);
        let segment_remaining = u64::from(v.read32(VmcsField32::GuestCsLimit).ok()?)
            .checked_sub(rip)?
            .saturating_sub(14);
        available = available.min(segment_remaining as usize);
    }
    if window.linear != linear {
        return None;
    }
    let physical = window.physical;
    let paged = v.read_natural(VmcsFieldNatural::GuestCr0).ok()? & (1 << 31) != 0;
    let bytes = &window.bytes;

    let current = state.last_instruction_count + state.tsc_offset;
    let mut budget = u64::MAX;
    for target in [
        super::next_timer_exit_count(ctx).map(|n| n + state.tsc_offset),
        super::next_io_channel_target_tsc(ctx),
        state.stop_at_tsc,
        super::next_single_step_start_tsc(ctx),
    ]
    .into_iter()
    .flatten()
    {
        budget = budget.min(target.saturating_sub(current));
    }
    let limit = budget.min(4096) as usize;
    if limit < 2 {
        return None;
    }
    let mut batch = InstructionBatch {
        start: rip,
        offsets: [0; 65],
        count: 0,
        repeat: None,
        pages: [0; 5],
        page_count: 1,
        accesses_memory: false,
        writes_memory: false,
        looping: false,
        loop_start: 0,
        instruction_budget: budget,
    };
    batch.pages[0] = physical.as_u64() & !4095;
    let mut offset = 0;
    if long {
        if let Some(length) = repeat_len(&bytes[..available]) {
            let original_count = state.gprs.rcx;
            let iterations = original_count.min(limit as u64);
            if iterations < 2 {
                return None;
            }
            batch.repeat = Some(RepeatBatch {
                original_count,
                iterations,
            });
            batch.offsets[1] = length as u16;
            batch.count = 1;
            batch.accesses_memory = true;
        }
    }
    while batch.repeat.is_none() && batch.count < limit.min(64) {
        // One backward conditional branch stays entirely inside the verified
        // region. Its fall-through is the execution breakpoint. PMIs bound
        // execution, then TF handles the margin before a deterministic stop.
        if can_loop && budget >= InstructionBatch::LOOP_DEADLINE_MARGIN && batch.count != 0 {
            let tail = &bytes[offset..available];
            if let [opcode @ 0x70..=0x7f, displacement, ..] = tail {
                let end = offset + 2;
                let target = end as i64 + i64::from(*displacement as i8);
                if target >= 0
                    && target < offset as i64
                    && batch.offsets[..batch.count].contains(&(target as u16))
                {
                    batch.count += 1;
                    batch.offsets[batch.count] = end as u16;
                    batch.looping = true;
                    batch.loop_start = batch.offsets[..batch.count - 1]
                        .iter()
                        .position(|&p| p == target as u16)?;
                    break;
                }
                let _ = opcode;
            }
        }
        let Some((length, memory, writes)) = safe_len(&bytes[offset..available], long, default32)
        else {
            break;
        };
        // Protecting translation tables traps hardware A/D updates too.
        // Step paged stores directly instead of faulting and then stepping.
        if paged && writes {
            break;
        }
        offset += length;
        batch.accesses_memory |= memory;
        batch.writes_memory |= writes;
        batch.count += 1;
        batch.offsets[batch.count] = offset as u16;
        if offset == available {
            break;
        }
    }
    if batch.repeat.is_none() && batch.count < 2 {
        return None;
    }
    // Instruction fetch can set page-table A bits. Do not batch code that
    // aliases its own translation tables and could change while executing.
    if paged {
        let code_page = physical.as_u64() & !4095;
        let mut table = v.read_natural(VmcsFieldNatural::GuestCr3).ok()? & 0x000f_ffff_ffff_f000;
        for shift in [39, 30, 21, 12] {
            if table == code_page {
                return None;
            }
            batch.pages[batch.page_count] = table;
            batch.page_count += 1;
            let address = table + ((linear >> shift) & 511) * 8;
            let mut entry = [0u8; 8];
            ctx.read_guest_memory(GuestPhysAddr::new(address), &mut entry)
                .ok()?;
            let entry = u64::from_le_bytes(entry);
            if shift == 12 || entry & (1 << 7) != 0 {
                break;
            }
            table = entry & 0x000f_ffff_ffff_f000;
        }
    }
    if let Some(repeat) = batch.repeat {
        // REP stores can change neither code nor the active translation tree.
        // Verify every destination page for this bounded chunk before entry.
        let width = match bytes[batch.offsets[1] as usize - 1] {
            0xa4 | 0xaa => 1u64,
            _ => {
                if bytes[..batch.offsets[1] as usize]
                    .iter()
                    .any(|b| b & 0xf8 == 0x48)
                {
                    8
                } else if bytes[..batch.offsets[1] as usize].contains(&0x66) {
                    2
                } else {
                    4
                }
            }
        };
        let length = repeat.iterations.checked_mul(width)?;
        let start = if flags & (1 << 10) != 0 {
            state.gprs.rdi.checked_sub(length - width)?
        } else {
            state.gprs.rdi
        };
        let end = start.checked_add(length - 1)?;
        let mut page = start & !4095;
        let mut destinations = [0u64; 9];
        let mut destination_count = 0;
        loop {
            let destination = super::svm::physical(ctx, page).ok()?.as_u64() & !4095;
            if batch.pages[..batch.page_count].contains(&destination) {
                return None;
            }
            *destinations.get_mut(destination_count)? = destination;
            destination_count += 1;
            if page == end & !4095 {
                break;
            }
            page = page.checked_add(4096)?;
        }
        // A store must not rewrite its own translation and redirect a later
        // iteration into the code tree. Include destination tables that are
        // absent from the instruction-fetch walk.
        page = start & !4095;
        loop {
            let mut table =
                v.read_natural(VmcsFieldNatural::GuestCr3).ok()? & 0x000f_ffff_ffff_f000;
            for shift in [39, 30, 21, 12] {
                if destinations[..destination_count].contains(&table) {
                    return None;
                }
                let address = table + ((page >> shift) & 511) * 8;
                let mut entry = [0u8; 8];
                ctx.read_guest_memory(GuestPhysAddr::new(address), &mut entry)
                    .ok()?;
                let entry = u64::from_le_bytes(entry);
                if shift == 12 || entry & (1 << 7) != 0 {
                    break;
                }
                table = entry & 0x000f_ffff_ffff_f000;
            }
            if page == end & !4095 {
                break;
            }
            page = page.checked_add(4096)?;
        }
    }
    Some(batch)
}

pub(crate) struct BatchGuard {
    saved: [Option<(GuestPhysAddr, HostPhysAddr, EptPermissions)>; 5],
}

pub(crate) fn protect<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    batch: &InstructionBatch,
) -> Option<BatchGuard> {
    let mut guard = BatchGuard { saved: [None; 5] };
    if !batch.accesses_memory {
        return Some(guard);
    }
    let protected = if batch.writes_memory {
        batch.page_count
    } else {
        1
    };
    for (index, &page) in batch.pages[..protected].iter().enumerate() {
        let gpa = GuestPhysAddr::new(page);
        let Some((host, permissions)) = ctx.state().ept.lookup(allocator, gpa) else {
            guard.restore(ctx, allocator);
            return None;
        };
        if permissions.bits() & 2 == 0 {
            continue;
        }
        let read_only = EptPermissions::from_bits(permissions.bits() & !2);
        if ctx
            .state_mut()
            .ept
            .remap_4k(allocator, gpa, host, read_only, EptMemoryType::WriteBack)
            .is_err()
        {
            guard.restore(ctx, allocator);
            return None;
        }
        guard.saved[index] = Some((gpa, host, permissions));
    }
    Some(guard)
}

impl BatchGuard {
    pub(crate) fn restore<C: VmContext, A: CowAllocator<C::CowPage>>(
        self,
        ctx: &mut C,
        allocator: &A,
    ) -> bool {
        let v = &ctx.state().vmcs;
        let write_fault = v.read32(VmcsField32::VmExitReason).ok() == Some(48)
            && v.read_natural(VmcsFieldNatural::ExitQualification)
                .unwrap_or(0)
                & 2
                != 0;
        let fault_page = v.read64(VmcsField64::GuestPhysicalAddr).unwrap_or(0) & !4095;
        let retry_single = write_fault
            && self
                .saved
                .iter()
                .flatten()
                .any(|(gpa, _, _)| gpa.as_u64() == fault_page);
        for (gpa, host, permissions) in self.saved.into_iter().flatten() {
            // Entries cannot disappear while the VM is stopped. Preserve the
            // original COW permission rather than promoting a shared page.
            ctx.state_mut()
                .ept
                .remap_4k(allocator, gpa, host, permissions, EptMemoryType::WriteBack)
                .expect("SVM batch mapping disappeared");
        }
        retry_single
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_encodings_match_independent_decoder() {
        use iced_x86::{Decoder, DecoderOptions, FlowControl, InstructionInfoFactory, OpAccess};
        let mut factory = InstructionInfoFactory::new();
        let mut random = 0x2a1e_76d3_598b_f004u64;
        for bitness in [16, 32, 64] {
            for case in 0..50_000 {
                let mut bytes = [0u8; 15];
                for byte in &mut bytes {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    *byte = random as u8;
                }
                match case % 6 {
                    0 => bytes[0] = 0x66,
                    1 => bytes[0] = 0x67,
                    2 => bytes[..2].copy_from_slice(&[0x66, 0x48]),
                    3 => bytes[..2].copy_from_slice(&[0x48, 0x66]),
                    4 => bytes[0] = 0x0f,
                    _ => {}
                }
                let Some((length, memory, writes)) = safe_len(&bytes, bitness == 64, bitness == 32)
                else {
                    continue;
                };
                let instruction = Decoder::new(bitness, &bytes, DecoderOptions::NONE).decode();
                // Invalid encodings fault before execution; they cannot skip
                // past the endpoint or execute an unverified instruction.
                if instruction.is_invalid() {
                    continue;
                }
                assert_eq!(
                    length,
                    instruction.len(),
                    "bitness={bitness} bytes={bytes:02x?}"
                );
                assert_eq!(
                    instruction.flow_control(),
                    FlowControl::Next,
                    "bytes={bytes:02x?}"
                );
                let info = factory.info(&instruction);
                let uses_memory = !info.used_memory().is_empty();
                let stores = info.used_memory().iter().any(|m| {
                    matches!(
                        m.access(),
                        OpAccess::Write
                            | OpAccess::CondWrite
                            | OpAccess::ReadWrite
                            | OpAccess::ReadCondWrite
                    )
                });
                assert_eq!(
                    (memory, writes),
                    (uses_memory, stores),
                    "bitness={bitness} bytes={bytes:02x?}"
                );
            }
        }
    }
    #[test]
    fn distinguishes_reads_from_stores() {
        for (instruction, writes) in [
            (&[0x48, 0x8b, 0x07][..], false), // mov rax,[rdi]
            (&[0x48, 0x89, 0x07], true),      // mov [rdi],rax
            (&[0x80, 0x3f, 0], false),        // cmp byte [rdi],0
            (&[0x80, 0x07, 1], true),         // add byte [rdi],1
            (&[0x0f, 0x94, 0x07], true),      // sete [rdi]
            (&[0x0f, 0xb6, 0x07], false),     // movzx eax,byte [rdi]
            (&[0x50], true),
            (&[0x58], false),
            (&[0xa4], true),
            (&[0xac], false),
        ] {
            assert_eq!(
                safe_len(instruction, true, false),
                Some((instruction.len(), true, writes))
            );
        }
    }

    #[test]
    fn branch_counter_accounts_for_prefix_partial_iteration_and_fallthrough() {
        let mut batch = InstructionBatch {
            start: 0x1000,
            offsets: [0; 65],
            count: 4,
            repeat: None,
            pages: [0; 5],
            page_count: 0,
            accesses_memory: false,
            writes_memory: false,
            looping: true,
            loop_start: 1,
            instruction_budget: u64::MAX,
        };
        batch.offsets[..5].copy_from_slice(&[0, 3, 4, 5, 7]);
        assert_eq!(batch.loop_period(), 1 << 30);
        batch.instruction_budget = InstructionBatch::LOOP_DEADLINE_MARGIN + 301;
        assert_eq!(batch.loop_period(), 100);
        batch.instruction_budget = InstructionBatch::LOOP_DEADLINE_MARGIN;
        assert_eq!(batch.loop_period(), 1);
        assert_eq!(batch.loop_instructions(0, 0x1000), Some(0));
        assert_eq!(batch.loop_instructions(0, 0x1005), Some(3));
        assert_eq!(batch.loop_instructions(10, 0x1004), Some(32));
        assert_eq!(batch.loop_instructions(10, 0x1007), Some(31));
        assert_eq!(batch.loop_instructions(0, 0x1007), None);
        assert_eq!(batch.loop_instructions(1, 0x1002), None);
        assert_eq!(batch.loop_instructions(u64::MAX, 0x1007), None);
    }
    #[test]
    fn stops_before_randomness_flags_memory_and_control_flow() {
        for instruction in [
            &[0x0f, 0xc7, 0xf0][..],
            &[0x0f, 0xc7, 0xf8],
            &[0x0f, 0x05],
            &[0x9c],
            &[0x9d],
            &[0xff, 0xd0],
            &[0xf3, 0xa4],
            &[0xeb, 0],
            &[0x0f, 0x31],
            &[0xfa],
            &[0xfb],
        ] {
            assert_eq!(safe_len(instruction, true, false), None);
        }
    }
    #[test]
    fn decodes_register_operations_and_address_only_operands() {
        for (instruction, length) in [
            (&[0x90][..], 1),
            (&[0x48, 0x31, 0xc0], 3),
            (&[0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0], 10),
            (&[0x48, 0x8d, 0x44, 0x24, 8], 5),
            (&[0x0f, 0x1f, 0x84, 0, 0, 0, 0, 0], 8),
            (&[0x66, 0x81, 0xc0, 1, 0], 5),
        ] {
            assert_eq!(
                safe_len(instruction, true, false),
                Some((length, false, false))
            );
        }
        assert_eq!(safe_len(&[0x48, 0xb8, 0], true, false), None);
        assert_eq!(safe_len(&[0x66; 15], true, false), None);
    }
    #[test]
    fn partial_count_requires_a_known_boundary() {
        let mut b = InstructionBatch {
            start: 0x1000,
            offsets: [0; 65],
            count: 2,
            repeat: None,
            pages: [0; 5],
            page_count: 0,
            accesses_memory: false,
            writes_memory: false,
            looping: false,
            loop_start: 0,
            instruction_budget: u64::MAX,
        };
        b.offsets[1] = 3;
        b.offsets[2] = 8;
        assert_eq!(b.completed_at(0x1003), Some(1));
        assert_eq!(b.completed_at(0x1008), Some(2));
        assert_eq!(b.completed_at(0x1004), None);
    }
}
