// SPDX-License-Identifier: GPL-2.0
//! Bounded instruction sequences for AMD hardware execution.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
use super::super::traits::{CountedLoopBatch, CowAllocator, InstructionBatch, RepeatBatch};
use super::super::vm_state::SVM_CODE_PAGE_CAPACITY;
#[cfg(not(feature = "cargo"))]
use crate::ept::NptExecutionGuard;
#[cfg(feature = "cargo")]
use crate::prelude::*;
#[cfg(feature = "cargo")]
use bedrock_ept::NptExecutionGuard;

const _: () = assert!(SVM_CODE_PAGE_CAPACITY <= NptExecutionGuard::MAX_PAGES);

/// Unknown instructions and control transfers terminate a batch. The flags
/// distinguish memory access from stores, including implicit stack accesses.
fn safe_len(bytes: &[u8], long: bool, default32: bool) -> Option<(usize, bool, bool)> {
    if long && bytes.starts_with(&[0xf3, 0x0f, 0x1e, 0xfa]) {
        return Some((4, false, false)); // ENDBR64 changes no registers or RAM.
    }
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
    if long && bytes.starts_with(&[0xf3, 0x0f, 0x1e, 0xfa]) {
        return 0;
    }
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

fn collect_translation_tree<C: VmContext>(ctx: &mut C, batch: &InstructionBatch) -> Option<()> {
    let root = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestCr3)
        .ok()?
        & 0x000f_ffff_ffff_f000;
    if batch.pages[..batch.code_page_count].contains(&root) {
        return None;
    }
    let scratch = &ctx.state().svm_guard;
    if scratch.valid && scratch.root == root {
        return (!batch.pages[..batch.code_page_count]
            .iter()
            .any(|page| scratch.tables[..scratch.count].contains(page)))
        .then_some(());
    }
    let scratch = &mut ctx.state_mut().svm_guard;
    scratch.alias_proof.valid = false;
    scratch.valid = false;
    scratch.translation_count = 0;
    scratch.translation_cursor = 0;
    scratch.root = root;
    scratch.tables[0] = root;
    scratch.levels[0] = 4;
    scratch.count = 1;
    let mut cursor = 0;
    let mut bytes = [0u8; 512];
    while cursor < ctx.state().svm_guard.count {
        let level = ctx.state().svm_guard.levels[cursor];
        let table = ctx.state().svm_guard.tables[cursor];
        if level > 1 {
            for offset in (0..4096).step_by(512) {
                ctx.read_guest_memory(GuestPhysAddr::new(table + offset), &mut bytes)
                    .ok()?;
                for entry in bytes.chunks_exact(8) {
                    let entry = u64::from_le_bytes(entry.try_into().ok()?);
                    if entry & 1 == 0 {
                        continue;
                    }
                    if entry & (1 << 7) != 0 {
                        if level == 4 {
                            return None;
                        }
                        continue;
                    }
                    let child = entry & 0x000f_ffff_ffff_f000;
                    if batch.pages[..batch.code_page_count].contains(&child) {
                        return None;
                    }
                    let scratch = &mut ctx.state_mut().svm_guard;
                    if let Some(index) = scratch.tables[..scratch.count]
                        .iter()
                        .position(|&p| p == child)
                    {
                        if scratch.levels[index] != level - 1 {
                            return None;
                        }
                        continue;
                    }
                    if scratch.count == scratch.tables.len() {
                        return None;
                    }
                    let index = scratch.count;
                    scratch.tables[index] = child;
                    scratch.levels[index] = level - 1;
                    scratch.count += 1;
                }
            }
        }
        cursor += 1;
    }
    ctx.state_mut().svm_guard.valid = true;
    Some(())
}

/// The table proof covers every reachable frame, so A/D updates cannot change
/// these mappings. Only call after collect_translation_tree validates CR3.
fn cached_code_translation<C: VmContext>(ctx: &mut C, linear: u64) -> Option<u64> {
    if ctx.state().svm_guard.valid {
        let cache = &ctx.state().svm_guard;
        if let Some(&(_, physical)) = cache.translations[..cache.translation_count]
            .iter()
            .find(|&&(page, _)| page == linear)
        {
            return Some(physical);
        }
    }
    let physical = super::svm::physical(ctx, linear).ok()?.as_u64() & !4095;
    let cache = &mut ctx.state_mut().svm_guard;
    if cache.valid {
        let slot = if cache.translation_count < cache.translations.len() {
            let slot = cache.translation_count;
            cache.translation_count += 1;
            slot
        } else {
            let slot = cache.translation_cursor;
            cache.translation_cursor = (slot + 1) % cache.translations.len();
            slot
        };
        cache.translations[slot] = (linear, physical);
    }
    Some(physical)
}

/// IRETQ reads its stack; descriptor loads can set only the accessed bit.
/// Keep the proof when those descriptor updates cannot change guarded frames.
fn scalar_iret_preserves_guard<C: VmContext>(
    ctx: &C,
    window: &super::svm::InstructionWindow,
) -> bool {
    if !window.bytes.starts_with(&[0x48, 0xcf]) {
        return false;
    }
    let v = &ctx.state().vmcs;
    let check = || -> Option<()> {
        // Shadow-stack transitions require separate memory-write validation.
        if v.read_natural(VmcsFieldNatural::GuestCr4).ok()? & (1 << 23) != 0 {
            return None;
        }
        let rsp = v.read_natural(VmcsFieldNatural::GuestRsp).ok()?;
        for slot in [1u64, 4] {
            let selector_address = rsp.checked_add(slot * 8)?;
            let mut selector = [0u8; 2];
            for index in 0..2 {
                let physical =
                    super::svm::physical(ctx, selector_address.checked_add(index as u64)?).ok()?;
                ctx.read_guest_memory(physical, &mut selector[index..index + 1])
                    .ok()?;
            }
            let selector = u16::from_le_bytes(selector);
            if slot == 4 && selector & !3 == 0 {
                continue;
            }
            if selector & !3 == 0 {
                return None;
            }
            let (base, limit) = if selector & 4 == 0 {
                (
                    v.read_natural(VmcsFieldNatural::GuestGdtrBase).ok()?,
                    v.read32(VmcsField32::GuestGdtrLimit).ok()?,
                )
            } else {
                if v.read32(VmcsField32::GuestLdtrAccessRights).ok()? & (1 << 16) != 0 {
                    return None;
                }
                (
                    v.read_natural(VmcsFieldNatural::GuestLdtrBase).ok()?,
                    v.read32(VmcsField32::GuestLdtrLimit).ok()?,
                )
            };
            let offset = u64::from(selector & !7);
            if offset + 7 > u64::from(limit) {
                return None;
            }
            let physical = super::svm::physical(ctx, base.checked_add(offset + 5)?).ok()?;
            let mut access = [0];
            ctx.read_guest_memory(physical, &mut access).ok()?;
            if access[0] & 1 != 0 {
                continue;
            }
            let page = physical.as_u64() & !4095;
            let guard = &ctx.state().svm_guard;
            if guard.tables[..guard.count].contains(&page)
                || guard.code[..guard.code_count]
                    .iter()
                    .any(|proof| proof.page == page)
            {
                return None;
            }
        }
        Some(())
    };
    check().is_some()
}

/// Retain table and code proofs across guarded entries or scalar instructions
/// whose writes are proven disjoint. A/D updates cannot add table frames.
pub(crate) fn retain_translation_cache<C: VmContext>(
    ctx: &mut C,
    batch: Option<&InstructionBatch>,
    window: Option<&super::svm::InstructionWindow>,
    software: bool,
) {
    if !ctx.state().svm_guard.valid {
        ctx.state_mut().svm_guard.code_count = 0;
        return;
    }
    let keep = !software
        && match batch {
            Some(batch) => batch.page_execution || batch.guarded_stores || !batch.writes_memory,
            None => window.is_some_and(|window| {
                let long = ctx
                    .state()
                    .vmcs
                    .read32(VmcsField32::GuestCsAccessRights)
                    .unwrap_or(0)
                    & (1 << 13)
                    != 0;
                long && (scalar_iret_preserves_guard(ctx, window)
                    || matches!(window.bytes[0], 0xc3 | 0x9d | 0xe4..=0xe7 | 0xec..=0xef)
                    || relative_branch(&window.bytes, true, false).is_some()
                    || safe_len(&window.bytes, true, false).is_some_and(|(_, _, writes)| !writes)
                    || scalar_store_preserves_guard(ctx, window, false))
            }),
        };
    if !keep {
        ctx.state_mut().svm_guard.valid = false;
        ctx.state_mut().svm_guard.code_count = 0;
        return;
    }
    match batch {
        Some(batch) if batch.page_execution || batch.guarded_stores => {
            // Other cached pages may be written as data during this entry.
            let cache = &mut ctx.state_mut().svm_guard;
            let mut index = 0;
            while index < cache.code_count {
                if !batch.pages[..batch.code_page_count].contains(&cache.code[index].page) {
                    cache.code_count -= 1;
                    cache.code[index] = cache.code[cache.code_count];
                } else {
                    index += 1;
                }
            }
        }
        None => {
            if let Some(window) = window {
                if safe_len(&window.bytes, true, false).is_some_and(|(_, _, writes)| writes)
                    && !scalar_store_preserves_guard(ctx, window, true)
                {
                    ctx.state_mut().svm_guard.code_count = 0;
                }
            }
        }
        _ => {}
    }
}

fn scalar_store_preserves_guard<C: VmContext>(
    ctx: &C,
    window: &super::svm::InstructionWindow,
    protect_code: bool,
) -> bool {
    let state = ctx.state();
    let g = &state.gprs;
    let Ok(rsp) = state.vmcs.read_natural(VmcsFieldNatural::GuestRsp) else {
        return false;
    };
    let Ok(rip) = state.vmcs.read_natural(VmcsFieldNatural::GuestRip) else {
        return false;
    };
    let gprs = [
        g.rax, g.rcx, g.rdx, g.rbx, rsp, g.rbp, g.rsi, g.rdi, g.r8, g.r9, g.r10, g.r11, g.r12,
        g.r13, g.r14, g.r15,
    ];
    let range = if window.bytes[0] == 0x9c {
        // Unprefixed PUSHFQ is emulated at an SVM intercept, but its stack
        // destination can be checked before entry just like an ordinary PUSH.
        rsp.checked_sub(8).map(|address| (address, 8))
    } else {
        safe_len(&window.bytes, true, false).and_then(|(length, _, _)| {
            // RIP-relative stores use the instruction end, not the window end.
            store_range(&window.bytes[..length], rip, &gprs, 0)
        })
    };
    let Some((address, width)) = range else {
        return false;
    };
    let Some(end) = address.checked_add(width - 1) else {
        return false;
    };
    let mut page = address & !4095;
    loop {
        let Ok(physical) = super::svm::physical(ctx, page) else {
            return false;
        };
        if state.svm_guard.tables[..state.svm_guard.count].contains(&(physical.as_u64() & !4095)) {
            return false;
        }
        if protect_code
            && state.svm_guard.code[..state.svm_guard.code_count]
                .iter()
                .any(|proof| proof.page == physical.as_u64() & !4095)
        {
            return false;
        }
        if page == end & !4095 {
            return true;
        }
        page += 4096;
    }
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

#[cfg(test)]
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

#[derive(Clone, Copy)]
struct PageHazards {
    boundary: [u8; 32],
    edge: u16,
    offsets: [u16; 4],
    count: usize,
}

fn cached_page_hazards<C: VmContext>(ctx: &mut C, physical: u64) -> Option<PageHazards> {
    if !ctx.state().svm_guard.valid {
        ctx.state_mut().svm_guard.code_count = 0;
        ctx.state_mut().svm_guard.alias_proof.valid = false;
    }
    let cache = &ctx.state().svm_guard;
    if let Some(proof) = cache.code[..cache.code_count]
        .iter()
        .find(|p| p.page == physical)
    {
        return Some(PageHazards {
            boundary: proof.boundary,
            edge: proof.edge,
            offsets: proof.offsets,
            count: proof.count,
        });
    }
    // Translation invalidation does not necessarily change code. Reuse a
    // previous scan only after comparing every byte, including page edges.
    let memo = ctx
        .state()
        .svm_guard
        .hazard_memos
        .iter()
        .position(|memo| memo.valid && memo.proof.page == physical);
    let mut bytes = [0u8; 512];
    let unchanged = memo.is_some_and(|index| {
        for offset in (0..4096).step_by(bytes.len()) {
            if ctx
                .read_guest_memory(GuestPhysAddr::new(physical + offset as u64), &mut bytes)
                .is_err()
                || ctx.state().svm_guard.hazard_memos[index].bytes[offset..offset + bytes.len()]
                    != bytes
            {
                return false;
            }
        }
        true
    });
    let hazards = if unchanged {
        let proof = ctx.state().svm_guard.hazard_memos[memo.unwrap()].proof;
        PageHazards {
            boundary: proof.boundary,
            edge: proof.edge,
            offsets: proof.offsets,
            count: proof.count,
        }
    } else {
        let hazards = page_hazards(ctx, physical)?;
        let index = memo.unwrap_or(ctx.state().svm_guard.hazard_memo_cursor);
        ctx.state_mut().svm_guard.hazard_memos[index].valid = false;
        for offset in (0..4096).step_by(bytes.len()) {
            ctx.read_guest_memory(GuestPhysAddr::new(physical + offset as u64), &mut bytes)
                .ok()?;
            ctx.state_mut().svm_guard.hazard_memos[index].bytes[offset..offset + bytes.len()]
                .copy_from_slice(&bytes);
        }
        let cache = &mut ctx.state_mut().svm_guard;
        cache.hazard_memos[index].proof = super::super::vm_state::SvmCodeProof {
            page: physical,
            boundary: hazards.boundary,
            edge: hazards.edge,
            offsets: hazards.offsets,
            count: hazards.count,
        };
        cache.hazard_memos[index].valid = true;
        if memo.is_none() {
            cache.hazard_memo_cursor = (index + 1) % cache.hazard_memos.len();
        }
        hazards
    };
    let cache = &mut ctx.state_mut().svm_guard;
    let index = if cache.code_count < cache.code.len() {
        let index = cache.code_count;
        cache.code_count += 1;
        index
    } else {
        let index = cache.code_cursor;
        cache.code_cursor = (cache.code_cursor + 1) % cache.code.len();
        index
    };
    cache.code[index] = super::super::vm_state::SvmCodeProof {
        page: physical,
        boundary: hazards.boundary,
        edge: hazards.edge,
        offsets: hazards.offsets,
        count: hazards.count,
    };
    Some(hazards)
}

fn hazardous_entry(bytes: &[u8]) -> bool {
    let mut repeat = false;
    for index in 0..15 {
        let Some(&byte) = bytes.get(index) else {
            return false;
        };
        if matches!(
            byte,
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0x40..=0x4f | 0xf2 | 0xf3
        ) {
            repeat |= matches!(byte, 0xf2 | 0xf3);
            continue;
        }
        return (repeat && matches!(byte, 0xa4..=0xa7 | 0xaa..=0xaf))
            || bytes[index..].starts_with(&[0x0f, 0x07])
            || (bytes[index..].starts_with(&[0x0f, 0xc7])
                && bytes.get(index + 2).is_some_and(|&b| b >= 0xf0));
    }
    false
}

fn page_hazards<C: VmContext>(ctx: &C, physical: u64) -> Option<PageHazards> {
    // Amortize guest-memory translation without placing a full code page on
    // the kernel stack. The carry covers every legal instruction prefix chain.
    const CHUNK: usize = 512;
    let mut result = PageHazards {
        boundary: [0; 32],
        edge: 0,
        offsets: [0; 4],
        count: 0,
    };
    let mut bytes = [0u8; CHUNK + 16];
    for offset in (0..4096).step_by(CHUNK) {
        ctx.read_guest_memory(
            GuestPhysAddr::new(physical + offset as u64),
            &mut bytes[16..],
        )
        .ok()?;
        if offset == 0 {
            result.boundary[..16].copy_from_slice(&bytes[16..32]);
        }
        for index in 0..bytes.len() {
            let position = offset as i64 + index as i64 - 16;
            let byte = bytes[index];
            let marker = (byte == 0x0f
                && (bytes[index..].starts_with(&[0x0f, 0x07])
                    || (bytes[index..].starts_with(&[0x0f, 0xc7])
                        && bytes.get(index + 2).is_some_and(|&b| b >= 0xf0))))
                || (matches!(byte, 0xf2 | 0xf3) && hazardous_entry(&bytes[index..]));
            if position < 0 || position >= 4096 || !marker {
                continue;
            }
            let mut first = index;
            while first > 0
                && index - first < 14
                && matches!(
                    bytes[first - 1],
                    0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0x40
                        ..=0x4f | 0xf2 | 0xf3
                )
            {
                first -= 1;
            }
            for entry in first..=index {
                let position = offset as i64 + entry as i64 - 16;
                if position < 0 || !hazardous_entry(&bytes[entry..]) {
                    continue;
                }
                let position = position as u16;
                if result.offsets[..result.count].contains(&position) {
                    continue;
                }
                if result.count == result.offsets.len() {
                    return None;
                }
                result.offsets[result.count] = position;
                result.count += 1;
            }
        }
        bytes.copy_within(CHUNK..CHUNK + 16, 0);
    }
    // Wrapping aliases and crossings to other permitted pages stay on the
    // conservative path. Interior hazards are covered at every prefix entry.
    result.boundary[16..].copy_from_slice(&bytes[..16]);
    result.edge = summarize_edge(&result.boundary);
    if forbidden_page_bytes(&result.boundary[..16])
        || forbidden_page_bytes(&result.boundary[16..])
        || !hazard_boundary_safe(&result, &result)
    {
        return None;
    }
    Some(result)
}

fn collect_page_breakpoints<C: VmContext>(
    ctx: &mut C,
    batch: &mut InstructionBatch,
    hazards: &[PageHazards; SVM_CODE_PAGE_CAPACITY],
) -> Option<()> {
    batch.page_breakpoint_count = 0;
    if hazards[..batch.code_page_count]
        .iter()
        .all(|h| h.count == 0)
    {
        return Some(());
    }
    let proof = &ctx.state().svm_guard.alias_proof;
    // Only hazardous pages require alias breakpoints. Their physical set is
    // independent of the order and membership of ordinary selected code pages.
    let hazard_pages = hazards[..batch.code_page_count]
        .iter()
        .filter(|h| h.count != 0)
        .count();
    if ctx.state().svm_guard.valid
        && proof.valid
        && proof.page_count == hazard_pages
        && hazards[..batch.code_page_count]
            .iter()
            .enumerate()
            .all(|(i, h)| {
                h.count == 0
                    || proof.pages[..proof.page_count]
                        .iter()
                        .position(|&page| page == batch.pages[i])
                        .is_some_and(|slot| {
                            proof.counts[slot] == h.count
                                && proof.offsets[slot][..h.count] == h.offsets[..h.count]
                        })
            })
    {
        batch.page_breakpoints = proof.breakpoints;
        batch.page_breakpoint_count = proof.breakpoint_count;
        return Some(());
    }
    // Enumerate executable virtual aliases. NPT alone protects physical pages;
    // a hardware breakpoint must cover every virtual entry to hazardous bytes.
    // A physical table may appear through several virtual paths. Keep each
    // path to enumerate every executable alias, with a bounded heap workspace.
    use super::super::vm_state::SvmAliasWalk;
    ctx.state_mut().svm_guard.aliases[0] = SvmAliasWalk {
        table: ctx
            .state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestCr3)
            .ok()?
            & 0x000f_ffff_ffff_f000,
        base: 0,
        level: 4,
    };
    let mut count = 1;
    let mut cursor = 0;
    let mut bytes = [0u8; 512];
    batch.page_breakpoint_count = 0;
    while cursor < count {
        let walk = ctx.state().svm_guard.aliases[cursor];
        let shift = 12 + 9 * (walk.level - 1);
        for offset in (0..4096).step_by(512) {
            ctx.read_guest_memory(GuestPhysAddr::new(walk.table + offset), &mut bytes)
                .ok()?;
            for (part, entry) in bytes.chunks_exact(8).enumerate() {
                let entry = u64::from_le_bytes(entry.try_into().ok()?);
                if entry & 1 == 0 || entry & (1 << 63) != 0 {
                    continue;
                }
                let base = walk.base | ((offset / 8 + part as u64) << shift);
                let physical = entry & 0x000f_ffff_ffff_f000;
                if walk.level == 1 || entry & (1 << 7) != 0 {
                    let physical = physical & !((1u64 << shift) - 1);
                    for (index, hazard) in hazards.iter().enumerate().take(batch.code_page_count) {
                        let page = batch.pages[index];
                        if page < physical || page - physical >= 1 << shift {
                            continue;
                        }
                        let virtual_page = base + (page - physical);
                        let virtual_page = if virtual_page & (1 << 47) != 0 {
                            virtual_page | (!0u64 << 48)
                        } else {
                            virtual_page
                        };
                        for &offset in &hazard.offsets[..hazard.count] {
                            let address = virtual_page + u64::from(offset);
                            if batch.page_breakpoints[..batch.page_breakpoint_count]
                                .contains(&address)
                            {
                                continue;
                            }
                            if batch.page_breakpoint_count == 4 {
                                return None;
                            }
                            batch.page_breakpoints[batch.page_breakpoint_count] = address;
                            batch.page_breakpoint_count += 1;
                        }
                    }
                } else {
                    if count == ctx.state().svm_guard.aliases.len() {
                        return None;
                    }
                    ctx.state_mut().svm_guard.aliases[count] = SvmAliasWalk {
                        table: physical,
                        base,
                        level: walk.level - 1,
                    };
                    count += 1;
                }
            }
        }
        cursor += 1;
    }
    let proof = &mut ctx.state_mut().svm_guard.alias_proof;
    proof.valid = true;
    proof.page_count = hazard_pages;
    let mut slot = 0;
    for (i, hazard) in hazards.iter().enumerate().take(batch.code_page_count) {
        if hazard.count == 0 {
            continue;
        }
        proof.pages[slot] = batch.pages[i];
        proof.offsets[slot] = hazard.offsets;
        proof.counts[slot] = hazard.count;
        slot += 1;
    }
    proof.breakpoints = batch.page_breakpoints;
    proof.breakpoint_count = batch.page_breakpoint_count;
    Some(())
}

fn edge_prefix(byte: u8) -> bool {
    matches!(
        byte,
        0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0x40..=0x4f | 0xf2 | 0xf3
    )
}

// Cache the first opcode after leading prefixes and the distance to the
// nearest REP prefix in the trailing prefix chain. Fifteen means no usable
// trailing REP or a leading chain too long to continue a legal instruction.
fn summarize_edge(boundary: &[u8; 32]) -> u16 {
    let leading = boundary[..16]
        .iter()
        .position(|&b| !edge_prefix(b))
        .unwrap_or(15);
    let mut trailing_rep = 15;
    for (i, &byte) in boundary[16..].iter().rev().take(14).enumerate() {
        if !edge_prefix(byte) {
            break;
        }
        if matches!(byte, 0xf2 | 0xf3) {
            trailing_rep = i + 1;
            break;
        }
    }
    (u16::from(boundary[leading]) << 8) | ((leading as u16) << 4) | trailing_rep as u16
}

fn hazard_boundary_safe(left: &PageHazards, right: &PageHazards) -> bool {
    // Each half was scanned when its page proof was built. Only opcodes or
    // REP prefix chains straddling the boundary need to be checked here.
    let first = right.boundary[0];
    let last = left.boundary[31];
    if (last == 0x0f && first == 0x07)
        || (left.boundary[30] == 0x0f && last == 0xc7 && first >= 0xf0)
        || (last == 0x0f && first == 0xc7 && right.boundary[1] >= 0xf0)
    {
        return false;
    }
    let distance = left.edge & 15;
    let leading = (right.edge >> 4) & 15;
    !(distance + leading <= 14 && matches!((right.edge >> 8) as u8, 0xa4..=0xa7 | 0xaa..=0xaf))
}

// A counted MOV-store loop can use entry-time range validation when RDI
// advances once per iteration and RCX decreases once before JNZ. Other writes
// or control transfers require the ordinary conservative planner.
fn counted_store_loop<C: VmContext>(
    ctx: &C,
    linear: u64,
    bytes: &[u8],
    gprs: &[u64; 16],
    batch: &mut InstructionBatch,
) -> Option<()> {
    let mut candidate = *batch;
    let mut offset = 0;
    let mut decrement = None;
    let mut payload_registers = 0u16;
    let mut stride = None;
    let mut first = u64::MAX;
    let mut last = 0;
    while candidate.count < 64 {
        let tail = bytes.get(offset..)?;
        if tail.starts_with(&[0x75]) || tail.starts_with(&[0x0f, 0x85]) {
            let (length, displacement) = relative_branch(tail, true, false)?;
            if offset as i64 + length as i64 + displacement != 0
                || decrement.is_none()
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
        let counter = match instruction {
            [0x48, 0xff, 0xc9] => Some((1u8, 64u8)),
            [0x48, 0xff, 0xca] => Some((2, 64)),
            [0xff, 0xc9] => Some((1, 32)),
            [0xff, 0xca] => Some((2, 32)),
            _ => None,
        };
        if let Some((register, width)) = counter {
            if decrement.is_some() {
                return None;
            }
            decrement = Some((candidate.count as u64, register, width));
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
            // Include high-byte aliases and stores before the decrement.
            if matches!(instruction[p], 0x88 | 0x89) {
                let mut source = ((instruction[p + 1] >> 3) & 7) | ((rex & 4) << 1);
                if instruction[p] == 0x88 && rex == 0 && source >= 4 {
                    source -= 4;
                }
                payload_registers |= 1 << source;
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
    let (decrement_index, register, width) = decrement?;
    if payload_registers & (1 << register) != 0 {
        return None;
    }
    let original_count = gprs[register as usize];
    let count = if width == 32 {
        original_count & u32::MAX as u64
    } else {
        original_count
    };
    let iterations = count
        .min(candidate.instruction_budget / candidate.count as u64)
        .min(65536);
    if iterations == 0 {
        return None;
    }
    let start = gprs[7].checked_add(first)?;
    let end = gprs[7]
        .checked_add(iterations.checked_sub(1)?.checked_mul(stride)?)?
        .checked_add(last.checked_sub(1)?)?;
    collect_code_tables(ctx, &mut candidate, linear)?;
    validate_contiguous_store_range(ctx, &candidate, start, end)?;
    candidate.counted_loop = Some(CountedLoopBatch {
        register,
        width,
        original_count,
        iterations,
        decrement_index,
    });
    candidate.uses_counter = false;
    candidate.counter_bounded = true;
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
    can_guard_page_tables: bool,
    window: &super::svm::InstructionWindow,
) -> Option<InstructionBatch> {
    let page = window.physical.as_u64() & !4095;
    let mut allow_page = false;
    // Reuse hazard scans while the guard proves the bytes have not changed.
    // Alias proofs follow the guarded tree; cross-page bytes are checked anew.
    let mut hazards = [PageHazards {
        boundary: [0; 32],
        edge: 0,
        offsets: [0; 4],
        count: 0,
    }; SVM_CODE_PAGE_CAPACITY];
    let v = &ctx.state().vmcs;
    let long = v.read32(VmcsField32::GuestCsAccessRights).ok()? & (1 << 13) != 0;
    if long
        && can_loop
        && can_guard_page_tables
        && instruction_budget(ctx) > InstructionBatch::COUNTER_DEADLINE_MARGIN
        && !ctx.state().svm_rejected_pages.contains(&page)
    {
        if let Some(hazard) = cached_page_hazards(ctx, page) {
            hazards[0] = hazard;
            allow_page = true;
        }
        if !allow_page {
            let state = ctx.state_mut();
            state.svm_rejected_pages[state.svm_rejected_cursor] = page;
            state.svm_rejected_cursor = (state.svm_rejected_cursor + 1) % 64;
        }
    }
    let prepared = prepare_verified(ctx, can_loop, allow_page, can_guard_page_tables, window);
    if allow_page && !prepared.as_ref().is_some_and(|b| b.page_execution) {
        let state = ctx.state_mut();
        state.svm_rejected_pages[state.svm_rejected_cursor] = page;
        state.svm_rejected_cursor = (state.svm_rejected_cursor + 1) % 64;
    }
    let mut batch = prepared?;
    if batch.guarded_stores {
        if !can_guard_page_tables || collect_translation_tree(ctx, &batch).is_none() {
            return None;
        }
    }
    if batch.page_execution {
        if ctx
            .state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestCr0)
            .ok()?
            & (1 << 31)
            == 0
        {
            ctx.state_mut().svm_guard.count = 0;
            ctx.state_mut().svm_guard.valid = false;
        }
        if ctx
            .state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestCr0)
            .ok()?
            & (1 << 31)
            != 0
            && collect_translation_tree(ctx, &batch).is_none()
        {
            let state = ctx.state_mut();
            state.svm_rejected_pages[state.svm_rejected_cursor] = page;
            state.svm_rejected_cursor = (state.svm_rejected_cursor + 1) % 64;
            return prepare_verified(ctx, can_loop, false, false, window);
        }
        let current = window.linear & !4095;
        let recent = ctx.state().svm_recent_pages;
        let mut updated = [u64::MAX; SVM_CODE_PAGE_CAPACITY];
        updated[0] = current;
        let mut next = 1;
        for linear in recent {
            if linear == u64::MAX || linear == current {
                continue;
            }
            if next < updated.len() {
                updated[next] = linear;
                next += 1;
            }
            if batch.code_page_count == SVM_CODE_PAGE_CAPACITY
                || batch.page_count == batch.pages.len()
            {
                continue;
            }
            let Some(physical) = cached_code_translation(ctx, linear) else {
                continue;
            };
            // Translation tables can never become executable code. Duplicated
            // physical code aliases are already covered by the guarded tree.
            if batch.pages[..batch.page_count].contains(&physical)
                || ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&physical)
                || ctx.state().svm_rejected_pages.contains(&physical)
            {
                continue;
            }
            let Some(hazard) = cached_page_hazards(ctx, physical) else {
                // A failed optional scan is also unusable as a primary page.
                // Remember it instead of rescanning its bytes on every plan.
                // Rejection is conservative even if the guest later rewrites
                // the page: only the acceleration opportunity is lost.
                let state = ctx.state_mut();
                state.svm_rejected_pages[state.svm_rejected_cursor] = physical;
                state.svm_rejected_cursor = (state.svm_rejected_cursor + 1) % 64;
                continue;
            };
            // Each physical hazard needs at least one execution breakpoint.
            // Do not enumerate the alias tree for a set already known to
            // exceed the four hardware slots.
            if hazards[..batch.code_page_count]
                .iter()
                .map(|h| h.count)
                .sum::<usize>()
                + hazard.count
                > 4
            {
                continue;
            }
            if hazards[..batch.code_page_count].iter().any(|previous| {
                !hazard_boundary_safe(previous, &hazard) || !hazard_boundary_safe(&hazard, previous)
            }) {
                continue;
            }
            batch.pages.copy_within(
                batch.code_page_count..batch.page_count,
                batch.code_page_count + 1,
            );
            batch.pages[batch.code_page_count] = physical;
            hazards[batch.code_page_count] = hazard;
            batch.code_page_count += 1;
            batch.page_count += 1;
        }
        ctx.state_mut().svm_recent_pages = updated;
        while collect_page_breakpoints(ctx, &mut batch, &hazards).is_none() {
            if batch.code_page_count == 1 {
                let state = ctx.state_mut();
                state.svm_rejected_pages[state.svm_rejected_cursor] = page;
                state.svm_rejected_cursor = (state.svm_rejected_cursor + 1) % 64;
                return prepare_verified(ctx, can_loop, false, false, window);
            }
            // Keep hazard-free pages when virtual aliases exhaust the slots.
            // Removing them cannot reduce the number of needed breakpoints.
            let remove = (1..batch.code_page_count)
                .rev()
                .find(|&index| hazards[index].count != 0)
                .unwrap_or(batch.code_page_count - 1);
            batch
                .pages
                .copy_within(remove + 1..batch.page_count, remove);
            hazards.copy_within(remove + 1..batch.code_page_count, remove);
            batch.code_page_count -= 1;
            batch.page_count -= 1;
        }
    }
    Some(batch)
}

pub(crate) fn remember_page<C: VmContext>(ctx: &mut C, linear: u64) {
    let page = linear & !4095;
    let recent = &mut ctx.state_mut().svm_recent_pages;
    if recent[0] == page {
        return;
    }
    let position = recent
        .iter()
        .position(|&p| p == page)
        .unwrap_or(recent.len() - 1);
    recent.copy_within(0..position, 1);
    recent[0] = page;
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
    allow_guarded_stores: bool,
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
        counted_loop: None,
        pages: [0; 20],
        code_page_count: 1,
        page_breakpoints: [0; 4],
        page_breakpoint_count: 0,
        branch_exits: [0; 3],
        branch_exit_count: 0,
        branch_exit_counts: [0; 3],
        page_count: 1,
        accesses_memory: false,
        writes_memory: false,
        validated_stores: false,
        guarded_stores: false,
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
    if long && paged && can_loop {
        if counted_store_loop(ctx, linear, &bytes[..available], &gprs, &mut batch).is_some() {
            return Some(batch);
        }
    }
    let mut changed = 0u16;
    let mut stores = StorePlan::default();
    let mut offset = 0;
    let mut branch_targets = [0i64; 64];
    let mut allow_branch_exits =
        long && allow_guarded_stores && budget > InstructionBatch::COUNTER_DEADLINE_MARGIN;
    let mut branch_sources = [0u16; 64];
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
                if (!allow_branch_exits && (target < 0 || target > available as i64))
                    || (backward
                        && (budget < InstructionBatch::COUNTER_DEADLINE_MARGIN
                            || (paged && batch.writes_memory && !batch.guarded_stores)))
                {
                    break;
                }
                if branch_count == 0 {
                    first_branch = batch.count;
                }
                branch_targets[branch_count] = target;
                branch_sources[branch_count] = (batch.count + 1) as u16;
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
            if long
                && can_loop
                && allow_guarded_stores
                && budget > InstructionBatch::COUNTER_DEADLINE_MARGIN
            {
                batch.guarded_stores = true;
                batch.uses_counter = true;
                batch.validated_stores = false;
            } else {
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
    // A bounded non-paged forward branch has two exact stop boundaries.
    // Stop before either successor instead of counting a converging path with
    // IRPERF, which undercounted this case intermittently on the test host.
    if !long && !paged && batch.counter_bounded && branch_count != 0 {
        let source = usize::from(branch_sources[0]);
        let end = i64::from(batch.offsets[source]);
        let target = branch_targets[0];
        let max_ip = if default32 {
            u32::MAX as u64
        } else {
            u16::MAX as u64
        };
        if target > end
            && rip
                .checked_add(target as u64)
                .is_some_and(|ip| ip <= max_ip)
        {
            batch.count = source;
            branch_count = 1;
            allow_branch_exits = true;
        }
    }
    if batch.guarded_stores && branch_count == 0 {
        // Every boundary has a known prefix length, including write faults.
        batch.uses_counter = false;
    }
    if batch.uses_counter {
        let mut invalid = false;
        let mut exact_paths = allow_branch_exits && branch_count != 0;
        for (index, &target) in branch_targets[..branch_count].iter().enumerate() {
            let decoded = target >= 0
                && target <= u16::MAX as i64
                && batch.offsets[..=batch.count].contains(&(target as u16));
            if decoded {
                exact_paths = false;
                continue;
            }
            let address = rip.wrapping_add(target as u64);
            let canonical = matches!((address as i64) >> 47, 0 | -1);
            if !allow_branch_exits || !canonical {
                invalid = true;
                break;
            }
            if let Some(slot) = batch.branch_exits[..batch.branch_exit_count]
                .iter()
                .position(|&target| target == address)
            {
                if batch.branch_exit_counts[slot] != branch_sources[index] {
                    exact_paths = false;
                }
                continue;
            }
            if batch.branch_exit_count == batch.branch_exits.len() {
                invalid = true;
                break;
            }
            batch.branch_exit_counts[batch.branch_exit_count] = branch_sources[index];
            batch.branch_exits[batch.branch_exit_count] = address;
            batch.branch_exit_count += 1;
        }
        if invalid {
            // Keep the straight-line prefix when targets exhaust the traps.
            batch.count = first_branch;
            batch.uses_counter = false;
            batch.branch_exit_count = 0;
        } else if exact_paths {
            batch.uses_counter = false;
            batch.counter_bounded = true;
        }
    }
    if batch.repeat.is_none() && batch.count < 2 && batch.branch_exit_count == 0 {
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
    saved_count: usize,
    execution: Option<NptExecutionGuard>,
}

pub(crate) fn protect<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    batch: &InstructionBatch,
) -> Option<BatchGuard> {
    let mut guard = BatchGuard {
        saved_count: 0,
        execution: None,
    };
    if !batch.accesses_memory && batch.branch_exit_count == 0 {
        return Some(guard);
    }
    let protected = if batch.page_execution || (batch.writes_memory && !batch.validated_stores) {
        batch.page_count
    } else {
        1
    };
    let tables = if batch.page_execution || batch.guarded_stores {
        ctx.state().svm_guard.count
    } else {
        0
    };
    for index in 0..protected + tables {
        let page = if index < protected {
            batch.pages[index]
        } else {
            ctx.state().svm_guard.tables[index - protected]
        };
        ctx.state_mut().svm_guard.saved[index].valid = false;
        guard.saved_count = index + 1;
        let gpa = GuestPhysAddr::new(page);
        let write_guard = match ctx.state_mut().ept.restrict_write_4k(allocator, gpa) {
            Ok(Some(guard)) => guard,
            Ok(None) => continue,
            Err(_) => {
                guard.restore(ctx, allocator);
                return None;
            }
        };
        ctx.state_mut().svm_guard.saved[index] = super::super::vm_state::SvmGuardSaved {
            guest: page,
            write_guard,
            valid: true,
        };
    }
    if batch.page_execution {
        let mut pages = [GuestPhysAddr::new(0); SVM_CODE_PAGE_CAPACITY];
        for (target, &page) in pages.iter_mut().zip(&batch.pages[..batch.code_page_count]) {
            *target = GuestPhysAddr::new(page);
        }
        guard.execution = ctx
            .state_mut()
            .ept
            .restrict_execution_to_pages(allocator, &pages[..batch.code_page_count]);
        if guard.execution.is_none() {
            guard.restore(ctx, allocator);
            return None;
        }
    }
    Some(guard)
}

impl BatchGuard {
    pub(crate) fn restore<C: VmContext, A: CowAllocator<C::CowPage>>(
        self,
        ctx: &mut C,
        allocator: &A,
    ) -> bool {
        let page_execution = self.execution.is_some();
        if let Some(execution) = self.execution {
            execution.restore(&mut ctx.state_mut().ept, allocator);
            // The fast exit is a synthetic boundary. Replay its instruction
            // once with stepping and unrestricted execute permissions.
        }
        let v = &ctx.state().vmcs;
        let delivered_exception = matches!(v.read32(VmcsField32::VmExitReason).ok(), Some(0 | 514));
        let fetch_boundary = v.read32(VmcsField32::VmExitReason).ok() == Some(37)
            && v.read_natural(VmcsFieldNatural::ExitQualification)
                .ok()
                .is_some_and(|q| q & InstructionBatch::PAGE_FETCH_BOUNDARY != 0)
            && super::svm::InstructionWindow::read(ctx)
                .ok()
                .is_some_and(|window| {
                    // Entry stores often need scalar replay for A/D or table
                    // writes anyway. Replanning first only adds another guard.
                    window.bytes[0] == 0xc3
                        || relative_branch(&window.bytes, true, false).is_some()
                        || safe_len(&window.bytes, true, false)
                            .is_some_and(|(_, _, writes)| !writes)
                });
        let write_fault = v.read32(VmcsField32::VmExitReason).ok() == Some(48)
            && v.read_natural(VmcsFieldNatural::ExitQualification)
                .unwrap_or(0)
                & 2
                != 0;
        let fault_page = v.read64(VmcsField64::GuestPhysicalAddr).unwrap_or(0) & !4095;
        let retry_single = write_fault
            && ctx.state().svm_guard.saved[..self.saved_count]
                .iter()
                .any(|saved| saved.valid && saved.guest == fault_page);
        for index in 0..self.saved_count {
            let saved = ctx.state().svm_guard.saved[index];
            ctx.state_mut().svm_guard.saved[index].valid = false;
            if !saved.valid {
                continue;
            }
            saved
                .write_guard
                .restore(&mut ctx.state_mut().ept, allocator);
        }
        // Exceptions are delivered by the common handler after accounting.
        // A trap may have advanced RIP, so stepping would lose the exception.
        (page_execution && !delivered_exception && !fetch_boundary) || retry_single
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
        prepare_verified(ctx, true, false, false, &window)
    }

    #[test]
    fn code_translations_follow_guard_revocation_and_root_changes() {
        let mut ctx = paged_context(&[0x90]);
        ctx.state_mut().svm_recent_pages[0] = 0x7000;
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let first = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(first.pages[..first.code_page_count].contains(&0x7000));
        assert_eq!(ctx.state().svm_guard.translation_count, 1);
        // Hardware A/D updates retain the physical mapping and table proof.
        ctx.memory[0x6038..0x6040].copy_from_slice(&0x7067u64.to_le_bytes());
        assert_eq!(cached_code_translation(&mut ctx, 0x7000), Some(0x7000));

        // An unprotected table write must revoke the guard before replanning.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x6038..0x6040].copy_from_slice(&0x9007u64.to_le_bytes());
        let second = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(second.pages[..second.code_page_count].contains(&0x9000));
        assert_eq!(ctx.state().svm_guard.translation_count, 1);

        // A different CR3 rebuilds even when the previous proof was valid.
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x2000);
        ctx.memory[0x2000..0x2008].copy_from_slice(&0x4007u64.to_le_bytes());
        ctx.memory[0x6038..0x6040].copy_from_slice(&0xa007u64.to_le_bytes());
        let third = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(third.pages[..third.code_page_count].contains(&0xa000));
        assert_eq!(ctx.state().svm_guard.root, 0x2000);
        assert_eq!(ctx.state().svm_guard.translation_count, 1);
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
    fn bounded_real_mode_branches_stop_at_exact_successor_counts() {
        let mut ctx = paged_context(&[0xf6, 0xc1, 1, 0x74, 1, 0x90, 0x90, 0x49, 0x75, 0xf6]);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr0, 0);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestCsAccessRights, 0x9b);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestCsLimit, 0xffff);
        ctx.vmcs_setup()
            .write64(VmcsField64::GuestIa32Efer, 0)
            .unwrap();
        ctx.state_mut().stop_at_tsc = Some(100);
        let batch = planned(&ctx).unwrap();
        assert!(!batch.uses_counter);
        assert_eq!(batch.count, 2);
        assert_eq!(batch.endpoint(), 0x1005);
        assert_eq!(&batch.branch_exits[..batch.branch_exit_count], &[0x1006]);
        assert_eq!(batch.completed_at(0x1005), Some(2));
        assert_eq!(batch.completed_at(0x1006), Some(2));
        assert!(!batch.is_boundary(0x1007));
    }

    #[test]
    fn iret_descriptor_access_updates_revoke_aliasing_proofs() {
        let mut ctx = paged_context(&[0x48, 0xcf]);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr4, 0);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestGdtrBase, 0x9000);
        ctx.vmcs_setup()
            .set_field32(VmcsField32::GuestGdtrLimit, 0x17);
        ctx.memory[0x8008..0x8010].copy_from_slice(&8u64.to_le_bytes());
        ctx.memory[0x8020..0x8028].copy_from_slice(&16u64.to_le_bytes());
        ctx.memory[0x900d] = 0x9a;
        ctx.memory[0x9015] = 0x92;
        ctx.state_mut().svm_guard.tables[..4].copy_from_slice(&[0x3000, 0x4000, 0x5000, 0x6000]);
        ctx.state_mut().svm_guard.count = 4;
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(scalar_iret_preserves_guard(&ctx, &window));
        ctx.state_mut().svm_guard.code[0].page = 0x9000;
        ctx.state_mut().svm_guard.code_count = 1;
        assert!(!scalar_iret_preserves_guard(&ctx, &window));
        ctx.memory[0x900d] |= 1;
        ctx.memory[0x9015] |= 1;
        assert!(scalar_iret_preserves_guard(&ctx, &window)); // No descriptor write is needed.
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestGdtrBase, 0x6380);
        assert!(!scalar_iret_preserves_guard(&ctx, &window)); // Descriptor would update a table frame.
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestGdtrBase, 0x9000);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr4, 1 << 23);
        assert!(!scalar_iret_preserves_guard(&ctx, &window)); // CET writes need separate validation.
    }

    #[test]
    fn outgoing_branches_stop_before_unknown_targets_and_respect_trap_capacity() {
        // ENDBR and a jump over REP: the unreachable REP need not be decoded.
        let mut ctx = paged_context(&[0xf3, 0x0f, 0x1e, 0xfa, 0xeb, 0x2a, 0xf3, 0xa4]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_verified(&ctx, true, false, true, &window).unwrap();
        assert_eq!(batch.count, 2);
        assert_eq!(&batch.branch_exits[..batch.branch_exit_count], &[0x1030]);
        assert!(batch.is_boundary(0x1030) && batch.is_execution_stop(0x1030));
        assert!(!batch.is_boundary(0x102f));
        // The same target reached after different prefix lengths needs a counter.
        ctx.memory[0x1000..0x1008].copy_from_slice(&[0x90, 0x90, 0x74, 4, 0x74, 2, 0xf3, 0xa4]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let duplicate = prepare_verified(&ctx, true, false, true, &window).unwrap();
        assert!(duplicate.uses_counter);
        assert_eq!(
            &duplicate.branch_exits[..duplicate.branch_exit_count],
            &[0x1008]
        );
        // Four distinct unknown entries exceed the three outgoing trap slots.
        ctx.memory[0x1000..0x100c].copy_from_slice(&[
            0x90, 0x90, 0x74, 0x70, 0x74, 0x70, 0x74, 0x70, 0x74, 0x70, 0xf3, 0xa4,
        ]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_verified(&ctx, true, false, true, &window).unwrap();
        assert!(!batch.uses_counter && batch.branch_exit_count == 0 && batch.count == 2);
    }

    #[test]
    fn decoded_memory_loops_guard_every_table_and_reject_code_table_aliases() {
        // Changing RDI and counter payloads prevent static destination proofs.
        let code = [
            0x48, 0x89, 0x0f, 0x48, 0x8d, 0x7f, 8, 0x48, 0xff, 0xc9, 0x75, 0xf4,
        ];
        let mut ctx = paged_context(&code);
        ctx.state_mut().gprs.rcx = 100;
        for offset in [0x1800, 0x1820, 0x1840, 0x1860, 0x1880] {
            ctx.memory[offset..offset + 3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        }
        for (address, entry) in [(0x3008, 0x8007u64), (0x8000, 0x9007), (0x9000, 0xa007)] {
            ctx.memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.guarded_stores && batch.uses_counter && !batch.counter_bounded);
        assert!(!batch.page_execution && !batch.validated_stores && batch.counted_loop.is_none());
        assert!(!prepare(&mut ctx, true, false, &window).is_some_and(|b| b.guarded_stores));
        assert!(ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&0xa000));
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator,
            bedrock_ept::PageTableFormat::AmdNpt,
        )
        .unwrap();
        let mappings = [
            0x1000, 0x3000, 0x4000, 0x5000, 0x6000, 0x7000, 0x8000, 0x9000, 0xa000,
        ];
        for page in mappings {
            ctx.state_mut()
                .ept
                .map_4k(
                    &mut allocator,
                    GuestPhysAddr::new(page),
                    HostPhysAddr::new(page + 0x1000000),
                    bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
                    bedrock_ept::EptMemoryType::WriteBack,
                )
                .unwrap();
        }
        let guard = protect(&mut ctx, &allocator, &batch).unwrap();
        for page in mappings {
            let permissions = ctx
                .state()
                .ept
                .lookup(&allocator, GuestPhysAddr::new(page))
                .unwrap()
                .1
                .bits();
            assert_eq!(permissions & 2 != 0, page == 0x7000);
            assert_ne!(permissions & 4, 0); // This guard does not restrict instruction fetch.
        }
        ctx.vmcs_setup().set_field32(VmcsField32::VmExitReason, 48);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::ExitQualification, 2);
        ctx.state()
            .vmcs
            .write64(VmcsField64::GuestPhysicalAddr, 0xa000)
            .unwrap();
        assert!(guard.restore(&mut ctx, &allocator)); // Replay the table write with stepping.
        for page in mappings {
            assert_eq!(
                ctx.state()
                    .ept
                    .lookup(&allocator, GuestPhysAddr::new(page))
                    .unwrap()
                    .1,
                bedrock_ept::EptPermissions::READ_WRITE_EXECUTE
            );
        }
        // A decoded instruction stream may not alias an unrelated table frame.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x9000..0x9008].copy_from_slice(&0x1007u64.to_le_bytes());
        assert!(prepare(&mut ctx, true, true, &window).is_none());
    }

    #[test]
    fn counted_store_loops_prove_every_destination_and_translation() {
        let code = [
            0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f, 8, 0x48, 0xff, 0xc9, 0x75, 0xf4, 0x0f, 0x01, 0xd9,
        ];
        let mut ctx = paged_context(&code);
        ctx.state_mut().gprs.rcx = 100;
        let b = planned(&ctx).unwrap();
        assert!(b.counted_loop.is_some() && b.validated_stores && !b.uses_counter);
        assert_eq!(b.count, 4);
        for address in [0x1000, 0x3000, 0x4000, 0x5000] {
            ctx.state_mut().gprs.rdi = address;
            assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        }
        ctx.state_mut().gprs.rdi = 0x7000;
        ctx.state_mut().gprs.rcx = 0;
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        ctx.state_mut().gprs.rcx = u64::MAX;
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
    }

    #[test]
    fn narrow_counted_loops_reject_payload_aliases_and_zero_low_counts() {
        for (opcode, register) in [(0xc9, 1), (0xca, 2)] {
            let code = [
                0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f, 8, 0xff, opcode, 0x75, 0xf5,
            ];
            let mut ctx = paged_context(&code);
            ctx.state_mut().gprs.rcx = 0x100000064;
            ctx.state_mut().gprs.rdx = 0x100000064;
            let batch = planned(&ctx).unwrap().counted_loop.unwrap();
            assert_eq!(
                (batch.register, batch.width, batch.iterations),
                (register, 32, 100)
            );
            // Counter payload before DEC, including CH/DH.
            for store in [
                [0x48, 0x89, (register << 3) | 7],
                [0x66, 0x88, ((register + 4) << 3) | 7],
            ] {
                ctx.memory[0x1000..0x1003].copy_from_slice(&store);
                assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
            }
            ctx.memory[0x1000..0x1003].copy_from_slice(&code[..3]);
            ctx.state_mut().gprs.rcx = 1 << 32;
            ctx.state_mut().gprs.rdx = 1 << 32;
            assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        }
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
        assert!(b.counted_loop.is_some() && b.validated_stores && !b.uses_counter);
        assert_eq!(b.count, 11);
        assert_eq!(b.offsets[b.count] as usize, length);

        // A later destination aliases a translation table, even though the
        // first destination does not. Reject the whole loop.
        ctx.memory[0x6040..0x6048].copy_from_slice(&0x6007u64.to_le_bytes());
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        // A noncontiguous mapping requires a different proof.
        ctx.memory[0x6040..0x6048].copy_from_slice(&0xa007u64.to_le_bytes());
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        ctx.memory[0x6040..0x6048].copy_from_slice(&0x8007u64.to_le_bytes());
        assert!(planned(&ctx).unwrap().counted_loop.is_some());
        // Moving stores after the pointer advance invalidates the range.
        ctx.memory[0x1000..0x1004].copy_from_slice(&[0x48, 0x8d, 0x7f, 64]);
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
    }

    #[test]
    fn counted_loops_fit_inside_deadline_margin_and_reject_rcx_payloads() {
        let code = [
            0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f, 8, 0x48, 0xff, 0xc9, 0x75, 0xf4,
        ];
        let mut ctx = paged_context(&code);
        ctx.state_mut().gprs.rcx = 100;
        ctx.state_mut().stop_at_tsc = Some(13);
        let b = planned(&ctx).unwrap();
        assert_eq!(b.counted_loop.unwrap().iterations, 3);
        assert!(!b.uses_counter);
        for store in [[0x48, 0x89, 0x0f], [0x40, 0x88, 0x0f]] {
            ctx.memory[0x1000..0x1003].copy_from_slice(&store);
            assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        }
        // REX.R selects R9 rather than RCX and must remain eligible.
        ctx.memory[0x1000..0x1003].copy_from_slice(&[0x4c, 0x89, 0x0f]);
        assert!(planned(&ctx).unwrap().counted_loop.is_some());
        let mut ctx = paged_context(&[
            0x88, 0x2f, 0x48, 0x8d, 0x7f, 8, 0x48, 0xff, 0xc9, 0x75, 0xf5,
        ]);
        ctx.state_mut().gprs.rcx = 100;
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some())); // CH
    }

    #[test]
    fn new_run_revokes_code_approvals_after_userspace_changes() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(
            !prepare(&mut ctx, true, false, &window)
                .unwrap()
                .page_execution
        );
        let b = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(&b.pages[..b.page_count], &[0x1000]);
        assert_eq!(
            &ctx.state().svm_guard.tables[..ctx.state().svm_guard.count],
            &[0x3000, 0x4000, 0x5000, 0x6000]
        );
        assert!(
            prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
        // A new RUN revokes approvals before userspace changes become visible,
        // including changes outside the small instruction window.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x1ff0..0x1ff3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        assert!(
            !prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
        assert!(ctx.state().svm_rejected_pages.contains(&0x1000));
        ctx.memory[0x1ff0..0x1ff3].fill(0x90);
        assert!(
            !prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
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
        let batch = prepare_verified(&ctx, true, true, false, &window).unwrap();
        assert!(batch.page_execution && batch.uses_counter);
        ctx.state_mut().stop_at_tsc = Some(InstructionBatch::COUNTER_DEADLINE_MARGIN);
        assert!(
            !prepare_verified(&ctx, true, true, false, &window).is_some_and(|b| b.page_execution)
        );
    }

    #[test]
    fn wide_page_sets_keep_the_four_breakpoint_limit() {
        extern crate std;
        let mut ctx = paged_context(&[0x90]);
        ctx.memory.resize(0x40000, 0);
        ctx.memory[0x10000..0x20000].fill(0x90);
        for index in 0..SVM_CODE_PAGE_CAPACITY {
            let virtual_page = (index + 1) * 4096;
            let physical_page = 0x10000 + index * 4096;
            ctx.memory[0x6000 + (index + 1) * 8..0x6008 + (index + 1) * 8]
                .copy_from_slice(&(physical_page as u64 | 7).to_le_bytes());
            ctx.state_mut().svm_recent_pages[index] = virtual_page as u64;
        }
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(batch.code_page_count, SVM_CODE_PAGE_CAPACITY);
        assert_eq!(batch.page_breakpoint_count, 0);
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator,
            bedrock_ept::PageTableFormat::AmdNpt,
        )
        .unwrap();
        let mappings: std::vec::Vec<_> = batch.pages[..batch.page_count]
            .iter()
            .chain(ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].iter())
            .copied()
            .collect();
        for page in mappings {
            ctx.state_mut()
                .ept
                .map_4k(
                    &mut allocator,
                    GuestPhysAddr::new(page),
                    HostPhysAddr::new(page + 0x1000000),
                    bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
                    bedrock_ept::EptMemoryType::WriteBack,
                )
                .unwrap();
        }
        let guard = protect(&mut ctx, &allocator, &batch).unwrap();
        guard.restore(&mut ctx, &allocator);
        for index in 0..5 {
            let address = 0x10100 + index * 4096;
            ctx.memory[address..address + 3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        }
        ctx.state_mut().svm_guard.valid = false;
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(batch.page_breakpoint_count, 4);
        assert!(!batch.pages[..batch.code_page_count].contains(&0x14000));
    }

    #[test]
    fn code_selection_keeps_safe_pages_when_hazards_exhaust_breakpoints() {
        let mut ctx = paged_context(&[0x90]);
        for page in [0x7000, 0x8000, 0x9000] {
            ctx.memory[page + 0x100..page + 0x104].copy_from_slice(&[0x48, 0x0f, 0xc7, 0xf0]);
        }
        ctx.state_mut().svm_recent_pages[..4].copy_from_slice(&[0x7000, 0x8000, 0x9000, 0xa000]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(batch.page_breakpoint_count, 4);
        assert_eq!(batch.code_page_count, 4);
        assert!(batch.pages[..batch.code_page_count].contains(&0xa000));
        assert!(!batch.pages[..batch.code_page_count].contains(&0x9000));
    }

    #[test]
    fn optional_pages_with_excess_hazards_are_rejected_between_plans() {
        let mut ctx = paged_context(&[0x90]);
        for offset in (0..80).step_by(16) {
            ctx.memory[0x9000 + offset..0x9004 + offset].copy_from_slice(&[0x48, 0x0f, 0xc7, 0xf0]);
        }
        ctx.state_mut().svm_recent_pages[1] = 0x9000;
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let first = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(first.page_execution);
        assert_eq!(first.code_page_count, 1);
        assert!(ctx.state().svm_rejected_pages.contains(&0x9000));
        let cursor = ctx.state().svm_rejected_cursor;
        ctx.state_mut().svm_guard.valid = false;
        let second = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(second.page_execution);
        assert_eq!(second.code_page_count, 1);
        assert_eq!(ctx.state().svm_rejected_cursor, cursor);
    }

    #[test]
    fn page_sets_revalidate_contents_and_cross_page_prefixes() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x3000].fill(0x90);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        ctx.state_mut().svm_recent_pages[0] = 0x2000;
        let b = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(b.code_page_count, 2);
        assert_eq!(&b.pages[..2], &[0x1000, 0x2000]);
        // Each page is safe alone, but either virtual adjacency could execute
        // RDRAND across their physical boundary. Exclude the optional page.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x1ffe..0x2000].copy_from_slice(&[0x0f, 0xc7]);
        ctx.memory[0x2000] = 0xf0;
        assert!(page_safe(&ctx, 0x1000).is_some());
        assert!(page_safe(&ctx, 0x2000).is_some());
        assert_eq!(
            prepare(&mut ctx, true, true, &window)
                .unwrap()
                .code_page_count,
            1
        );
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x1ffe..0x2003].fill(0x90);
        ctx.memory[0x2100..0x2103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let b = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(b.code_page_count, 2);
        assert_eq!(&b.page_breakpoints[..b.page_breakpoint_count], &[0x2100]);
    }

    #[test]
    fn code_cache_drops_pages_before_they_can_be_written_as_data() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x3000].fill(0x90);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        ctx.state_mut().svm_recent_pages[0] = 0x2000;
        let mut batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(ctx.state().svm_guard.code_count, 2);
        batch.code_page_count = 1;
        batch.page_count = 1;
        retain_translation_cache(&mut ctx, Some(&batch), Some(&window), false);
        assert_eq!(ctx.state().svm_guard.code_count, 1);
        assert_eq!(ctx.state().svm_guard.code[0].page, 0x1000);
        // The omitted page is writable during this entry, even with the
        // translation tree protected. Its next inclusion must scan again.
        ctx.memory[0x2100..0x2103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let next = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(next.code_page_count, 2);
        assert_eq!(
            &next.page_breakpoints[..next.page_breakpoint_count],
            &[0x2100]
        );
    }

    #[test]
    fn scalar_code_writes_revoke_scans_without_revoking_disjoint_tables() {
        for (opcode, address) in [(&[0x48, 0x89, 0x07][..], 0x1ff0), (&[0x50][..], 0x1ff8)] {
            let mut ctx = paged_context(&[0x90]);
            ctx.memory[0x1000..0x2000].fill(0x90);
            let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
            prepare(&mut ctx, true, true, &window).unwrap();
            let mut store = super::super::svm::InstructionWindow::read(&ctx).unwrap();
            store.bytes[..opcode.len()].copy_from_slice(opcode);
            ctx.state_mut().gprs.rdi = address;
            ctx.vmcs_setup()
                .set_field_natural(VmcsFieldNatural::GuestRsp, address);
            retain_translation_cache(&mut ctx, None, Some(&store), false);
            assert!(ctx.state().svm_guard.valid);
            assert_eq!(ctx.state().svm_guard.code_count, 0);
            ctx.memory[0x1ff0..0x1ff3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
            assert!(
                !prepare(&mut ctx, true, true, &window)
                    .unwrap()
                    .page_execution
            );
        }
    }

    #[test]
    fn page_execution_guards_tables_outside_the_code_translation_path() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        // A second PML4 branch and a separate data-translation hierarchy.
        for (address, entry) in [(0x3008, 0x8007u64), (0x8000, 0x9007), (0x9000, 0xa007)] {
            ctx.memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let b = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(b.page_execution);
        for table in [0x3000, 0x4000, 0x5000, 0x6000, 0x8000, 0x9000, 0xa000] {
            assert!(ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&table));
        }
        // A code page reused as an unrelated page table cannot run freely.
        let mut store = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        store.bytes[..3].copy_from_slice(&[0x48, 0x89, 0x07]);
        ctx.state_mut().gprs.rdi = 0x9000;
        retain_translation_cache(&mut ctx, None, Some(&store), false);
        assert!(!ctx.state().svm_guard.valid);
        ctx.memory[0x9000..0x9008].copy_from_slice(&0x1007u64.to_le_bytes());
        assert!(!prepare(&mut ctx, true, true, &window).is_some_and(|b| b.page_execution));
    }

    #[test]
    fn page_execution_accepts_large_trees_and_falls_back_at_workspace_capacity() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory.resize(1024 * 1024, 0);
        ctx.memory[0x1000..0x2000].fill(0x90);
        for index in 1..91 {
            let table = 0x7000 + index * 4096;
            ctx.memory[0x5000 + index * 8..0x5008 + index * 8]
                .copy_from_slice(&(table as u64 | 7).to_le_bytes());
        }
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(
            prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
        assert_eq!(ctx.state().svm_guard.count, 94);
        assert!(ctx.state().svm_guard.tables[..94].contains(&(0x7000 + 90 * 4096)));
        // Simulate returning to userspace before changing RAM.
        ctx.state_mut().svm_guard.valid = false;
        for index in 91..128 {
            let table = 0x7000 + index * 4096;
            ctx.memory[0x5000 + index * 8..0x5008 + index * 8]
                .copy_from_slice(&(table as u64 | 7).to_le_bytes());
        }
        assert!(
            !prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
    }

    #[test]
    fn hazardous_pages_cover_aliases_in_large_executable_trees() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory.resize(1024 * 1024, 0);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        for index in 1..91 {
            let table = 0x7000 + index * 4096;
            ctx.memory[0x5000 + index * 8..0x5008 + index * 8]
                .copy_from_slice(&(table as u64 | 7).to_le_bytes());
        }
        // The same hazardous physical page also appears at a distant GVA.
        ctx.memory[0x8000..0x8008].copy_from_slice(&0x1007u64.to_le_bytes());
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(ctx.state().svm_guard.count, 94);
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x1100, 0x200100]
        );
    }

    #[test]
    fn edge_summaries_match_the_byte_scanner_for_crossing_opcodes_and_prefixes() {
        let check = |suffix: &[u8], prefix: &[u8]| {
            let mut left = PageHazards {
                boundary: [0x90; 32],
                edge: 0,
                offsets: [0; 4],
                count: 0,
            };
            let mut right = left;
            left.boundary[32 - suffix.len()..].copy_from_slice(suffix);
            right.boundary[..prefix.len()].copy_from_slice(prefix);
            // The fast comparison requires independently safe edge halves.
            if forbidden_page_bytes(&left.boundary[16..])
                || forbidden_page_bytes(&right.boundary[..16])
            {
                return;
            }
            left.edge = summarize_edge(&left.boundary);
            right.edge = summarize_edge(&right.boundary);
            let mut joined = [0; 32];
            joined[..16].copy_from_slice(&left.boundary[16..]);
            joined[16..].copy_from_slice(&right.boundary[..16]);
            assert_eq!(
                hazard_boundary_safe(&left, &right),
                !forbidden_page_bytes(&joined),
                "suffix={suffix:x?} prefix={prefix:x?}"
            );
        };
        for byte in 0..=255u8 {
            check(&[0x0f, 0xc7], &[byte]);
            check(&[0x0f], &[0xc7, byte]);
            check(&[0x0f], &[byte]);
            for left_prefixes in 0..14 {
                for right_prefixes in 0..15 {
                    let mut suffix = [0x66; 14];
                    suffix[0] = 0xf3;
                    let mut prefix = [0x48; 16];
                    prefix[right_prefixes] = byte;
                    check(&suffix[..left_prefixes + 1], &prefix[..right_prefixes + 1]);
                }
            }
        }
    }

    #[test]
    fn alias_proofs_ignore_safe_code_pages_and_selection_order() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        ctx.state_mut().svm_recent_pages[..2].copy_from_slice(&[0x7000, 0x8000]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let first = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(first.page_breakpoint_count, 1);
        assert_eq!(ctx.state().svm_guard.alias_proof.page_count, 1);
        // The traversal scratch is not proof state. A cache hit must not
        // restart it, even when the harmless selected pages and order change.
        ctx.state_mut().svm_guard.aliases[0].table = 0;
        ctx.set_guest_rip(0x7000);
        ctx.state_mut().svm_recent_pages[..2].copy_from_slice(&[0x1000, 0x9000]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let second = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(second.page_breakpoint_count, 1);
        assert_eq!(second.page_breakpoints[0], 0x1100);
        assert!(second.pages[..second.code_page_count].contains(&0x9000));
        assert_eq!(ctx.state().svm_guard.aliases[0].table, 0);
    }

    #[test]
    fn alias_proofs_rebuild_after_table_and_hazard_changes() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x1100]
        );
        assert!(ctx.state().svm_guard.alias_proof.valid);
        let mut store = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        store.bytes[..3].copy_from_slice(&[0x48, 0x89, 0x07]);
        ctx.state_mut().gprs.rdi = 0x6048;
        retain_translation_cache(&mut ctx, None, Some(&store), false);
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x1007u64.to_le_bytes());
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x1100, 0x9100]
        );
        // The tree is intact, but a code write changes breakpoint offsets.
        ctx.state_mut().gprs.rdi = 0x1100;
        retain_translation_cache(&mut ctx, None, Some(&store), false);
        assert!(ctx.state().svm_guard.valid);
        ctx.memory[0x1100..0x1103].fill(0x90);
        ctx.memory[0x1200..0x1203].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x1200, 0x9200]
        );
    }

    #[test]
    fn endbr64_retains_proofs_and_bounds_the_decoded_instruction() {
        let bytes = [0xf3, 0x0f, 0x1e, 0xfa, 0x48, 0x89, 0x07];
        assert_eq!(safe_len(&bytes, true, false), Some((4, false, false)));
        assert_eq!(modified_gprs(&bytes, true), 0);
        let mut ctx = paged_context(&bytes);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        prepare(&mut ctx, true, true, &window).unwrap();
        retain_translation_cache(&mut ctx, None, Some(&window), false);
        assert!(ctx.state().svm_guard.valid);
        assert!(ctx.state().svm_guard.code_count > 0);
        for other in [[0xf3, 0x0f, 0x1e, 0xfb], [0xf3, 0x0f, 0x1e, 0xf8]] {
            assert_eq!(safe_len(&other, true, false), None);
        }
    }

    #[test]
    fn popf_and_port_io_keep_ram_proofs_until_event_delivery() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        let mut window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        prepare(&mut ctx, true, true, &window).unwrap();
        let code_count = ctx.state().svm_guard.code_count;
        for opcode in [0x9d, 0xe4, 0xe5, 0xe6, 0xe7, 0xec, 0xed, 0xee, 0xef] {
            window.bytes[0] = opcode;
            retain_translation_cache(&mut ctx, None, Some(&window), false);
            assert!(ctx.state().svm_guard.valid);
            assert_eq!(ctx.state().svm_guard.code_count, code_count);
        }
        // INS/OUTS are not classified as scalar register/device operations.
        window.bytes[0] = 0x6c;
        retain_translation_cache(&mut ctx, None, Some(&window), false);
        assert!(!ctx.state().svm_guard.valid);
        prepare(&mut ctx, true, true, &window).unwrap();
        window.bytes[0] = 0xec;
        // A pending guest event can write an interrupt frame before IN runs.
        retain_translation_cache(&mut ctx, None, Some(&window), true);
        assert!(!ctx.state().svm_guard.valid);
    }

    #[test]
    fn conditional_branches_retain_table_and_code_proofs() {
        for condition in 0..16 {
            for near in [false, true] {
                let mut ctx = paged_context(&[0x90]);
                ctx.memory[0x1000..0x2000].fill(0x90);
                let mut window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
                prepare(&mut ctx, true, true, &window).unwrap();
                let code_count = ctx.state().svm_guard.code_count;
                assert!(code_count > 0);
                if near {
                    window.bytes[..6].copy_from_slice(&[
                        0x0f,
                        0x80 + condition,
                        0xfa,
                        0xff,
                        0xff,
                        0xff,
                    ]);
                } else {
                    window.bytes[..2].copy_from_slice(&[0x70 + condition, 0xfe]);
                }
                retain_translation_cache(&mut ctx, None, Some(&window), false);
                assert!(ctx.state().svm_guard.valid);
                assert_eq!(ctx.state().svm_guard.code_count, code_count);
                // CALL still writes a return address; it must not inherit
                // the memory-free classification of conditional branches.
                window.bytes[..5].copy_from_slice(&[0xe8, 0, 0, 0, 0]);
                retain_translation_cache(&mut ctx, None, Some(&window), false);
                assert!(!ctx.state().svm_guard.valid);
                assert_eq!(ctx.state().svm_guard.code_count, 0);
            }
        }
    }

    #[test]
    fn translation_cache_tracks_root_and_invalidates_before_unprotected_writes() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        retain_translation_cache(&mut ctx, Some(&batch), Some(&window), false);
        assert!(ctx.state().svm_guard.valid);
        retain_translation_cache(&mut ctx, None, Some(&window), false);
        assert!(ctx.state().svm_guard.valid);
        // Emulation may write interrupt frames or device data into tables.
        retain_translation_cache(&mut ctx, None, Some(&window), true);
        assert!(!ctx.state().svm_guard.valid);
        prepare(&mut ctx, true, true, &window).unwrap();
        ctx.memory.copy_within(0x3000..0x4000, 0x8000);
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x8000);
        prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(ctx.state().svm_guard.root, 0x8000);
        assert_eq!(ctx.state().svm_guard.tables[0], 0x8000);
        let mut store = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        store.bytes[..3].copy_from_slice(&[0x48, 0x89, 0x07]);
        ctx.state_mut().gprs.rdi = 0x6000;
        retain_translation_cache(&mut ctx, None, Some(&store), false);
        assert!(!ctx.state().svm_guard.valid);
        prepare(&mut ctx, true, true, &window).unwrap();
        // Unsupported instructions cannot retain the proof while unguarded.
        store.bytes[..2].copy_from_slice(&[0x0f, 0x0b]);
        retain_translation_cache(&mut ctx, None, Some(&store), false);
        assert!(!ctx.state().svm_guard.valid);
    }

    #[test]
    fn scalar_rip_relative_stores_use_instruction_end() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        let mut window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        prepare(&mut ctx, true, true, &window).unwrap();
        window.bytes[..3].copy_from_slice(&[0x48, 0x89, 0x05]);
        // Eight bytes straddle the table's end. The old window-based address
        // incorrectly placed this write entirely in unprotected data RAM.
        window.bytes[3..7].copy_from_slice(&(0x6ffci32 - 0x1007).to_le_bytes());
        assert!(!scalar_store_preserves_guard(&ctx, &window, false));
        retain_translation_cache(&mut ctx, None, Some(&window), false);
        assert!(!ctx.state().svm_guard.valid);

        prepare(&mut ctx, true, true, &window).unwrap();
        window.bytes[3..7].copy_from_slice(&(0x7000i32 - 0x1007).to_le_bytes());
        assert!(scalar_store_preserves_guard(&ctx, &window, false));
        window.bytes[3..7].copy_from_slice(&(0x1000i32 - 0x1007).to_le_bytes());
        assert!(!scalar_store_preserves_guard(&ctx, &window, true));
        retain_translation_cache(&mut ctx, None, Some(&window), false);
        assert!(ctx.state().svm_guard.valid);
        assert_eq!(ctx.state().svm_guard.code_count, 0);
    }

    #[test]
    fn scalar_store_proofs_cover_both_pages_stack_writes_and_physical_aliases() {
        for (address, keep) in [
            (0x7000, true),
            (0x6000, false),
            (0x7ffc, true),
            (0x5ffc, false),
        ] {
            let mut ctx = paged_context(&[0x48, 0x89, 0x07]);
            let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
            let batch = prepare(&mut ctx, true, true, &window).unwrap();
            assert!(batch.page_execution);
            ctx.state_mut().gprs.rdi = address;
            retain_translation_cache(&mut ctx, None, Some(&window), false);
            assert_eq!(ctx.state().svm_guard.valid, keep, "address={address:#x}");
        }
        let mut ctx = paged_context(&[0x48, 0x89, 0x07]);
        ctx.memory[0x6038..0x6040].copy_from_slice(&0x6007u64.to_le_bytes());
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        prepare(&mut ctx, true, true, &window).unwrap();
        ctx.state_mut().gprs.rdi = 0x7000; // GVA data alias to a protected table.
        retain_translation_cache(&mut ctx, None, Some(&window), false);
        assert!(!ctx.state().svm_guard.valid);
        for (rsp, keep) in [(0x8008, true), (0x6008, false)] {
            let mut ctx = paged_context(&[0x50]);
            let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
            prepare(&mut ctx, true, true, &window).unwrap();
            ctx.vmcs_setup()
                .set_field_natural(VmcsFieldNatural::GuestRsp, rsp);
            retain_translation_cache(&mut ctx, None, Some(&window), false);
            assert_eq!(ctx.state().svm_guard.valid, keep, "rsp={rsp:#x}");
        }
    }

    #[test]
    fn unsafe_entries_cover_prefixes_and_every_executable_alias() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1100..0x1105].copy_from_slice(&[0x66, 0x48, 0x0f, 0xc7, 0xf0]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let b = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(b.page_execution);
        for address in [0x1100, 0x1101, 0x1102] {
            assert!(b.page_breakpoints[..b.page_breakpoint_count].contains(&address));
        }
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x1007u64.to_le_bytes());
        // Three hazards, two executable aliases exceed four debug registers.
        assert!(
            !prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x6048..0x6050].copy_from_slice(&(0x1007u64 | (1 << 63)).to_le_bytes());
        ctx.state_mut().svm_rejected_pages.fill(u64::MAX);
        assert!(
            prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x1100..0x1105].copy_from_slice(&[0xf3, 0x66, 0x48, 0xa4, 0x90]);
        let b = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(&b.page_breakpoints[..b.page_breakpoint_count], &[0x1100]);
    }

    #[test]
    fn hazard_memos_recheck_changed_bytes_after_proof_invalidation() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        assert_eq!(cached_page_hazards(&mut ctx, 0x1000).unwrap().count, 0);
        ctx.state_mut().svm_guard.valid = false;
        assert_eq!(cached_page_hazards(&mut ctx, 0x1000).unwrap().count, 0);
        // New hazards crossing a chunk boundary must invalidate the memo.
        ctx.memory[0x11ff..0x1203].copy_from_slice(&[0x48, 0x0f, 0xc7, 0xf0]);
        ctx.state_mut().svm_guard.valid = false;
        let hazards = cached_page_hazards(&mut ctx, 0x1000).unwrap();
        assert_eq!(&hazards.offsets[..hazards.count], &[0x1ff, 0x200]);
        // Page-edge hazards must also be rescanned and rejected.
        ctx.memory[0x1ffd..0x2000].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        ctx.state_mut().svm_guard.valid = false;
        assert!(cached_page_hazards(&mut ctx, 0x1000).is_none());
    }

    #[test]
    fn hazard_prefixes_are_found_across_scan_chunks() {
        let mut ctx = paged_context(&[0x90]);
        for offset in [128, 254, 255, 256, 510, 511, 768, 4000] {
            for (bytes, expected) in [
                (&[0x66, 0x48, 0x0f, 0xc7, 0xf0][..], &[0, 1, 2][..]),
                (&[0xf3, 0x66, 0x48, 0xa4], &[0]),
                (&[0x48, 0x0f, 0x07], &[0, 1]),
            ] {
                ctx.memory[0x1000..0x2000].fill(0x90);
                ctx.memory[0x1000 + offset..0x1000 + offset + bytes.len()].copy_from_slice(bytes);
                let hazards = page_hazards(&ctx, 0x1000).unwrap();
                assert_eq!(hazards.count, expected.len());
                for &prefix in expected {
                    assert!(hazards.offsets[..hazards.count].contains(&((offset + prefix) as u16)));
                }
            }
        }
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
            counted_loop: None,
            pages: [0; 20],
            code_page_count: 1,
            page_breakpoints: [0; 4],
            page_breakpoint_count: 0,
            branch_exits: [0; 3],
            branch_exit_count: 0,
            branch_exit_counts: [0; 3],
            page_count: 0,
            accesses_memory: false,
            writes_memory: false,
            validated_stores: false,
            guarded_stores: false,
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
            counted_loop: None,
            pages: [0; 20],
            code_page_count: 1,
            page_breakpoints: [0; 4],
            page_breakpoint_count: 0,
            branch_exits: [0; 3],
            branch_exit_count: 0,
            branch_exit_counts: [0; 3],
            page_count: 0,
            accesses_memory: false,
            writes_memory: false,
            validated_stores: false,
            guarded_stores: false,
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
