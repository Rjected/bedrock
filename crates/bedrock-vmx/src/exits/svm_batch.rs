// SPDX-License-Identifier: GPL-2.0
//! Bounded instruction sequences for AMD hardware execution.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
use super::super::traits::{CowAllocator, InstructionBatch, RepeatBatch};
#[cfg(not(feature = "cargo"))]
use crate::ept::NptExecutionGuard;
#[cfg(feature = "cargo")]
use crate::prelude::*;
#[cfg(feature = "cargo")]
use bedrock_ept::NptExecutionGuard;

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

fn relative_branch(bytes: &[u8], long: bool, default32: bool) -> Option<(usize, i64)> {
    let first = *bytes.first()?;
    if matches!(first, 0x70..=0x7f | 0xeb) {
        return Some((2, i64::from(*bytes.get(1)? as i8)));
    }
    let opcode_length = if first == 0xe9 {
        1
    } else if first == 0x0f && matches!(*bytes.get(1)?, 0x80..=0x8f) {
        2
    } else {
        return None;
    };
    let width = if long || default32 { 4 } else { 2 };
    let displacement = bytes.get(opcode_length..opcode_length + width)?;
    let displacement = if width == 4 {
        i64::from(i32::from_le_bytes(displacement.try_into().ok()?))
    } else {
        i64::from(i16::from_le_bytes(displacement.try_into().ok()?))
    };
    Some((opcode_length + width, displacement))
}

fn endpoint_intercepted(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        [0xf4, ..] | [0x0f, 0xa2 | 0x31, ..] | [0x0f, 0x01, 0xd9 | 0xf9, ..]
    )
}

fn opcode_start(bytes: &[u8], long: bool) -> Option<(usize, u8, bool)> {
    let mut p = 0;
    let mut rex = 0;
    let mut word = false;
    loop {
        let byte = *bytes.get(p)?;
        if matches!(byte, 0x26 | 0x2e | 0x36 | 0x3e | 0x64..=0x67)
            || (long && matches!(byte, 0x40..=0x4f))
        {
            word |= byte == 0x66;
            rex = if long && matches!(byte, 0x40..=0x4f) {
                byte
            } else {
                0
            };
            p += 1;
        } else {
            return Some((p, rex, word));
        }
    }
}

/// Over-approximate GPR writes; unknown operations invalidate every address
/// register. Subregister writes invalidate the whole architectural register.
fn modified_gprs(bytes: &[u8], long: bool) -> u16 {
    let Some((p, rex, _)) = opcode_start(bytes, long) else {
        return u16::MAX;
    };
    let op = bytes[p];
    let bit = |register: u8, byte: bool| {
        let register = if byte && rex == 0 && (4..8).contains(&register) {
            register - 4
        } else {
            register
        };
        1u16 << register
    };
    let destination = |byte: bool| -> Option<u16> {
        let m = *bytes.get(p + 1)?;
        Some(if m >> 6 == 3 {
            bit((m & 7) | ((rex & 1) << 3), byte)
        } else {
            0
        })
    };
    let register = |byte: bool| -> Option<u16> {
        Some(bit(
            ((*bytes.get(p + 1)? >> 3) & 7) | ((rex & 4) << 1),
            byte,
        ))
    };
    match op {
        0x90 if rex & 1 == 0 => 0,
        0x38..=0x3d | 0x84 | 0x85 | 0x9e | 0xf5 | 0xf8 | 0xf9 | 0xfc | 0xfd => 0,
        0x50..=0x57 => 1 << 4,
        0x58..=0x5f => (1 << 4) | bit((op & 7) | ((rex & 1) << 3), false),
        0xb0..=0xbf => bit((op & 7) | ((rex & 1) << 3), op < 0xb8),
        0x88 | 0x89 | 0xc6 | 0xc7 | 0xc0 | 0xc1 | 0xd0..=0xd3 | 0xfe | 0xff => {
            destination(matches!(op, 0x88 | 0xc6 | 0xc0 | 0xd0 | 0xd2 | 0xfe)).unwrap_or(u16::MAX)
        }
        0x8a | 0x8b | 0x8d | 0x69 | 0x6b => register(op == 0x8a).unwrap_or(u16::MAX),
        0x00..=0x35 if op & 7 <= 3 => if op & 2 == 0 {
            destination(op & 1 == 0)
        } else {
            register(op & 1 == 0)
        }
        .unwrap_or(u16::MAX),
        0x00..=0x35 if matches!(op & 7, 4 | 5) => 1,
        0x80 | 0x81 | 0x83 => {
            if bytes.get(p + 1).is_some_and(|m| (m >> 3) & 7 == 7) {
                0
            } else {
                destination(op == 0x80).unwrap_or(u16::MAX)
            }
        }
        0x0f if bytes.get(p + 1) == Some(&0x1f) => 0,
        _ => u16::MAX,
    }
}

/// Address of a MOV store or PUSH whose address registers retain their entry
/// values. Other stores and address/FS/GS overrides terminate the batch.
fn store_range(bytes: &[u8], rip: u64, gprs: &[u64; 16], changed: u16) -> Option<(u64, u64)> {
    let (p, rex, word) = opcode_start(bytes, true)?;
    if bytes[..p].iter().any(|b| matches!(b, 0x64 | 0x65 | 0x67)) {
        return None;
    }
    let op = bytes[p];
    if matches!(op, 0x50..=0x57) {
        if changed & (1 << 4) != 0 {
            return None;
        }
        let width = if word && rex & 8 == 0 { 2 } else { 8 };
        return Some((gprs[4].checked_sub(width)?, width));
    }
    if !matches!(op, 0x88 | 0x89 | 0xc6 | 0xc7) {
        return None;
    }
    let width = if matches!(op, 0x88 | 0xc6) {
        1
    } else if rex & 8 != 0 {
        8
    } else if word {
        2
    } else {
        4
    };
    let m = *bytes.get(p + 1)?;
    let mode = m >> 6;
    if mode == 3 {
        return None;
    }
    let mut cursor = p + 2;
    let mut address = 0u64;
    let mut registers = 0u16;
    let mut no_base = false;
    let mut relative = false;
    if m & 7 == 4 {
        let sib = *bytes.get(cursor)?;
        cursor += 1;
        let index = ((sib >> 3) & 7) | ((rex & 2) << 2);
        if index != 4 {
            registers |= 1 << index;
            address = gprs[index as usize].wrapping_shl((sib >> 6) as u32);
        }
        no_base = mode == 0 && sib & 7 == 5;
        if !no_base {
            let base = (sib & 7) | ((rex & 1) << 3);
            registers |= 1 << base;
            address = address.wrapping_add(gprs[base as usize]);
        }
    } else if mode == 0 && m & 7 == 5 {
        relative = true;
        no_base = true;
    } else {
        let base = (m & 7) | ((rex & 1) << 3);
        registers |= 1 << base;
        address = gprs[base as usize];
    }
    if changed & registers != 0 {
        return None;
    }
    let displacement = if mode == 1 {
        i64::from(*bytes.get(cursor)? as i8)
    } else if mode == 2 || no_base {
        i64::from(i32::from_le_bytes(
            bytes.get(cursor..cursor + 4)?.try_into().ok()?,
        ))
    } else {
        0
    };
    if relative {
        address = rip.checked_add(bytes.len() as u64)?;
    }
    Some((address.wrapping_add(displacement as u64), width))
}

fn collect_code_tables<C: VmContext>(
    ctx: &C,
    batch: &mut InstructionBatch,
    linear: u64,
) -> Option<()> {
    if batch.page_count != 1 {
        return Some(());
    }
    let mut table = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestCr3)
        .ok()?
        & 0x000f_ffff_ffff_f000;
    for shift in [39, 30, 21, 12] {
        if table == batch.pages[0] {
            return None;
        }
        batch.pages[batch.page_count] = table;
        batch.page_count += 1;
        let mut entry = [0u8; 8];
        ctx.read_guest_memory(
            GuestPhysAddr::new(table + ((linear >> shift) & 511) * 8),
            &mut entry,
        )
        .ok()?;
        let entry = u64::from_le_bytes(entry);
        if shift == 12 || entry & (1 << 7) != 0 {
            break;
        }
        table = entry & 0x000f_ffff_ffff_f000;
    }
    Some(())
}

#[derive(Default)]
struct StorePlan {
    destinations: [u64; 8],
    count: usize,
    last_page: Option<u64>,
}

fn validate_store<C: VmContext>(
    ctx: &C,
    batch: &InstructionBatch,
    plan: &mut StorePlan,
    start: u64,
    width: u64,
) -> Option<()> {
    let end = start.checked_add(width - 1)? & !4095;
    let mut page = start & !4095;
    loop {
        // Consecutive stores to one virtual page cannot change its proved
        // translation: that page is disjoint from its own page tables. A
        // different destination invalidates this cache before it can write.
        if plan.last_page == Some(page) {
            if page == end {
                break;
            }
            page = page.checked_add(4096)?;
            continue;
        }
        let destination = super::svm::physical(ctx, page).ok()?.as_u64() & !4095;
        if batch.pages[..batch.page_count].contains(&destination) {
            return None;
        }
        if !plan.destinations[..plan.count].contains(&destination) {
            *plan.destinations.get_mut(plan.count)? = destination;
            plan.count += 1;
        }
        let mut table = ctx
            .state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestCr3)
            .ok()?
            & 0x000f_ffff_ffff_f000;
        for shift in [39, 30, 21, 12] {
            // Earlier stores cannot rewrite a later store's translation,
            // including the current destination's own page tables.
            if plan.destinations[..plan.count].contains(&table) {
                return None;
            }
            let mut entry = [0u8; 8];
            ctx.read_guest_memory(
                GuestPhysAddr::new(table + ((page >> shift) & 511) * 8),
                &mut entry,
            )
            .ok()?;
            let entry = u64::from_le_bytes(entry);
            if shift == 12 || entry & (1 << 7) != 0 {
                break;
            }
            table = entry & 0x000f_ffff_ffff_f000;
        }
        plan.last_page = Some(page);
        if page == end {
            break;
        }
        page = page.checked_add(4096)?;
    }
    Some(())
}

fn forbidden_page_bytes(bytes: &[u8]) -> bool {
    for start in 0..bytes.len() {
        // RDRAND/RDSEED/RDPID have no general SVM interception. Checking
        // their opcode at every byte also covers overlapping code and data.
        if bytes.get(start..start + 2) == Some(&[0x0f, 0xc7])
            && bytes.get(start + 2).is_some_and(|b| b & 0xf0 == 0xf0)
        {
            return true;
        }
        // SYSRET can restore TF from R11 without an SVM intercept. Preserve
        // architectural debug-trap accounting by using the scalar backend.
        if bytes.get(start..start + 2) == Some(&[0x0f, 0x07]) {
            return true;
        }
        if !matches!(bytes[start], 0xf2 | 0xf3) {
            continue;
        }
        for &byte in bytes.iter().skip(start + 1).take(14) {
            if matches!(byte, 0xa4..=0xa7 | 0xaa..=0xaf) {
                return true;
            }
            if !matches!(
                byte,
                0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0x40..=0x4f | 0xf2 | 0xf3
            ) {
                break;
            }
        }
    }
    false
}

fn page_safe<C: VmContext>(ctx: &C, physical: u64) -> Option<()> {
    // Stream the scan to avoid a 4KB buffer on the 8KB kernel stack. Carry
    // enough bytes to recognize any legal prefix chain between chunks.
    let mut bytes = [0u8; 272];
    let mut first = [0u8; 16];
    for offset in (0..4096).step_by(256) {
        ctx.read_guest_memory(GuestPhysAddr::new(physical + offset), &mut bytes[16..])
            .ok()?;
        if offset == 0 {
            first.copy_from_slice(&bytes[16..32]);
        }
        if forbidden_page_bytes(&bytes) {
            return None;
        }
        bytes.copy_within(256..272, 0);
    }
    bytes[16..32].copy_from_slice(&first);
    // A guest can map this same physical page at adjacent virtual addresses.
    (!forbidden_page_bytes(&bytes[..32])).then_some(())
}

// A counted MOV-store loop can use entry-time range validation when RDI
// advances once per iteration and RCX decreases once before JNZ. Other writes
// or control transfers require the ordinary conservative planner.
fn counted_store_loop<C: VmContext>(
    ctx: &C,
    bytes: &[u8],
    gprs: &[u64; 16],
    batch: &mut InstructionBatch,
) -> Option<()> {
    if gprs[1] == 0 {
        return None;
    }
    let mut candidate = *batch;
    let mut offset = 0;
    let mut decrement = false;
    let mut stride = None;
    let mut first = u64::MAX;
    let mut last = 0;
    while candidate.count < 64 {
        let tail = bytes.get(offset..)?;
        if tail.starts_with(&[0x75]) || tail.starts_with(&[0x0f, 0x85]) {
            let (length, displacement) = relative_branch(tail, true, false)?;
            if offset as i64 + length as i64 + displacement != 0
                || !decrement
                || stride.is_none()
                || first == u64::MAX
            {
                return None;
            }
            offset += length;
            candidate.count += 1;
            candidate.offsets[candidate.count] = offset as u16;
            break;
        }
        let (length, _, writes) = safe_len(tail, true, false)?;
        let instruction = &tail[..length];
        if instruction == [0x48, 0xff, 0xc9] && !decrement {
            decrement = true;
        } else if instruction.len() == 4
            && instruction[..3] == [0x48, 0x8d, 0x7f]
            && instruction[3] > 0
            && instruction[3] < 128
            && stride.is_none()
        {
            stride = Some(u64::from(instruction[3]));
        } else if writes && stride.is_none() {
            let (p, rex, _) = opcode_start(instruction, true)?;
            if !matches!(instruction[p], 0x88 | 0x89 | 0xc6 | 0xc7)
                || rex & 1 != 0
                || instruction.get(p + 1)? & 7 != 7
            {
                return None;
            }
            let (address, width) =
                store_range(instruction, candidate.start + offset as u64, gprs, 0)?;
            let displacement = address.checked_sub(gprs[7])?;
            first = first.min(displacement);
            last = last.max(displacement.checked_add(width)?);
        } else {
            return None;
        }
        offset += length;
        candidate.count += 1;
        candidate.offsets[candidate.count] = offset as u16;
    }
    let stride = stride?;
    if candidate.count == 64 || last > stride {
        return None;
    }
    let start = gprs[7].checked_add(first)?;
    let end = gprs[7]
        .checked_add(gprs[1].checked_sub(1)?.checked_mul(stride)?)?
        .checked_add(last.checked_sub(1)?)?;
    validate_contiguous_store_range(ctx, &candidate, start, end)?;
    candidate.uses_counter = true;
    candidate.counter_bounded = false;
    candidate.accesses_memory = true;
    candidate.writes_memory = true;
    candidate.validated_stores = true;
    candidate.endpoint_intercepted = endpoint_intercepted(bytes.get(offset..)?);
    *batch = candidate;
    Some(())
}

fn validate_contiguous_store_range<C: VmContext>(
    ctx: &C,
    batch: &InstructionBatch,
    start: u64,
    end: u64,
) -> Option<()> {
    let first_page = start & !4095;
    let last_page = end & !4095;
    if last_page.checked_sub(first_page)? / 4096 >= 4096 {
        return None;
    }
    let physical_start = super::svm::physical(ctx, first_page).ok()?.as_u64() & !4095;
    let physical_end = physical_start
        .checked_add(last_page - first_page)?
        .checked_add(4095)?;
    let overlaps = |page| page >= physical_start && page <= physical_end;
    if batch.pages[..batch.page_count]
        .iter()
        .any(|&page| overlaps(page))
    {
        return None;
    }
    let mut page = first_page;
    loop {
        if super::svm::physical(ctx, page).ok()?.as_u64() & !4095
            != physical_start.checked_add(page - first_page)?
        {
            return None;
        }
        let mut table = ctx
            .state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestCr3)
            .ok()?
            & 0x000f_ffff_ffff_f000;
        for shift in [39, 30, 21, 12] {
            // Stores cannot redirect their own or a later iteration's walks.
            if overlaps(table) || table == batch.pages[0] {
                return None;
            }
            let mut entry = [0u8; 8];
            ctx.read_guest_memory(
                GuestPhysAddr::new(table + ((page >> shift) & 511) * 8),
                &mut entry,
            )
            .ok()?;
            let entry = u64::from_le_bytes(entry);
            if shift == 12 || entry & (1 << 7) != 0 {
                break;
            }
            table = entry & 0x000f_ffff_ffff_f000;
        }
        if page == last_page {
            break;
        }
        page = page.checked_add(4096)?;
    }
    Some(())
}

pub(crate) fn prepare<C: VmContext>(
    ctx: &mut C,
    can_loop: bool,
    window: &super::svm::InstructionWindow,
) -> Option<InstructionBatch> {
    let page = window.physical.as_u64() & !4095;
    let mut allow_page = false;
    let v = &ctx.state().vmcs;
    let long = v.read32(VmcsField32::GuestCsAccessRights).ok()? & (1 << 13) != 0;
    if long
        && can_loop
        && instruction_budget(ctx) > InstructionBatch::COUNTER_DEADLINE_MARGIN
        && !ctx.state().svm_rejected_pages.contains(&page)
    {
        allow_page = page_safe(ctx, page).is_some();
        if !allow_page {
            let state = ctx.state_mut();
            state.svm_rejected_pages[state.svm_rejected_cursor] = page;
            state.svm_rejected_cursor = (state.svm_rejected_cursor + 1) % 64;
        }
    }
    prepare_verified(ctx, can_loop, allow_page, window)
}

fn instruction_budget<C: VmContext>(ctx: &C) -> u64 {
    let state = ctx.state();
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
    budget
}

fn prepare_verified<C: VmContext>(
    ctx: &C,
    can_loop: bool,
    allow_page: bool,
    window: &super::svm::InstructionWindow,
) -> Option<InstructionBatch> {
    let state = ctx.state();
    let v = &state.vmcs;
    let flags = v.read_natural(VmcsFieldNatural::GuestRflags).ok()?;
    if flags & ((1 << 8) | (1 << 16)) != 0
        || state.mtf_enabled
        || v.read_natural(VmcsFieldNatural::GuestDr7).ok()? & 0x20ff != 0
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

    let budget = instruction_budget(ctx);
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
        validated_stores: false,
        uses_counter: false,
        counter_bounded: true,
        page_execution: false,
        endpoint_intercepted: false,
        instruction_budget: budget,
    };
    batch.pages[0] = physical.as_u64() & !4095;
    if long && allow_page && can_loop && budget > InstructionBatch::COUNTER_DEADLINE_MARGIN {
        batch.page_execution = true;
        batch.uses_counter = true;
        batch.counter_bounded = false;
        batch.accesses_memory = true;
        batch.writes_memory = true;
        return Some(batch);
    }

    let g = &state.gprs;
    let gprs = [
        g.rax,
        g.rcx,
        g.rdx,
        g.rbx,
        v.read_natural(VmcsFieldNatural::GuestRsp).ok()?,
        g.rbp,
        g.rsi,
        g.rdi,
        g.r8,
        g.r9,
        g.r10,
        g.r11,
        g.r12,
        g.r13,
        g.r14,
        g.r15,
    ];
    if long && paged && can_loop && budget > InstructionBatch::COUNTER_DEADLINE_MARGIN {
        let mut candidate = batch;
        collect_code_tables(ctx, &mut candidate, linear)?;
        if counted_store_loop(ctx, &bytes[..available], &gprs, &mut candidate).is_some() {
            return Some(candidate);
        }
        // Failed loop recognition leaves the collected translation frames
        // intact, so the ordinary planner need not walk them again.
        batch = candidate;
    }
    let mut changed = 0u16;
    let mut stores = StorePlan::default();
    let mut offset = 0;
    let mut branch_targets = [0u16; 64];
    let mut branch_count = 0;
    let mut first_branch = 0;
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
        // Relative transfers may only enter decoded boundaries or the
        // endpoint. The retired-instruction counter accounts for their paths.
        if can_loop {
            let tail = &bytes[offset..available];
            if let Some((length, displacement)) = relative_branch(tail, long, default32) {
                let end = offset + length;
                let target = end as i64 + displacement;
                let backward = target < end as i64;
                if target < 0
                    || target > available as i64
                    || (backward
                        && (budget < InstructionBatch::COUNTER_DEADLINE_MARGIN
                            || (paged && batch.writes_memory)))
                {
                    break;
                }
                if branch_count == 0 {
                    first_branch = batch.count;
                }
                branch_targets[branch_count] = target as u16;
                branch_count += 1;
                offset = end;
                batch.count += 1;
                batch.offsets[batch.count] = end as u16;
                batch.uses_counter = true;
                batch.counter_bounded &= !backward;
                if offset == available {
                    break;
                }
                continue;
            }
        }
        let Some((length, memory, writes)) = safe_len(&bytes[offset..available], long, default32)
        else {
            break;
        };
        if paged && writes {
            if !long || (batch.uses_counter && !batch.counter_bounded) {
                break;
            }
            let instruction = &bytes[offset..offset + length];
            let Some((start, width)) =
                store_range(instruction, rip + offset as u64, &gprs, changed)
            else {
                break;
            };
            collect_code_tables(ctx, &mut batch, linear)?;
            if validate_store(ctx, &batch, &mut stores, start, width).is_none() {
                break;
            }
            batch.validated_stores = true;
        }
        changed |= modified_gprs(&bytes[offset..offset + length], long);
        offset += length;
        batch.accesses_memory |= memory;
        batch.writes_memory |= writes;
        batch.count += 1;
        batch.offsets[batch.count] = offset as u16;
        if offset == available {
            break;
        }
    }
    if batch.uses_counter
        && branch_targets[..branch_count]
            .iter()
            .any(|target| !batch.offsets[..=batch.count].contains(target))
    {
        // An unknown instruction or boundary ends verification. Keep the
        // straight-line prefix before the first transfer as an ordinary batch.
        batch.count = first_branch;
        batch.uses_counter = false;
    }
    if batch.repeat.is_none() && batch.count < 2 {
        return None;
    }
    // Instruction fetch can set page-table A bits. Do not batch code that
    // aliases its own translation tables and could change while executing.
    if paged {
        collect_code_tables(ctx, &mut batch, linear)?;
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
    batch.endpoint_intercepted =
        endpoint_intercepted(&bytes[batch.offsets[batch.count] as usize..available]);
    Some(batch)
}

pub(crate) struct BatchGuard {
    saved: [Option<(GuestPhysAddr, HostPhysAddr, EptPermissions)>; 5],
    execution: Option<NptExecutionGuard>,
}

pub(crate) fn protect<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    batch: &InstructionBatch,
) -> Option<BatchGuard> {
    let mut guard = BatchGuard {
        saved: [None; 5],
        execution: None,
    };
    if batch.page_execution {
        guard.execution = Some(
            ctx.state_mut()
                .ept
                .restrict_execution_to_page(allocator, GuestPhysAddr::new(batch.pages[0]))?,
        );
        return Some(guard);
    }
    if !batch.accesses_memory {
        return Some(guard);
    }
    let protected = if batch.writes_memory && !batch.validated_stores {
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
        if let Some(execution) = self.execution {
            execution.restore(&mut ctx.state_mut().ept, allocator);
            // The fast exit is a synthetic boundary. Replay its instruction
            // once with stepping and unrestricted execute permissions.
            return true;
        }
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
    use crate::tests::MockVmContext;

    #[test]
    fn naturally_trapped_endpoints_exclude_bitmap_dependent_msrs_and_rng() {
        for instruction in [
            &[0xf4][..],
            &[0x0f, 0xa2],
            &[0x0f, 0x31],
            &[0x0f, 0x01, 0xf9],
            &[0x0f, 0x01, 0xd9],
        ] {
            assert!(endpoint_intercepted(instruction));
        }
        for instruction in [
            &[0x0f, 0x30][..],
            &[0x0f, 0x32],
            &[0x0f, 0xc7, 0xf0],
            &[0xc3],
            &[0x90],
            &[0x0f],
            &[],
        ] {
            assert!(!endpoint_intercepted(instruction));
        }
    }

    fn paged_context(code: &[u8]) -> MockVmContext {
        let mut ctx = MockVmContext::new();
        ctx.set_guest_rip(0x1000);
        ctx.set_guest_rflags(2);
        let v = ctx.vmcs_setup();
        v.set_field_natural(VmcsFieldNatural::GuestCr0, 1 << 31);
        v.set_field_natural(VmcsFieldNatural::GuestCr3, 0x3000);
        v.set_field_natural(VmcsFieldNatural::GuestCsBase, 0);
        v.set_field_natural(VmcsFieldNatural::GuestRsp, 0x8000);
        v.set_field_natural(VmcsFieldNatural::GuestDr7, 0x400);
        v.set_field32(VmcsField32::GuestCsAccessRights, 1 << 13);
        v.write64(VmcsField64::GuestIa32Efer, 1 << 10).unwrap();
        for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x6007)] {
            ctx.memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        for page in 0..16 {
            ctx.memory[0x6000 + page * 8..0x6008 + page * 8]
                .copy_from_slice(&(page as u64 * 4096 + 7).to_le_bytes());
        }
        ctx.memory[0x1000..0x1000 + code.len()].copy_from_slice(code);
        ctx.state_mut().gprs.rdi = 0x7000;
        ctx
    }

    fn planned(ctx: &MockVmContext) -> Option<InstructionBatch> {
        let window = super::super::svm::InstructionWindow::read(ctx).unwrap();
        prepare_verified(ctx, true, false, &window)
    }

    #[test]
    fn paged_stores_require_stable_addresses_and_disjoint_translation_pages() {
        let code = [0x48, 0x89, 0x07, 0x48, 0x89, 0x47, 8, 0x0f, 0xc7, 0xf0];
        let mut ctx = paged_context(&code);
        let b = planned(&ctx).unwrap();
        assert_eq!(b.count, 2);
        assert!(b.validated_stores && b.writes_memory);
        for destination in [0x1000, 0x3000, 0x6000] {
            ctx.state_mut().gprs.rdi = destination;
            assert!(planned(&ctx).is_none());
        }
        let ctx = paged_context(&[0x48, 0x83, 0xc7, 8, 0x48, 0x89, 0x07, 0x0f, 0xc7, 0xf0]);
        assert!(planned(&ctx).is_none());
        let ctx = paged_context(&[0x48, 0x89, 0x07, 0x48, 0x89, 0x47, 8, 0x75, 0xf7]);
        assert!(!planned(&ctx).unwrap().uses_counter);
    }

    #[test]
    fn earlier_store_cannot_redirect_a_later_store() {
        let mut ctx = paged_context(&[0x48, 0x89, 0x06, 0x90, 0x48, 0x89, 0x1f, 0x0f, 0xc7, 0xf0]);
        for (address, entry) in [
            (0x3008, 0x8007u64),
            (0x8000, 0x9007),
            (0x9000, 0xa007),
            (0xa000, 0xb007),
        ] {
            ctx.memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        ctx.state_mut().gprs.rsi = 0xa000;
        ctx.state_mut().gprs.rdi = 1 << 39;
        let b = planned(&ctx).unwrap();
        assert_eq!(b.count, 2); // First store and NOP, stopping before the redirected store.
        assert!(b.validated_stores);
    }

    #[test]
    fn counter_regions_accept_multiple_branches_only_to_decoded_boundaries() {
        let ctx = paged_context(&[0x90, 0x74, 3, 0x90, 0xeb, 0xfa, 0x90, 0x0f, 0xc7, 0xf0]);
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter);
        assert_eq!(&b.offsets[..=b.count], &[0, 1, 3, 4, 6, 7]);
        let ctx = paged_context(&[0x90, 0x90, 0x74, 1, 0x48, 0x89, 0xc0, 0x0f, 0xc7, 0xf0]);
        let b = planned(&ctx).unwrap();
        assert!(!b.uses_counter);
        assert_eq!(b.count, 2); // Target enters the middle of MOV.
        let ctx = paged_context(&[0x90, 0x90, 0xeb, 0x7f, 0x0f, 0xc7, 0xf0]);
        let b = planned(&ctx).unwrap();
        assert!(!b.uses_counter);
        assert_eq!(b.count, 2);
        let ctx = paged_context(&[0x90, 0x90, 0x74, 3, 0x48, 0x89, 0x07, 0x0f, 0xc7, 0xf0]);
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter && b.counter_bounded && b.validated_stores);
    }

    #[test]
    fn counted_store_loops_prove_every_destination_and_translation() {
        let code = [
            0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f, 8, 0x48, 0xff, 0xc9, 0x75, 0xf4, 0x0f, 0x01, 0xd9,
        ];
        let mut ctx = paged_context(&code);
        ctx.state_mut().gprs.rcx = 100;
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter && b.validated_stores && !b.counter_bounded);
        assert_eq!(b.count, 4);
        for address in [0x1000, 0x3000, 0x4000, 0x5000] {
            ctx.state_mut().gprs.rdi = address;
            assert!(!planned(&ctx).is_some_and(|b| b.uses_counter));
        }
        ctx.state_mut().gprs.rdi = 0x7000;
        ctx.state_mut().gprs.rcx = 0;
        assert!(!planned(&ctx).is_some_and(|b| b.uses_counter));
        ctx.state_mut().gprs.rcx = u64::MAX;
        assert!(!planned(&ctx).is_some_and(|b| b.uses_counter));
    }

    #[test]
    fn counted_store_loops_accept_unrolled_memset_and_reject_mapping_changes() {
        // Eight stores per iteration, as in an unrolled memset body.
        let mut code = [0u8; 44];
        for index in 0..8 {
            code[index * 4..index * 4 + 4].copy_from_slice(&[0x48, 0x89, 0x47, index as u8 * 8]);
        }
        code[32..39].copy_from_slice(&[0x48, 0xff, 0xc9, 0x48, 0x8d, 0x7f, 64]);
        let length = 41;
        code[39..].copy_from_slice(&[0x75, (-(length as i8)) as u8, 0x0f, 0x01, 0xd9]);
        let mut ctx = paged_context(&code);
        ctx.state_mut().gprs.rcx = 128;
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter && b.validated_stores && !b.counter_bounded);
        assert_eq!(b.count, 11);
        assert_eq!(b.offsets[b.count] as usize, length);

        // A later destination aliases a translation table, even though the
        // first destination does not. Reject the whole loop.
        ctx.memory[0x6040..0x6048].copy_from_slice(&0x6007u64.to_le_bytes());
        assert!(!planned(&ctx).is_some_and(|b| b.uses_counter));
        // A noncontiguous mapping requires a different proof.
        ctx.memory[0x6040..0x6048].copy_from_slice(&0xa007u64.to_le_bytes());
        assert!(!planned(&ctx).is_some_and(|b| b.uses_counter));
        ctx.memory[0x6040..0x6048].copy_from_slice(&0x8007u64.to_le_bytes());
        assert!(planned(&ctx).unwrap().uses_counter);
        // Moving stores after the pointer advance invalidates the range.
        ctx.memory[0x1000..0x1004].copy_from_slice(&[0x48, 0x8d, 0x7f, 64]);
        assert!(!planned(&ctx).is_some_and(|b| b.uses_counter));
    }

    #[test]
    fn only_rejected_pages_are_cached_after_code_changes() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(prepare(&mut ctx, true, &window).unwrap().page_execution);
        // Modification outside the small instruction window must revoke
        // page-wide execution; accepted pages have no persistent cache entry.
        ctx.memory[0x1ff0..0x1ff3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        assert!(!prepare(&mut ctx, true, &window).unwrap().page_execution);
        assert!(ctx.state().svm_rejected_pages.contains(&0x1000));
        ctx.memory[0x1ff0..0x1ff3].fill(0x90);
        assert!(!prepare(&mut ctx, true, &window).unwrap().page_execution);
    }

    #[test]
    fn page_scan_rejects_rng_rep_chunk_boundaries_and_wrapping_aliases() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        assert!(page_safe(&ctx, 0x1000).is_some());
        for bytes in [
            &[0x0f, 0xc7, 0xf0][..],
            &[0xf3, 0x48, 0xab],
            &[0xf2, 0xa6],
            &[0x0f, 0x07],
        ] {
            for offset in [0, 254, 255, 256, 4094, 4095] {
                ctx.memory[0x1000..0x2000].fill(0x90);
                for (i, &byte) in bytes.iter().enumerate() {
                    ctx.memory[0x1000 + (offset + i) % 4096] = byte;
                }
                assert!(
                    page_safe(&ctx, 0x1000).is_none(),
                    "offset={offset} bytes={bytes:x?}"
                );
            }
        }
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1000..0x1004].copy_from_slice(&[0xf3, 0x0f, 0x1e, 0xfa]); // ENDBR64
        assert!(page_safe(&ctx, 0x1000).is_some());
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_verified(&ctx, true, true, &window).unwrap();
        assert!(batch.page_execution && batch.uses_counter);
        ctx.state_mut().stop_at_tsc = Some(65536);
        assert!(!prepare_verified(&ctx, true, true, &window).is_some_and(|b| b.page_execution));
    }

    #[test]
    fn forward_store_paths_require_addresses_stable_on_every_path() {
        // A store preceding a branch and another optional store are safe.
        let mut ctx = paged_context(&[
            0x48, 0x89, 0x07, 0x74, 4, 0x48, 0x89, 0x47, 8, 0x90, 0x0f, 0x01, 0xd9,
        ]);
        ctx.state_mut().stop_at_tsc = Some(4);
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter && b.counter_bounded && b.validated_stores);
        assert_eq!(b.count, 4);
        // An optional address change invalidates stores after the merge.
        let ctx = paged_context(&[0x90, 0x90, 0x74, 4, 0x48, 0x83, 0xc7, 8, 0x48, 0x89, 0x07]);
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter && b.counter_bounded);
        assert_eq!(b.count, 4); // Stops before the store with a changed address.
        assert!(!b.writes_memory);
        // Store loops require a range proof for every iteration.
        let ctx = paged_context(&[0x48, 0x89, 0x07, 0x90, 0x75, 0xfa]);
        let b = planned(&ctx).unwrap();
        assert!(!b.uses_counter);
        assert_eq!(b.count, 2);
    }

    #[test]
    fn forward_regions_use_the_endpoint_inside_the_interrupt_margin() {
        let mut ctx = paged_context(&[0x90, 0x74, 1, 0x90, 0x90, 0x0f, 0x01, 0xd9]);
        ctx.state_mut().stop_at_tsc = Some(4);
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter && b.counter_bounded && b.endpoint_intercepted);
        assert_eq!(b.count, 4);
        assert_eq!(b.counter_period(), 1 << 30);
        // A smaller deadline truncates the region before its branch target.
        ctx.state_mut().stop_at_tsc = Some(2);
        assert!(planned(&ctx).is_none());
        // Backward edges still require an interrupt-latency margin.
        let mut ctx = paged_context(&[0x90, 0x90, 0x75, 0xfc, 0x0f, 0x01, 0xd9]);
        ctx.state_mut().stop_at_tsc = Some(4);
        let b = planned(&ctx).unwrap();
        assert!(!b.uses_counter);
        assert_eq!(b.count, 2);
        ctx.state_mut().stop_at_tsc = Some(100_000);
        let b = planned(&ctx).unwrap();
        assert!(b.uses_counter && !b.counter_bounded);
    }

    #[test]
    fn relative_branch_lengths_and_targets_match_independent_decoder() {
        use iced_x86::{Decoder, DecoderOptions, FlowControl};
        for bitness in [16, 32, 64] {
            for displacement in -128i8..=127 {
                for opcode in (0x70..=0x7f).chain([0xeb]) {
                    let bytes = [opcode, displacement as u8];
                    let (length, relative) =
                        relative_branch(&bytes, bitness == 64, bitness == 32).unwrap();
                    let instruction = Decoder::new(bitness, &bytes, DecoderOptions::NONE).decode();
                    assert_eq!(length, instruction.len());
                    let mask = if bitness == 16 {
                        0xffff
                    } else if bitness == 32 {
                        0xffff_ffff
                    } else {
                        u64::MAX
                    };
                    assert_eq!(
                        (length as i64 + relative) as u64 & mask,
                        instruction.near_branch_target()
                    );
                    assert!(matches!(
                        instruction.flow_control(),
                        FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch
                    ));
                }
            }
            for displacement in [i32::MIN, -256, -1, 0, 256, i32::MAX] {
                for opcode in [&[0xe9][..], &[0x0f, 0x85][..]] {
                    let mut bytes = opcode.to_vec();
                    if bitness == 16 {
                        bytes.extend((displacement as i16).to_le_bytes());
                    } else {
                        bytes.extend(displacement.to_le_bytes());
                    }
                    let (length, relative) =
                        relative_branch(&bytes, bitness == 64, bitness == 32).unwrap();
                    let instruction = Decoder::new(bitness, &bytes, DecoderOptions::NONE).decode();
                    assert_eq!(length, instruction.len());
                    let mask = if bitness == 16 {
                        0xffff
                    } else if bitness == 32 {
                        0xffff_ffff
                    } else {
                        u64::MAX
                    };
                    assert_eq!(
                        (length as i64 + relative) as u64 & mask,
                        instruction.near_branch_target()
                    );
                }
            }
        }
    }
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
                if bitness == 64 {
                    use iced_x86::Register;
                    let gprs = [
                        Register::RAX,
                        Register::RCX,
                        Register::RDX,
                        Register::RBX,
                        Register::RSP,
                        Register::RBP,
                        Register::RSI,
                        Register::RDI,
                        Register::R8,
                        Register::R9,
                        Register::R10,
                        Register::R11,
                        Register::R12,
                        Register::R13,
                        Register::R14,
                        Register::R15,
                    ];
                    let changed = modified_gprs(&bytes[..length], true);
                    for register in info.used_registers() {
                        if matches!(
                            register.access(),
                            OpAccess::Write
                                | OpAccess::CondWrite
                                | OpAccess::ReadWrite
                                | OpAccess::ReadCondWrite
                        ) {
                            if let Some(index) = gprs
                                .iter()
                                .position(|&r| r == register.register().full_register())
                            {
                                assert_ne!(
                                    changed & (1 << index),
                                    0,
                                    "missing write: bytes={bytes:02x?} register={:?}",
                                    register.register()
                                );
                            }
                        }
                    }
                    let values = core::array::from_fn(|i| 0x10000 + i as u64 * 0x100);
                    if let Some((address, width)) = store_range(&bytes[..length], 0, &values, 0) {
                        let memory = info.used_memory();
                        assert_eq!(memory.len(), 1);
                        let expected = memory[0].virtual_address(0, |r, _, _| {
                            if matches!(
                                r,
                                Register::ES | Register::CS | Register::SS | Register::DS
                            ) {
                                return Some(0);
                            }
                            gprs.iter()
                                .position(|&g| g == r.full_register())
                                .map(|i| values[i])
                        });
                        assert_eq!(Some(address), expected, "address bytes={bytes:02x?}");
                        assert_eq!(
                            width,
                            memory[0].memory_size().size() as u64,
                            "width bytes={bytes:02x?}"
                        );
                    }
                }
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
    fn counter_period_leaves_a_deadline_margin() {
        let mut batch = InstructionBatch {
            start: 0x1000,
            offsets: [0; 65],
            count: 4,
            repeat: None,
            pages: [0; 5],
            page_count: 0,
            accesses_memory: false,
            writes_memory: false,
            validated_stores: false,
            uses_counter: true,
            counter_bounded: false,
            page_execution: false,
            endpoint_intercepted: false,
            instruction_budget: u64::MAX,
        };
        batch.offsets[..5].copy_from_slice(&[0, 3, 4, 5, 7]);
        assert_eq!(batch.counter_period(), 1 << 30);
        batch.instruction_budget = InstructionBatch::COUNTER_DEADLINE_MARGIN + 301;
        assert_eq!(batch.counter_period(), 301);
        batch.instruction_budget = InstructionBatch::COUNTER_DEADLINE_MARGIN;
        assert_eq!(batch.counter_period(), 1);
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
            validated_stores: false,
            uses_counter: false,
            counter_bounded: true,
            page_execution: false,
            endpoint_intercepted: false,
            instruction_budget: u64::MAX,
        };
        b.offsets[1] = 3;
        b.offsets[2] = 8;
        assert_eq!(b.completed_at(0x1003), Some(1));
        assert_eq!(b.completed_at(0x1008), Some(2));
        assert_eq!(b.completed_at(0x1004), None);
    }
}
