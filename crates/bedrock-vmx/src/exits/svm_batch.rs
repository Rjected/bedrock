// SPDX-License-Identifier: GPL-2.0
//! Bounded instruction sequences for AMD hardware execution.

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
use super::super::traits::{CountedLoopBatch, CowAllocator, InstructionBatch, RepeatBatch};
use super::super::vm_state::{SvmAliasEdge, SVM_ALIAS_EDGE_CAPACITY, SVM_ALIAS_PROOF_CAPACITY, SVM_ALIAS_PROOF_PAGE_CAPACITY, SVM_ALIAS_PROOF_WAYS, SVM_CODE_PAGE_CAPACITY, SVM_LEAF_PAGE_INDEX_CAPACITY, SVM_RECENT_PAGE_CAPACITY, SVM_RETAINED_GUARD_CAPACITY, SVM_TABLE_CAPACITY};
use core::sync::atomic::{AtomicU64, Ordering};

static REFRESH_PENDING: AtomicU64 = AtomicU64::new(0);
static REFRESH_LEAF: AtomicU64 = AtomicU64::new(0);
static REFRESH_REARM: AtomicU64 = AtomicU64::new(0);
static REFRESH_FULL: AtomicU64 = AtomicU64::new(0);
static COLLECT_CALLS: AtomicU64 = AtomicU64::new(0);
static COLLECT_HITS: AtomicU64 = AtomicU64::new(0);
static COLLECT_WALKS: AtomicU64 = AtomicU64::new(0);
static COLLECT_WALK_SUCCESS: AtomicU64 = AtomicU64::new(0);
static COLLECT_WALK_BREAKPOINT_LIMIT: AtomicU64 = AtomicU64::new(0);
static COLLECT_WALK_WORKSPACE_LIMIT: AtomicU64 = AtomicU64::new(0);
static COLLECT_WALK_READ_FAIL: AtomicU64 = AtomicU64::new(0);
static COLLECT_VALID_PROOF_MISSES: AtomicU64 = AtomicU64::new(0);
static COLLECT_DIRTY: AtomicU64 = AtomicU64::new(0);
static COLLECT_UNGUARDED: AtomicU64 = AtomicU64::new(0);
static LEAF_CHANGED_ENTRIES: AtomicU64 = AtomicU64::new(0);
static LEAF_INVALIDATED_PROOFS: AtomicU64 = AtomicU64::new(0);
static LEAF_SNAPSHOT_MISS: AtomicU64 = AtomicU64::new(0);
static INVALIDATE_TREE: AtomicU64 = AtomicU64::new(0);
static INVALIDATE_CODE: AtomicU64 = AtomicU64::new(0);
static FULL_PRESERVE: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_GUARD: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_NOT_READY: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_UNTRUSTED: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_ROOT: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_MAPPING: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_NO_PROOF: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_UPPER: AtomicU64 = AtomicU64::new(0);
static FULL_REJECT_NO_RELEASE: AtomicU64 = AtomicU64::new(0);
static TREE_CAP_FAIL: AtomicU64 = AtomicU64::new(0);
static TREE_SCAN_FAIL: AtomicU64 = AtomicU64::new(0);
static TREE_SCAN_MAX_COUNT: AtomicU64 = AtomicU64::new(0);
static TREE_RESTRICT_ERR: AtomicU64 = AtomicU64::new(0);
static TREE_RESTRICT_NONE: AtomicU64 = AtomicU64::new(0);
static TREE_READY_SUCCESS: AtomicU64 = AtomicU64::new(0);
static FULL_SWITCH_ROOT: AtomicU64 = AtomicU64::new(0);
static FULL_COLD_GATE: AtomicU64 = AtomicU64::new(0);
static FULL_DIRTY_GATE: AtomicU64 = AtomicU64::new(0);
static ROOT_PROOF_PRESERVED: AtomicU64 = AtomicU64::new(0);
#[cfg(not(feature = "cargo"))]
use crate::ept::NptExecutionGuard;
#[cfg(feature = "cargo")]
use crate::prelude::*;
#[cfg(feature = "cargo")]
use bedrock_ept::NptExecutionGuard;

const _: () = assert!(SVM_CODE_PAGE_CAPACITY <= NptExecutionGuard::MAX_PAGES);
const _: () = assert!(SVM_TABLE_CAPACITY % 64 == 0);
const _: () = assert!(SVM_TABLE_CAPACITY.is_power_of_two() && SVM_TABLE_CAPACITY <= u16::MAX as usize);

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
        0xa8 => immediate = 1,       // TEST AL, imm8 does not write memory.
        0xa9 => immediate = operand, // TEST AX/EAX/RAX, imm16/imm32.
        0x63 if long => modrm = true, // MOVSXD writes only its register destination.
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

pub(crate) fn bounded_repeat_entry(bytes: &[u8]) -> bool {
    repeat_len(bytes).is_some()
}

fn relative_branch(bytes: &[u8], long: bool, default32: bool) -> Option<(usize, i64)> {
    // Linux return thunks commonly use CS:JMP (2e e9). The segment prefix
    // does not change a relative branch target in long mode.
    let prefix = usize::from(long && bytes.first() == Some(&0x2e));
    let first = *bytes.get(prefix)?;
    if matches!(first, 0x70..=0x7f | 0xeb) {
        return Some((prefix + 2, i64::from(*bytes.get(prefix + 1)? as i8)));
    }
    let opcode_length = if first == 0xe9 {
        prefix + 1
    } else if first == 0x0f && matches!(*bytes.get(prefix + 1)?, 0x80..=0x8f) {
        prefix + 2
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
        0x63 if long => register(false).unwrap_or(u16::MAX),
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

/// Address of a single-destination store whose address registers retain their
/// entry values. Other stores and address/FS/GS overrides terminate the batch.
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
    // Arithmetic and bit operations on memory have the same single write
    // destination as MOV. Their prior read is safe once validate_store proves
    // that earlier stores cannot change this destination's translation.
    let byte = matches!(op, 0x88 | 0xc6 | 0x80 | 0xc0 | 0xd0 | 0xd2 | 0xfe | 0xf6)
        || op < 0x38 && op & 7 == 0;
    let modrm_index = match op {
        0x88 | 0x89 | 0xc6 | 0xc7 | 0x80 | 0x81 | 0x83 | 0xc0 | 0xc1
        | 0xd0..=0xd3 | 0xfe | 0xff | 0xf6 | 0xf7 => p + 1,
        0x00..=0x37 if op & 7 <= 1 => p + 1,
        0x0f if matches!(bytes.get(p + 1), Some(0x90..=0x9f)) => p + 2,
        _ => return None,
    };
    let width = if byte || op == 0x0f {
        1
    } else if rex & 8 != 0 {
        8
    } else if word {
        2
    } else {
        4
    };
    let m = *bytes.get(modrm_index)?;
    let mode = m >> 6;
    if mode == 3 {
        return None;
    }
    let mut cursor = modrm_index + 1;
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
    collect_translation_tree_pages(ctx, &batch.pages[..batch.code_page_count])
}

fn add_translation_child<C: VmContext>(
    ctx: &mut C,
    parent: usize,
    level: u8,
    child: u64,
    code_pages: &[u64],
) -> Option<()> {
    if code_pages.contains(&child) {
        return None;
    }
    let scratch = &mut ctx.state_mut().svm_guard;
    let mask = scratch.table_index.len() - 1;
    let shift = 64 - scratch.table_index.len().trailing_zeros();
    let mut slot = ((child >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> shift) as usize;
    let index = loop {
        let entry = scratch.table_index[slot];
        if entry == 0 {
            if scratch.count == scratch.tables.len() {
                TREE_CAP_FAIL.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let index = scratch.count;
            scratch.tables[index] = child;
            scratch.levels[index] = level - 1;
            scratch.count += 1;
            scratch.table_index[slot] = (index + 1) as u16;
            break index;
        }
        let index = usize::from(entry - 1);
        if scratch.tables[index] == child {
            if scratch.levels[index] != level - 1 {
                return None;
            }
            break index;
        }
        slot = (slot + 1) & mask;
    };
    scratch.children[parent][index / 64] |= 1u64 << (index % 64);
    Some(())
}

fn collect_translation_tree_pages<C: VmContext>(ctx: &mut C, code_pages: &[u64]) -> Option<()> {
    let root = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestCr3)
        .ok()?
        & 0x000f_ffff_ffff_f000;
    if code_pages.contains(&root) {
        return None;
    }
    let scratch = &ctx.state().svm_guard;
    if scratch.valid && scratch.root == root {
        return (!code_pages
            .iter()
            .any(|page| scratch.tables[..scratch.count].contains(page)))
        .then_some(());
    }
    // The global gate already walked and write-protected every reachable
    // table. Reuse that complete tree when a bounded guard expires without a
    // table write, instead of reading every table page again on each batch.
    if scratch.gate_ready
        && !scratch.gate_dirty
        && scratch.gate_root == root
        && scratch.gate_count != 0
    {
        let count = scratch.gate_count;
        if code_pages
            .iter()
            .any(|page| scratch.gate_tables[..count].contains(page))
        {
            return None;
        }
        let scratch = &mut ctx.state_mut().svm_guard;
        scratch.tree_generation = scratch.tree_generation.wrapping_add(1);
        scratch.translation_count = 0;
        scratch.translation_cursor = 0;
        scratch.valid = true;
        scratch.root = root;
        scratch.count = count;
        for index in 0..count {
            scratch.tables[index] = scratch.gate_tables[index];
        }
        return Some(());
    }
    let preserve_alias_proofs = can_refresh_leaf_only(
        &ctx.state().svm_guard,
        root,
        ctx.state().ept.mapping_generation(),
    );
    // The retained NPT guards keep page tables from inactive CR3 roots
    // immutable. Their alias proofs remain valid while another root is scanned.
    let guarded_root_switch = {
        let guard = &ctx.state().svm_guard;
        guard.gate_ready && !guard.gate_dirty && !guard.gate_links_untrusted
            && guard.gate_root != root
            && guard.retained_guard_count != 0
            && guard.retained_mapping_generation == ctx.state().ept.mapping_generation()
            && guard.gate_guards[..guard.gate_count].iter().all(|saved| saved.valid)
    };
    if guarded_root_switch {
        ROOT_PROOF_PRESERVED.fetch_add(1, Ordering::Relaxed);
    }
    let scratch = &mut ctx.state_mut().svm_guard;
    scratch.tree_generation = scratch.tree_generation.wrapping_add(1);
    if (!scratch.gate_ready || scratch.gate_dirty || scratch.gate_root != root)
        && !preserve_alias_proofs && !guarded_root_switch
    {
        invalidate_alias_proofs(scratch);
    }
    scratch.valid = false;
    scratch.translation_count = 0;
    scratch.translation_cursor = 0;
    scratch.root = root;
    scratch.tables[0] = root;
    scratch.levels[0] = 4;
    scratch.count = 1;
    scratch.table_index.fill(0);
    let shift = 64 - scratch.table_index.len().trailing_zeros();
    let root_slot = ((root >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> shift) as usize;
    scratch.table_index[root_slot] = 1;
    scratch.upper_count = 0;
    let mut cursor = 0;
    let mut bytes = [0u8; 512];
    while cursor < ctx.state().svm_guard.count {
        let level = ctx.state().svm_guard.levels[cursor];
        let table = ctx.state().svm_guard.tables[cursor];
        ctx.state_mut().svm_guard.children[cursor].fill(0);
        let edge_start = ctx.state().svm_guard.upper_count;
        let mut edge_len = 0usize;
        if level > 1 {
            // A guarded table cannot change behind the scanner. Its direct
            // child links remain valid even when CR3 points to another root;
            // newly reached or write-released tables still get read below.
            let cached = ctx.state().svm_guard.gate_tables[..ctx.state().svm_guard.gate_count]
                .iter()
                .enumerate()
                .find(|(index, &page)| {
                    page == table
                        && ctx.state().svm_guard.gate_guards[*index].valid
                        && !ctx.state().svm_guard.gate_links_untrusted
                        && ctx.state().svm_guard.gate_levels[*index] == level
                })
                .map(|(index, _)| index);
            if let Some(index) = cached {
                let old_len = ctx.state().svm_guard.gate_upper_lengths[index];
                let old_start = usize::from(ctx.state().svm_guard.gate_upper_starts[index]);
                if old_len != u16::MAX
                    && old_start + usize::from(old_len) <= ctx.state().svm_guard.gate_upper_count
                    && edge_start + usize::from(old_len) <= SVM_ALIAS_EDGE_CAPACITY
                {
                    for old in old_start..old_start + usize::from(old_len) {
                        let edge = ctx.state().svm_guard.gate_upper_edges[old];
                        let write = ctx.state().svm_guard.upper_count;
                        ctx.state_mut().svm_guard.upper_edges[write] = edge;
                        ctx.state_mut().svm_guard.upper_count += 1;
                    }
                    edge_len = usize::from(old_len);
                } else {
                    edge_len = usize::from(u16::MAX);
                }
                let child_words = ctx.state().svm_guard.gate_children[index];
                for (word_index, word) in child_words.into_iter().enumerate() {
                    let mut links = word;
                    while links != 0 {
                        let child_index = word_index * 64 + links.trailing_zeros() as usize;
                        let child = ctx.state().svm_guard.gate_tables[child_index];
                        add_translation_child(ctx, cursor, level, child, code_pages)?;
                        links &= links - 1;
                    }
                }
                ctx.state_mut().svm_guard.upper_starts[cursor] = edge_start as u16;
                ctx.state_mut().svm_guard.upper_lengths[cursor] = edge_len as u16;
                cursor += 1;
                continue;
            }
            for offset in (0..4096).step_by(512) {
                ctx.read_guest_memory(GuestPhysAddr::new(table + offset), &mut bytes)
                    .ok()?;
                for (part, entry) in bytes.chunks_exact(8).enumerate() {
                    let entry = u64::from_le_bytes(entry.try_into().ok()?);
                    if entry & 1 == 0 {
                        continue;
                    }
                    if edge_len != usize::from(u16::MAX) {
                        if ctx.state().svm_guard.upper_count < SVM_ALIAS_EDGE_CAPACITY {
                            let slot = (offset / 8 + part as u64) as u16;
                            let write = ctx.state().svm_guard.upper_count;
                            ctx.state_mut().svm_guard.upper_edges[write] = SvmAliasEdge { slot, entry };
                            ctx.state_mut().svm_guard.upper_count += 1;
                            edge_len += 1;
                        } else {
                            edge_len = usize::from(u16::MAX);
                        }
                    }
                    if entry & (1 << 7) != 0 {
                        if level == 4 {
                            return None;
                        }
                        continue;
                    }
                    let child = entry & 0x000f_ffff_ffff_f000;
                    add_translation_child(ctx, cursor, level, child, code_pages)?;
                }
            }
        }
        ctx.state_mut().svm_guard.upper_starts[cursor] = edge_start as u16;
        ctx.state_mut().svm_guard.upper_lengths[cursor] = edge_len as u16;
        cursor += 1;
    }
    ctx.state_mut().svm_guard.valid = true;
    Some(())
}

fn restore_global_table_guards<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
) {
    let count = ctx.state().svm_guard.gate_count;
    for index in 0..count {
        let saved = ctx.state().svm_guard.gate_guards[index];
        if saved.valid && retained_guard_for(&ctx.state().svm_guard, saved.guest).is_none() {
            saved.write_guard.restore(&mut ctx.state_mut().ept, allocator);
        }
        ctx.state_mut().svm_guard.gate_guards[index].valid = false;
    }
    let retained_count = ctx.state().svm_guard.retained_guard_count;
    for index in 0..retained_count {
        let saved = ctx.state().svm_guard.retained_guards[index];
        if saved.valid {
            saved.write_guard.restore(&mut ctx.state_mut().ept, allocator);
            ctx.state_mut().svm_guard.retained_guards[index].valid = false;
        }
    }
    let guard = &mut ctx.state_mut().svm_guard;
    guard.retained_guard_count = 0;
    guard.gate_count = 0;
    guard.gate_ready = false;
    guard.gate_dirty = false;
    guard.gate_links_untrusted = true;
    clear_leaf_snapshots(guard);
    invalidate_alias_proofs(guard);
}

fn retained_guard_for(guard: &super::super::vm_state::SvmGuardScratch, page: u64) -> Option<usize> {
    guard.retained_guards[..guard.retained_guard_count]
        .iter()
        .position(|saved| saved.valid && saved.guest == page)
}

fn forget_active_global_tree<C: VmContext>(ctx: &mut C) {
    let guard = &mut ctx.state_mut().svm_guard;
    guard.gate_count = 0;
    guard.gate_ready = false;
    guard.gate_dirty = false;
    guard.gate_links_untrusted = true;
    clear_leaf_snapshots(guard);
}

fn can_refresh_leaf_only(
    guard: &super::super::vm_state::SvmGuardScratch,
    root: u64,
    mapping_generation: u64,
) -> bool {
    guard.gate_ready
        && guard.gate_dirty
        && !guard.gate_links_untrusted
        && guard.gate_root == root
        && guard.gate_mapping_generation == mapping_generation
        && guard.gate_count != 0
        && guard.gate_guards[..guard.gate_count].iter().any(|saved| !saved.valid)
        && (0..guard.gate_count).all(|index| {
            guard.gate_guards[index].valid || guard.gate_levels[index] == 1
        })
}

fn invalidate_alias_proofs(guard: &mut super::super::vm_state::SvmGuardScratch) {
    INVALIDATE_TREE.fetch_add(1, Ordering::Relaxed);
    guard.alias_proof.valid = false;
    for proof in &mut guard.alias_proofs {
        proof.valid = false;
    }
}

/// A changed leaf PTE can create an executable alias only for the physical
/// page it names after the write. Removed aliases leave extra breakpoints.
fn leaf_page_slot(physical: u64) -> usize {
    ((physical >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15) as usize)
        & (SVM_LEAF_PAGE_INDEX_CAPACITY - 1)
}

fn leaf_page_insert(set: &mut [u64; SVM_LEAF_PAGE_INDEX_CAPACITY], physical: u64) -> Option<()> {
    let key = physical.checked_add(1)?;
    let mut slot = leaf_page_slot(physical);
    for _ in 0..set.len() {
        if set[slot] == 0 || set[slot] == key {
            set[slot] = key;
            return Some(());
        }
        slot = (slot + 1) & (set.len() - 1);
    }
    None
}

fn leaf_page_contains(set: &[u64; SVM_LEAF_PAGE_INDEX_CAPACITY], physical: u64) -> bool {
    let Some(key) = physical.checked_add(1) else { return false; };
    let mut slot = leaf_page_slot(physical);
    for _ in 0..set.len() {
        if set[slot] == key {
            return true;
        }
        if set[slot] == 0 {
            return false;
        }
        slot = (slot + 1) & (set.len() - 1);
    }
    false
}

fn snapshot_released_leaf<C: VmContext>(ctx: &mut C, page: u64) -> Option<()> {
    let guard = &ctx.state().svm_guard;
    if guard.leaf_snapshot_overflow {
        return None;
    }
    if guard.leaf_snapshots.iter().any(|snapshot| snapshot.valid && snapshot.page == page) {
        return Some(());
    }
    let slot = guard.leaf_snapshots.iter().position(|snapshot| !snapshot.valid)?;
    let mut bytes = [0u8; 512];
    for offset in (0..4096).step_by(bytes.len()) {
        ctx.read_guest_memory(GuestPhysAddr::new(page + offset as u64), &mut bytes).ok()?;
        ctx.state_mut().svm_guard.leaf_snapshots[slot].bytes[offset..offset + bytes.len()]
            .copy_from_slice(&bytes);
    }
    let snapshot = &mut ctx.state_mut().svm_guard.leaf_snapshots[slot];
    snapshot.page = page;
    snapshot.valid = true;
    Some(())
}

fn clear_leaf_snapshots(guard: &mut super::super::vm_state::SvmGuardScratch) {
    for snapshot in &mut guard.leaf_snapshots {
        snapshot.valid = false;
    }
    guard.leaf_snapshot_overflow = false;
}

fn invalidate_alias_proofs_for_released_leaves<C: VmContext>(ctx: &mut C) -> Option<()> {
    if ctx.state().svm_guard.leaf_snapshot_overflow {
        LEAF_SNAPSHOT_MISS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut bytes = [0u8; 512];
    for index in 0..ctx.state().svm_guard.gate_count {
        let guard = &ctx.state().svm_guard;
        if guard.gate_guards[index].valid {
            continue;
        }
        if guard.gate_levels[index] != 1 {
            return None;
        }
        let table = guard.gate_tables[index];
        let Some(snapshot) = guard.leaf_snapshots.iter()
            .position(|snapshot| snapshot.valid && snapshot.page == table) else {
            LEAF_SNAPSHOT_MISS.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        ctx.state_mut().svm_guard.leaf_pages.fill(0);
        for offset in (0..4096).step_by(bytes.len()) {
            ctx.read_guest_memory(GuestPhysAddr::new(table + offset as u64), &mut bytes).ok()?;
            for (part, entry) in bytes.chunks_exact(8).enumerate() {
                let entry = u64::from_le_bytes(entry.try_into().ok()?);
                let old = &ctx.state().svm_guard.leaf_snapshots[snapshot].bytes
                    [offset + part * 8..offset + (part + 1) * 8];
                let old = u64::from_le_bytes(old.try_into().ok()?);
                if entry == old || entry & 1 == 0 || entry & (1 << 63) != 0 {
                    continue;
                }
                let physical = entry & 0x000f_ffff_ffff_f000;
                if old & 1 != 0 && old & (1 << 63) == 0
                    && old & 0x000f_ffff_ffff_f000 == physical {
                    continue;
                }
                LEAF_CHANGED_ENTRIES.fetch_add(1, Ordering::Relaxed);
                leaf_page_insert(&mut ctx.state_mut().svm_guard.leaf_pages, physical)?;
            }
        }
        let guard = &mut *ctx.state_mut().svm_guard;
        let (set, proofs) = (&guard.leaf_pages, &mut guard.alias_proofs);
        for proof in proofs {
            if proof.valid && proof.pages[..proof.page_count]
                .iter().any(|&page| leaf_page_contains(set, page)) {
                proof.valid = false;
                LEAF_INVALIDATED_PROOFS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    clear_leaf_snapshots(&mut ctx.state_mut().svm_guard);
    Some(())
}

/// Keep an alias proof across a complete tree rebuild only if every released
/// table either retained its executable child links or cannot map one of the
/// proof's physical pages. A newly executable upper link can expose any
/// descendant, so it invalidates every proof.
type AliasRetention = [u64; SVM_ALIAS_PROOF_CAPACITY / 64];

fn alias_proof_retained(retained: &AliasRetention, index: usize) -> bool {
    retained[index / 64] & (1u64 << (index % 64)) != 0
}

fn alias_proofs_safe_across_rebuild<C: VmContext>(ctx: &C, root: u64) -> Option<AliasRetention> {
    let guard = &ctx.state().svm_guard;
    if !guard.gate_ready || !guard.gate_dirty || guard.gate_links_untrusted
        || guard.gate_root != root {
        FULL_REJECT_GUARD.fetch_add(1, Ordering::Relaxed);
        if !guard.gate_ready || !guard.gate_dirty {
            FULL_REJECT_NOT_READY.fetch_add(1, Ordering::Relaxed);
        } else if guard.gate_links_untrusted {
            FULL_REJECT_UNTRUSTED.fetch_add(1, Ordering::Relaxed);
        } else {
            FULL_REJECT_ROOT.fetch_add(1, Ordering::Relaxed);
        }
        return None;
    }
    if guard.gate_mapping_generation != ctx.state().ept.mapping_generation() {
        FULL_REJECT_MAPPING.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut retained = [0u64; SVM_ALIAS_PROOF_CAPACITY / 64];
    for (index, proof) in guard.alias_proofs.iter().enumerate() {
        if proof.valid && proof.root == root {
            retained[index / 64] |= 1u64 << (index % 64);
        }
    }
    if retained.iter().all(|&word| word == 0) {
        FULL_REJECT_NO_PROOF.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut released = false;
    let mut bytes = [0u8; 512];
    for table_index in 0..guard.gate_count {
        if guard.gate_guards[table_index].valid {
            continue;
        }
        released = true;
        let level = guard.gate_levels[table_index];
        let table = guard.gate_tables[table_index];
        let old_start = usize::from(guard.gate_upper_starts[table_index]);
        let old_len = usize::from(guard.gate_upper_lengths[table_index]);
        if level > 1 && (old_len == usize::from(u16::MAX)
            || old_start + old_len > guard.gate_upper_count)
        {
            FULL_REJECT_UPPER.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        for offset in (0..4096).step_by(bytes.len()) {
            ctx.read_guest_memory(GuestPhysAddr::new(table + offset as u64), &mut bytes).ok()?;
            for (part, entry) in bytes.chunks_exact(8).enumerate() {
                let entry = u64::from_le_bytes(entry.try_into().ok()?);
                if entry & 1 == 0 || entry & (1 << 63) != 0 {
                    continue;
                }
                if level == 1 {
                    let physical = entry & 0x000f_ffff_ffff_f000;
                    for (index, proof) in guard.alias_proofs.iter().enumerate() {
                        if alias_proof_retained(&retained, index)
                            && proof.pages[..proof.page_count].contains(&physical) {
                            retained[index / 64] &= !(1u64 << (index % 64));
                        }
                    }
                } else {
                    let slot = (offset / 8 + part) as u16;
                    let unchanged = guard.gate_upper_edges[old_start..old_start + old_len]
                        .iter()
                        .any(|old| old.slot == slot
                            && old.entry & 1 != 0
                            && old.entry & (1 << 63) == 0
                            && (old.entry ^ entry) & ((1 << 7) | 0x000f_ffff_ffff_f000) == 0);
                    if !unchanged {
                        FULL_REJECT_UPPER.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                }
            }
        }
    }
    if !released {
        FULL_REJECT_NO_RELEASE.fetch_add(1, Ordering::Relaxed);
    }
    released.then_some(retained)
}

pub(crate) fn refresh_global_tree<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    can_guard_page_tables: bool,
) {
    if !can_guard_page_tables {
        ctx.state_mut().svm_guard.gate_disabled_no_rogpt = true;
        if !ctx.state().svm_guard.gate_ready
            && !ctx.state().svm_guard.gate_dirty
            && ctx.state().svm_guard.gate_count == 0
        {
            return;
        }
        // Without ROGPT, protecting page tables turns ordinary hardware page
        // walks into nested-page faults. Keep the global gate off; validated
        // bounded batches still protect their own code and store ranges.
        restore_global_table_guards(ctx, allocator);
        return;
    }
    ctx.state_mut().svm_guard.gate_disabled_no_rogpt = false;
    if ctx.state().svm_guard.retained_guard_count != 0
        && ctx.state().svm_guard.retained_mapping_generation
            != ctx.state().ept.mapping_generation()
    {
        restore_global_table_guards(ctx, allocator);
    }
    if ctx.state().svm_gate_pending_write && ctx.state().svm_gate_scalar_page.is_some() {
        REFRESH_PENDING.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if ctx.state().svm_gate_pending_write {
        ctx.state_mut().svm_gate_pending_write = false;
        ctx.state_mut().svm_gate_replay_start = None;
        ctx.state_mut().svm_gate_replay_rip = None;
    }
    let Some(root) = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestCr3)
        .ok()
        .map(|root| root & 0x000f_ffff_ffff_f000)
    else {
        return;
    };
    if ctx.state().svm_guard.gate_ready
        && !ctx.state().svm_guard.gate_dirty
        && ctx.state().svm_guard.gate_root == root
    {
        return;
    }
    let paged = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestCr0)
        .ok()
        .is_some_and(|cr0| cr0 & (1 << 31) != 0);
    if !paged {
        restore_global_table_guards(ctx, allocator);
        ctx.state_mut().svm_guard.gate_root = root;
        ctx.state_mut().svm_guard.gate_ready = true;
        return;
    }
    // A guest store to a leaf PTE cannot change which page-table frames are
    // reachable. Once the store has retired, rearm only its released leaf
    // guard. Upper-level writes, host writes, and CR3 changes still rebuild
    // the complete guarded tree below.
    let leaf_only = can_refresh_leaf_only(
        &ctx.state().svm_guard,
        root,
        ctx.state().ept.mapping_generation(),
    );
    if leaf_only {
        REFRESH_LEAF.fetch_add(1, Ordering::Relaxed);
        if invalidate_alias_proofs_for_released_leaves(ctx).is_none() {
            invalidate_alias_proofs(&mut ctx.state_mut().svm_guard);
        }
        let count = ctx.state().svm_guard.gate_count;
        let mut rearmed = true;
        for index in 0..count {
            if ctx.state().svm_guard.gate_guards[index].valid {
                continue;
            }
            let page = ctx.state().svm_guard.gate_tables[index];
            let gpa = GuestPhysAddr::new(page);
            let _ = ctx.state_mut().ept.invalidate_npt_code_4k(allocator, gpa);
            match ctx.state_mut().ept.restrict_write_4k(allocator, gpa) {
                Ok(Some(write_guard)) => {
                    let saved = super::super::vm_state::SvmGuardSaved {
                        guest: page,
                        write_guard,
                        valid: true,
                    };
                    let guard = &mut ctx.state_mut().svm_guard;
                    let slot = guard.retained_guards[..guard.retained_guard_count]
                        .iter().position(|entry| !entry.valid).unwrap_or(guard.retained_guard_count);
                    if slot == guard.retained_guards.len() {
                        saved.write_guard.restore(&mut ctx.state_mut().ept, allocator);
                        rearmed = false;
                        break;
                    }
                    guard.retained_guards[slot] = saved;
                    guard.retained_guard_count = guard.retained_guard_count.max(slot + 1);
                    guard.gate_guards[index] = saved;
                }
                _ => {
                    rearmed = false;
                    break;
                }
            }
        }
        if rearmed {
            let guard = &mut ctx.state_mut().svm_guard;
            guard.valid = false;
            guard.translation_count = 0;
            guard.translation_cursor = 0;
            guard.tree_generation = guard.tree_generation.wrapping_add(1);
            guard.alias_proof.valid = false;
            guard.gate_dirty = false;
            REFRESH_REARM.fetch_add(1, Ordering::Relaxed);
            return;
        }
        invalidate_alias_proofs(&mut ctx.state_mut().svm_guard);
    }
    ctx.state_mut().svm_guard.valid = false;
    REFRESH_FULL.fetch_add(1, Ordering::Relaxed);
    if !ctx.state().svm_guard.gate_ready {
        FULL_COLD_GATE.fetch_add(1, Ordering::Relaxed);
    } else if ctx.state().svm_guard.gate_dirty {
        FULL_DIRTY_GATE.fetch_add(1, Ordering::Relaxed);
    } else if ctx.state().svm_guard.gate_root != root {
        FULL_SWITCH_ROOT.fetch_add(1, Ordering::Relaxed);
    }
    let mut retained = alias_proofs_safe_across_rebuild(ctx, root);
    if retained.is_some() {
        FULL_PRESERVE.fetch_add(1, Ordering::Relaxed);
    } else {
        FULL_REJECT.fetch_add(1, Ordering::Relaxed);
        // A released table in the old root is no longer write-protected.
        // Switching CR3 before its store can be checked must not preserve
        // proofs for that old root through the retained-guard registry.
        if ctx.state().svm_guard.gate_dirty {
            invalidate_alias_proofs(&mut ctx.state_mut().svm_guard);
        }
    }
    let scanned = collect_translation_tree_pages(ctx, &[]).is_some();
    TREE_SCAN_MAX_COUNT.fetch_max(ctx.state().svm_guard.count as u64, Ordering::Relaxed);
    if !scanned {
        TREE_SCAN_FAIL.fetch_add(1, Ordering::Relaxed);
    }
    // Keep guards from inactive roots live. A write to any of those tables
    // traps and invalidates their alias proofs before the next guest entry.
    if scanned && ctx.state().svm_guard.retained_guard_count
        .saturating_add(ctx.state().svm_guard.count) > SVM_RETAINED_GUARD_CAPACITY
    {
        restore_global_table_guards(ctx, allocator);
        retained = None;
    } else {
        forget_active_global_tree(ctx);
    }
    ctx.state_mut().svm_guard.gate_root = root;
    if !scanned {
        return;
    }
    let count = ctx.state().svm_guard.count;
    for index in 0..count {
        let page = ctx.state().svm_guard.tables[index];
        let gpa = GuestPhysAddr::new(page);
        ctx.state_mut().svm_guard.gate_tables[index] = page;
        ctx.state_mut().svm_guard.gate_levels[index] = ctx.state().svm_guard.levels[index];
        ctx.state_mut().svm_guard.gate_children[index] = ctx.state().svm_guard.children[index];
        ctx.state_mut().svm_guard.gate_upper_starts[index] = ctx.state().svm_guard.upper_starts[index];
        ctx.state_mut().svm_guard.gate_upper_lengths[index] = ctx.state().svm_guard.upper_lengths[index];
        ctx.state_mut().svm_guard.gate_guards[index].valid = false;
        let _ = ctx.state_mut().ept.invalidate_npt_code_4k(allocator, gpa);
        if let Some(retained_index) = retained_guard_for(&ctx.state().svm_guard, page) {
            let saved = ctx.state().svm_guard.retained_guards[retained_index];
            if saved.write_guard.is_active(&ctx.state().ept, allocator, gpa) {
                ctx.state_mut().svm_guard.gate_guards[index] = saved;
                continue;
            }
            ctx.state_mut().svm_guard.retained_guards[retained_index].valid = false;
            invalidate_alias_proofs(&mut ctx.state_mut().svm_guard);
            retained = None;
        }
        match ctx.state_mut().ept.restrict_write_4k(allocator, gpa) {
            Ok(Some(write_guard)) => {
                let saved = super::super::vm_state::SvmGuardSaved {
                    guest: page, write_guard, valid: true,
                };
                let guard = &mut ctx.state_mut().svm_guard;
                let slot = guard.retained_guards[..guard.retained_guard_count]
                    .iter().position(|entry| !entry.valid).unwrap_or(guard.retained_guard_count);
                if slot == guard.retained_guards.len() {
                    saved.write_guard.restore(&mut ctx.state_mut().ept, allocator);
                    restore_global_table_guards(ctx, allocator);
                    return;
                }
                guard.retained_guards[slot] = saved;
                guard.retained_guard_count = guard.retained_guard_count.max(slot + 1);
                guard.gate_guards[index] = saved;
            }
            Ok(None) => {
                TREE_RESTRICT_NONE.fetch_add(1, Ordering::Relaxed);
                restore_global_table_guards(ctx, allocator);
                return;
            }
            Err(_) => {
                TREE_RESTRICT_ERR.fetch_add(1, Ordering::Relaxed);
                restore_global_table_guards(ctx, allocator);
                return;
            }
        }
    }
    let mapping_generation = ctx.state().ept.mapping_generation();
    let guard = &mut ctx.state_mut().svm_guard;
    guard.gate_upper_count = guard.upper_count;
    for index in 0..guard.upper_count {
        guard.gate_upper_edges[index] = guard.upper_edges[index];
    }
    guard.gate_count = count;
    guard.gate_root = root;
    guard.gate_mapping_generation = mapping_generation;
    guard.retained_mapping_generation = mapping_generation;
    guard.gate_dirty = false;
    guard.gate_links_untrusted = false;
    guard.gate_ready = true;
    TREE_READY_SUCCESS.fetch_add(1, Ordering::Relaxed);
    if let Some(retained) = retained {
        for (index, proof) in guard.alias_proofs.iter_mut().enumerate() {
            proof.valid = alias_proof_retained(&retained, index);
        }
    }
}

/// Only a decoded scalar store has a verifiable next RIP for rearming the
/// table guard after one retired instruction. REP and unknown encodings keep
/// the older conservative gate until the scalar page is released.
pub(crate) fn scalar_table_store_next_rip<C: VmContext>(
    ctx: &C,
    window: &super::svm::InstructionWindow,
) -> Option<u64> {
    let long = ctx.state().vmcs.read32(VmcsField32::GuestCsAccessRights).ok()? & (1 << 13) != 0;
    if !long {
        return None;
    }
    let (length, _, writes) = safe_len(&window.bytes, true, false)?;
    writes.then(|| window.linear.checked_add(length as u64)).flatten()
}

pub(crate) fn release_global_table_write<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    page: u64,
) -> bool {
    let Some(index) = retained_guard_for(&ctx.state().svm_guard, page) else {
        return false;
    };
    let saved = ctx.state().svm_guard.retained_guards[index];
    saved.write_guard.restore(&mut ctx.state_mut().ept, allocator);
    ctx.state_mut().svm_guard.retained_guards[index].valid = false;
    let count = ctx.state().svm_guard.gate_count;
    let active_leaf = (0..count).any(|current| {
        ctx.state().svm_guard.gate_tables[current] == page
            && ctx.state().svm_guard.gate_levels[current] == 1
    });
    if active_leaf && snapshot_released_leaf(ctx, page).is_none() {
        ctx.state_mut().svm_guard.leaf_snapshot_overflow = true;
        LEAF_SNAPSHOT_MISS.fetch_add(1, Ordering::Relaxed);
        invalidate_alias_proofs(&mut ctx.state_mut().svm_guard);
    }
    for current in 0..count {
        if ctx.state().svm_guard.gate_tables[current] == page {
            ctx.state_mut().svm_guard.gate_guards[current].valid = false;
        }
    }
    ctx.state_mut().svm_guard.gate_dirty = true;
    // The dirty gate prevents proof use until the store retires. A leaf PTE
    // write can then invalidate only proofs whose code pages are executable
    // through that leaf; upper or inactive-root writes still drop all proofs.
    if !active_leaf {
        invalidate_alias_proofs(&mut ctx.state_mut().svm_guard);
    }
    true
}

/// The table proof covers every reachable frame, so A/D updates cannot change
/// these mappings. Only call after collect_translation_tree validates CR3.
pub(super) fn cached_code_translation<C: VmContext>(ctx: &mut C, linear: u64) -> Option<u64> {
    let gate_guarded = ctx.state().svm_guard.gate_ready
        && !ctx.state().svm_guard.gate_dirty
        && ctx.state().svm_guard.gate_root
            == (ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestCr3).ok()?
                & 0x000f_ffff_ffff_f000);
    if ctx.state().svm_guard.valid || gate_guarded {
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
    if cache.valid || gate_guarded {
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
    if batch.is_some_and(|batch| batch.global_execution) {
        ctx.state_mut().svm_guard.valid = false;
        ctx.state_mut().svm_guard.code_count = 0;
        return;
    }
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
                long && (window.bytes.starts_with(&[0x0f, 0x31])
                    || window.bytes.starts_with(&[0x0f, 0x01, 0xf9])
                    || window.bytes.starts_with(&[0x0f, 0xa2])
                    || scalar_iret_preserves_guard(ctx, window)
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
            // Retain only pages whose bytes are covered by this entry's
            // write guards, including cached code guarded only as data.
            let cache = &mut ctx.state_mut().svm_guard;
            let mut index = 0;
            while index < cache.code_count {
                if !batch.pages[..batch.code_page_count].contains(&cache.code[index].page)
                    && !cache
                        .saved
                        .iter()
                        .any(|saved| saved.valid && saved.guest == cache.code[index].page)
                {
                    cache.code_count -= 1;
                    cache.code[index] = cache.code[cache.code_count];
                } else {
                    index += 1;
                }
            }
        }
        None => {
            if let Some(window) = window {
                if (safe_len(&window.bytes, true, false).is_some_and(|(_, _, writes)| writes)
                    || window.bytes[0] == 0xe8)
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
    } else if window.bytes[0] == 0xe8 {
        // A direct near CALL pushes the return address. Its displacement does
        // not affect the write destination.
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

fn hazard_memo_lookup<C: VmContext>(ctx: &C, physical: u64) -> (usize, Option<usize>) {
    let cache = &ctx.state().svm_guard;
    let key = ((physical >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15) as usize)
        & (super::super::vm_state::SVM_HAZARD_LOOKUP_CAPACITY - 1);
    let hinted = cache.hazard_lookup[key].checked_sub(1).map(usize::from);
    let valid = |index: usize| {
        let memo = &cache.hazard_memos[index];
        (memo.valid || memo.guarded) && memo.proof.page == physical
    };
    let found = hinted.filter(|&index| valid(index))
        .or_else(|| cache.hazard_memos.iter().position(|memo| {
            (memo.valid || memo.guarded) && memo.proof.page == physical
        }));
    (key, found)
}

fn page_hazards_memo<C: VmContext>(ctx: &mut C, physical: u64) -> Option<PageHazards> {
    // Unguarded pages always compare exact bytes before reusing a scan.
    // Recurrent guarded pages can skip that comparison while the NPT mapping
    // stays fixed: guest writes fault, and host writes invalidate the memo.
    let (key, memo) = hazard_memo_lookup(ctx, physical);
    if let Some(index) = memo {
        ctx.state_mut().svm_guard.hazard_lookup[key] = (index + 1) as u16;
    }
    let unchanged = memo.is_some_and(|index| {
        let memo = &ctx.state().svm_guard.hazard_memos[index];
        let mapping_unchanged = memo.guarded_mapping_generation
            == ctx.state().ept.mapping_generation();
        memo.valid && ((memo.guarded && mapping_unchanged) || ctx.guest_memory_matches(
            GuestPhysAddr::new(physical), &memo.bytes,
        ) == Ok(true))
    });
    if unchanged {
        let memo = &mut ctx.state_mut().svm_guard.hazard_memos[memo.unwrap()];
        memo.hits = memo.hits.saturating_add(1);
        return (!memo.rejected).then_some(PageHazards {
            boundary: memo.proof.boundary,
            edge: memo.proof.edge,
            offsets: memo.proof.offsets,
            count: memo.proof.count,
        });
    }
    let index = if let Some(index) = memo {
        index
    } else {
        let cache = &ctx.state().svm_guard;
        let Some(index) = (0..cache.hazard_memos.len())
            .map(|step| (cache.hazard_memo_cursor + step) % cache.hazard_memos.len())
            .find(|&index| !cache.hazard_memos[index].guarded)
        else {
            return page_hazards(ctx, physical, None).flatten();
        };
        index
    };
    // A scan already reads the entire guest page. Save those bytes as each
    // chunk arrives instead of reading all 4 KB again to populate the memo.
    let hazards = page_hazards(ctx, physical, Some(index))?;
    let cache = &mut ctx.state_mut().svm_guard;
    cache.code_epoch = cache.code_epoch.wrapping_add(1);
    // Region proofs are keyed by page and revision, not by memo slot. Use a
    // scratch-wide revision so eviction cannot revive an older proof.
    cache.hazard_memos[index].revision = cache.code_epoch;
    cache.hazard_memos[index].valid = false;
    if memo.is_none() {
        cache.hazard_memos[index].hits = 0;
    }
    let cache = &mut ctx.state_mut().svm_guard;
    cache.hazard_memos[index].rejected = hazards.is_none();
    cache.hazard_memos[index].proof = super::super::vm_state::SvmCodeProof {
        page: physical,
        boundary: hazards.map_or([0; 32], |hazards| hazards.boundary),
        edge: hazards.map_or(0, |hazards| hazards.edge),
        offsets: hazards.map_or([0; 4], |hazards| hazards.offsets),
        count: hazards.map_or(0, |hazards| hazards.count),
    };
    cache.hazard_memos[index].valid = true;
    cache.hazard_lookup[key] = (index + 1) as u16;
    if memo.is_none() {
        cache.hazard_memo_cursor = (index + 1) % cache.hazard_memos.len();
    }
    hazards
}

fn cached_page_hazards<C: VmContext>(ctx: &mut C, physical: u64) -> Option<PageHazards> {
    if !ctx.state().svm_guard.valid {
        ctx.state_mut().svm_guard.code_count = 0;
        let root = ctx
            .state()
            .vmcs
            .read_natural(VmcsFieldNatural::GuestCr3)
            .ok()
            .map(|root| root & 0x000f_ffff_ffff_f000);
        let gate_guarded = root.is_some_and(|root| {
            let guard = &ctx.state().svm_guard;
            guard.gate_ready && !guard.gate_dirty && guard.gate_root == root
        });
        let pending_leaf_refresh = root.is_some_and(|root| {
            can_refresh_leaf_only(&ctx.state().svm_guard, root, ctx.state().ept.mapping_generation())
        });
        let pending_full_refresh = root.is_some_and(|root| {
            let guard = &ctx.state().svm_guard;
            guard.gate_ready && guard.gate_dirty && guard.gate_root == root
        });
        if !gate_guarded && !pending_leaf_refresh && !pending_full_refresh {
            INVALIDATE_CODE.fetch_add(1, Ordering::Relaxed);
            invalidate_alias_proofs(&mut ctx.state_mut().svm_guard);
        }
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
    let hazards = page_hazards_memo(ctx, physical)?;
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

// The outer Option reports a failed guest-memory read; the inner Option
// reports a fully read page that cannot use a global hazard proof.
fn page_hazards<C: VmContext>(
    ctx: &mut C,
    physical: u64,
    memo_index: Option<usize>,
) -> Option<Option<PageHazards>> {
    // Amortize guest-memory translation without placing a full code page on
    // the kernel stack. The carry covers every legal instruction prefix chain.
    const CHUNK: usize = 512;
    let mut result = PageHazards {
        boundary: [0; 32],
        edge: 0,
        offsets: [0; 4],
        count: 0,
    };
    let mut too_many_hazards = false;
    let mut bytes = [0u8; CHUNK + 16];
    for offset in (0..4096).step_by(CHUNK) {
        ctx.read_guest_memory(
            GuestPhysAddr::new(physical + offset as u64),
            &mut bytes[16..],
        )
        .ok()?;
        if let Some(index) = memo_index {
            ctx.state_mut().svm_guard.hazard_memos[index].bytes[offset..offset + CHUNK]
                .copy_from_slice(&bytes[16..]);
        }
        if too_many_hazards {
            continue;
        }
        if offset == 0 {
            result.boundary[..16].copy_from_slice(&bytes[16..32]);
        }
        'scan: for index in 0..bytes.len() {
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
                    // Finish copying the remaining chunks into the memo so a
                    // rejected page can still be recognized by exact bytes.
                    too_many_hazards = true;
                    break 'scan;
                }
                result.offsets[result.count] = position;
                result.count += 1;
            }
        }
        bytes.copy_within(CHUNK..CHUNK + 16, 0);
    }
    if too_many_hazards {
        return Some(None);
    }
    // Interior hazards are covered at every prefix entry. Add breakpoint
    // entries for any partial instruction at the physical page boundary.
    result.boundary[16..].copy_from_slice(&bytes[..16]);
    // A prefix or partial opcode at the end of a page can become hazardous
    // when the next virtual page is fetched. Stop at every possible entry
    // into such a suffix, before the cross-page instruction can execute.
    for start in 2..16 {
        if !boundary_start_hazard(&result.boundary[16..], start) {
            continue;
        }
        let position = (4080 + start) as u16;
        if result.offsets[..result.count].contains(&position) {
            continue;
        }
        if result.count == result.offsets.len() {
            return Some(None);
        }
        result.offsets[result.count] = position;
        result.count += 1;
    }
    result.edge = summarize_edge(&result.boundary);
    if forbidden_page_bytes(&result.boundary[..16])
        || forbidden_page_bytes(&result.boundary[16..])
        || !hazard_boundary_safe(&result, &result)
    {
        return Some(None);
    }
    Some(Some(result))
}

/// A globally executable page must be safe at every entry byte, including
/// joins with any other executable page. Unknown pages remain NX until this
/// proof succeeds and their NPT leaves become write-protected.
pub(crate) fn globally_safe_code<C: VmContext>(ctx: &mut C, physical: u64) -> bool {
    let Some(hazards) = page_hazards_memo(ctx, physical) else {
        return false;
    };
    hazards.count == 0
        && hazards.edge & 15 == 15
        && hazards.boundary[31] != 0x0f
        && hazards.boundary[30..] != [0x0f, 0xc7]
}

/// Repeatedly fetched hazardous code can retain its exact-byte scan while an
/// NPT write guard prevents guest writes. Host writes invalidate the memo
/// directly; writes through NPT release the guard before retrying.
pub(crate) fn protect_recurrent_code<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    physical: u64,
) {
    let cache = &ctx.state().svm_guard;
    let (_, index) = hazard_memo_lookup(ctx, physical);
    let Some(index) = index.filter(|&index| cache.hazard_memos[index].valid) else {
        return;
    };
    if cache.hazard_memos[index].guarded {
        let guard = cache.hazard_memos[index].write_guard;
        if cache.hazard_memos[index].rejected || cache.hazard_memos[index].proof.count == 0 {
            guard.restore(&mut ctx.state_mut().ept, allocator);
            ctx.state_mut().svm_guard.hazard_memos[index].guarded = false;
            return;
        }
        let generation = ctx.state().ept.mapping_generation();
        if cache.hazard_memos[index].guarded_mapping_generation == generation {
            return;
        }
        if guard.is_active(&ctx.state().ept, allocator, GuestPhysAddr::new(physical)) {
            ctx.state_mut().svm_guard.hazard_memos[index].guarded_mapping_generation = generation;
            return;
        }
        ctx.state_mut().svm_guard.hazard_memos[index].guarded = false;
    }
    let memo = &ctx.state().svm_guard.hazard_memos[index];
    if memo.rejected || memo.proof.count == 0 || memo.hits < 16 {
        return;
    }
    if ctx.state().svm_guard.hazard_memos.iter().filter(|memo| memo.guarded).count() >= 8 {
        return;
    }
    if let Ok(Some(write_guard)) = ctx.state_mut().ept
        .restrict_write_4k(allocator, GuestPhysAddr::new(physical)) {
        let mapping_generation = ctx.state().ept.mapping_generation();
        let memo = &mut ctx.state_mut().svm_guard.hazard_memos[index];
        memo.write_guard = write_guard;
        memo.guarded = true;
        memo.guarded_mapping_generation = mapping_generation;
    }
}

pub(crate) fn release_recurrent_code_write<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    physical: u64,
) -> bool {
    let (_, index) = hazard_memo_lookup(ctx, physical);
    let Some(index) = index.filter(|&index| ctx.state().svm_guard.hazard_memos[index].guarded) else {
        return false;
    };
    let write_guard = ctx.state().svm_guard.hazard_memos[index].write_guard;
    write_guard.restore(&mut ctx.state_mut().ept, allocator);
    let cache = &mut ctx.state_mut().svm_guard;
    cache.hazard_memos[index].guarded = false;
    cache.hazard_memos[index].valid = false;
    cache.hazard_memos[index].hits = 0;
    cache.code_epoch = cache.code_epoch.wrapping_add(1);
    cache.code_count = 0;
    true
}

pub(crate) fn prepare_global<C: VmContext, A: CowAllocator<C::CowPage>>(
    ctx: &mut C,
    allocator: &A,
    can_count: bool,
    allow_unsafe: bool,
    window: &super::svm::InstructionWindow,
) -> Option<InstructionBatch> {
    let page = window.physical.as_u64() & !4095;
    let trusted = ctx.state().ept.npt_trusted_code_4k(allocator, GuestPhysAddr::new(page));
    if !can_count
        || !ctx.state().svm_guard.gate_ready
        || ctx.state().svm_guard.gate_dirty
        || (!trusted && !allow_unsafe)
        || ctx.state().svm_guard.gate_tables[..ctx.state().svm_guard.gate_count].contains(&page)
        || ctx.state().mtf_enabled
        || ctx.state().vmcs.read32(VmcsField32::GuestCsAccessRights).ok()? & (1 << 13) == 0
        || ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRflags).ok()? & ((1 << 8) | (1 << 16)) != 0
        || ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestDr7).ok()? & 0x20ff != 0
    {
        return None;
    }
    let budget = instruction_budget(ctx);
    if budget <= InstructionBatch::COUNTER_DEADLINE_MARGIN {
        return None;
    }
    let mut batch = InstructionBatch {
        start: ctx.state().vmcs.read_natural(VmcsFieldNatural::GuestRip).ok()?,
        offsets: [0; 65],
        count: 0,
        repeat: None,
        counted_loop: None,
        pages: [0; 68],
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
        uses_counter: true,
        counter_bounded: false,
        page_execution: true,
        global_execution: true,
        endpoint_intercepted: false,
        instruction_budget: budget,
    };
    batch.pages[0] = page;
    if !trusted {
        let hazard = cached_page_hazards(ctx, page)?;
        if hazard.count == 0 {
            // A page first fetched while the translation gate was dirty can
            // remain on the scalar path after the gate becomes usable. The
            // exact-byte proof is already available here: promote it just as
            // the execute-fault handler would, so later entries can run
            // across this page without another fetch fault or scan.
            // A write fault on previously trusted code retries at this same
            // RIP. Re-protecting the page before that store retires would
            // fault forever, so promote only a proven non-store entry.
            if safe_len(&window.bytes, true, false).is_some_and(|(_, _, writes)| !writes)
                && hazard.edge & 15 == 15
                && hazard.boundary[31] != 0x0f
                && hazard.boundary[30..] != [0x0f, 0xc7]
                && ctx.state_mut().ept
                    .trust_npt_code_4k(allocator, GuestPhysAddr::new(page))
                    .is_some()
            {
                return Some(batch);
            }
            return None;
        }
        if !boundary_starts_covered(&hazard)
            || hazard.offsets[..hazard.count].contains(&((window.physical.as_u64() & 4095) as u16))
        {
            return None;
        }
        collect_page_breakpoints(ctx, &mut batch, core::slice::from_ref(&hazard))?;
        if batch.page_breakpoint_count == 0 {
            return None;
        }
        // A counted global interval can cross a second hazardous page without
        // an execute fault when all of its executable aliases fit the same
        // four hardware breakpoints. Only try the most recently visited page:
        // scanning many candidates here would make ordinary guest setup slow.
        let current_linear = window.linear & !4095;
        let recent = ctx.state().svm_recent_pages;
        if let Some(previous_linear) = recent
            .into_iter()
            .find(|&linear| linear != u64::MAX && linear != current_linear)
        {
            if let Some(previous) = cached_code_translation(ctx, previous_linear) {
                if previous != page
                    && !ctx.state().svm_guard.gate_tables[..ctx.state().svm_guard.gate_count]
                        .contains(&previous)
                    && !ctx
                        .state()
                        .ept
                        .npt_trusted_code_4k(allocator, GuestPhysAddr::new(previous))
                {
                    if let Some(other) = cached_page_hazards(ctx, previous) {
                        if other.count != 0
                            && boundary_starts_covered(&other)
                            && hazard.count + other.count <= batch.page_breakpoints.len()
                            && hazard_boundary_safe(&hazard, &other)
                            && hazard_boundary_safe(&other, &hazard)
                        {
                            let single = batch;
                            batch.pages[1] = previous;
                            batch.code_page_count = 2;
                            batch.page_count = 2;
                            if collect_page_breakpoints(ctx, &mut batch, &[hazard, other]).is_none()
                            {
                                batch = single;
                            }
                        }
                    }
                }
            }
        }
    }
    Some(batch)
}

fn visit_alias_entry<C: VmContext>(
    ctx: &mut C,
    batch: &mut InstructionBatch,
    hazards: &[PageHazards],
    walk: super::super::vm_state::SvmAliasWalk,
    slot: u16,
    entry: u64,
    count: &mut usize,
) -> Option<()> {
    if entry & 1 == 0 || entry & (1 << 63) != 0 {
        return Some(());
    }
    let shift = 12 + 9 * (walk.level - 1);
    let base = walk.base | (u64::from(slot) << shift);
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
                if batch.page_breakpoints[..batch.page_breakpoint_count].contains(&address) {
                    continue;
                }
                if batch.page_breakpoint_count == 4 {
                    COLLECT_WALK_BREAKPOINT_LIMIT.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                batch.page_breakpoints[batch.page_breakpoint_count] = address;
                batch.page_breakpoint_count += 1;
            }
        }
    } else {
        if *count == ctx.state().svm_guard.aliases.len() {
            COLLECT_WALK_WORKSPACE_LIMIT.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        ctx.state_mut().svm_guard.aliases[*count] = super::super::vm_state::SvmAliasWalk {
            table: physical,
            base,
            level: walk.level - 1,
        };
        *count += 1;
    }
    Some(())
}

fn alias_proof_key(root: u64, batch: &InstructionBatch, hazards: &[PageHazards]) -> u64 {
    let mut hash = root.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for (index, hazard) in hazards.iter().enumerate().take(batch.code_page_count) {
        if hazard.count == 0 {
            continue;
        }
        let mut item = batch.pages[index].wrapping_mul(0xbf58_476d_1ce4_e5b9)
            ^ hazard.count as u64;
        for &offset in &hazard.offsets[..hazard.count] {
            item = item.rotate_left(11).wrapping_mul(0x94d0_49bb_1331_11eb)
                ^ offset as u64;
        }
        // Physical code pages are a set; selecting them in a different order
        // must address the same cache set. Full equality still guards hashes.
        hash ^= item.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(27);
    }
    hash ^ (hash >> 32)
}

fn collect_page_breakpoints<C: VmContext>(
    ctx: &mut C,
    batch: &mut InstructionBatch,
    hazards: &[PageHazards],
) -> Option<()> {
    COLLECT_CALLS.fetch_add(1, Ordering::Relaxed);
    if hazards.len() < batch.code_page_count {
        return None;
    }
    batch.page_breakpoint_count = 0;
    if hazards[..batch.code_page_count]
        .iter()
        .all(|h| h.count == 0)
    {
        return Some(());
    }
    // Only hazardous pages require alias breakpoints. Their physical set is
    // independent of the order and membership of ordinary selected code pages.
    let hazard_pages = hazards[..batch.code_page_count]
        .iter()
        .filter(|h| h.count != 0)
        .count();
    if hazard_pages > SVM_ALIAS_PROOF_PAGE_CAPACITY {
        return None;
    }
    let root = ctx
        .state()
        .vmcs
        .read_natural(VmcsFieldNatural::GuestCr3)
        .ok()?
        & 0x000f_ffff_ffff_f000;
    let guard = &ctx.state().svm_guard;
    let key_hash = alias_proof_key(root, batch, hazards);
    let set = ((key_hash as usize) & (SVM_ALIAS_PROOF_CAPACITY / SVM_ALIAS_PROOF_WAYS - 1))
        * SVM_ALIAS_PROOF_WAYS;
    let tree_guarded = !guard.gate_dirty && ((guard.valid && guard.root == root)
        || (guard.gate_ready && guard.gate_root == root));
    if !tree_guarded { COLLECT_UNGUARDED.fetch_add(1, Ordering::Relaxed); }
    if guard.gate_dirty { COLLECT_DIRTY.fetch_add(1, Ordering::Relaxed); }
    let matches = |proof: &super::super::vm_state::SvmAliasProof| {
        tree_guarded
            && proof.valid
            && proof.key_hash == key_hash
            && proof.root == root
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
    };
    let cache = &ctx.state().svm_guard;
    let last = cache.alias_last_hit;
    let cached = if last < cache.alias_proofs.len() && matches(&cache.alias_proofs[last]) {
        Some(last)
    } else {
        cache.alias_proofs[set..set + SVM_ALIAS_PROOF_WAYS]
            .iter()
            .enumerate()
            .find(|(way, proof)| set + *way != last && matches(proof))
            .map(|(way, _)| set + way)
    };
    if let Some(index) = cached {
        COLLECT_HITS.fetch_add(1, Ordering::Relaxed);
        let proof = &ctx.state().svm_guard.alias_proofs[index];
        batch.page_breakpoints = proof.breakpoints;
        batch.page_breakpoint_count = proof.breakpoint_count;
        ctx.state_mut().svm_guard.alias_last_hit = index;
        return Some(());
    }
    if ctx.state().svm_guard.alias_proofs[set..set + SVM_ALIAS_PROOF_WAYS]
        .iter().any(|proof| proof.valid) {
        COLLECT_VALID_PROOF_MISSES.fetch_add(1, Ordering::Relaxed);
    }
    COLLECT_WALKS.fetch_add(1, Ordering::Relaxed);
    // Enumerate executable virtual aliases. NPT alone protects physical pages;
    // a hardware breakpoint must cover every virtual entry to hazardous bytes.
    // A physical table may appear through several virtual paths. Keep each
    // path to enumerate every executable alias, with a bounded heap workspace.
    use super::super::vm_state::SvmAliasWalk;
    ctx.state_mut().svm_guard.aliases[0] = SvmAliasWalk {
        table: root,
        base: 0,
        level: 4,
    };
    let mut count = 1;
    let mut cursor = 0;
    let mut bytes = [0u8; 512];
    batch.page_breakpoint_count = 0;
    while cursor < count {
        let walk = ctx.state().svm_guard.aliases[cursor];
        let cached = if walk.level > 1 && !ctx.state().svm_guard.gate_links_untrusted {
            ctx.state().svm_guard.gate_tables[..ctx.state().svm_guard.gate_count]
                .iter()
                .enumerate()
                .find(|(index, &page)| {
                    page == walk.table
                        && ctx.state().svm_guard.gate_guards[*index].valid
                        && ctx.state().svm_guard.gate_levels[*index] == walk.level
                        && ctx.state().svm_guard.gate_upper_lengths[*index] != u16::MAX
                        && usize::from(ctx.state().svm_guard.gate_upper_starts[*index])
                            + usize::from(ctx.state().svm_guard.gate_upper_lengths[*index])
                            <= ctx.state().svm_guard.gate_upper_count
                })
                .map(|(index, _)| index)
        } else {
            None
        };
        if let Some(index) = cached {
            let start = usize::from(ctx.state().svm_guard.gate_upper_starts[index]);
            let end = start + usize::from(ctx.state().svm_guard.gate_upper_lengths[index]);
            for edge_index in start..end {
                let edge = ctx.state().svm_guard.gate_upper_edges[edge_index];
                visit_alias_entry(ctx, batch, hazards, walk, edge.slot, edge.entry, &mut count)?;
            }
            cursor += 1;
            continue;
        }
        for offset in (0..4096).step_by(512) {
            if ctx.read_guest_memory(GuestPhysAddr::new(walk.table + offset), &mut bytes).is_err() {
                COLLECT_WALK_READ_FAIL.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            for (part, entry) in bytes.chunks_exact(8).enumerate() {
                let entry = u64::from_le_bytes(entry.try_into().ok()?);
                // Most table slots are absent. Avoid entering the alias
                // walker for entries that cannot name executable code.
                if entry & 1 == 0 || entry & (1 << 63) != 0 {
                    continue;
                }
                // At the 4KB leaf level, only mappings of selected code
                // pages can contribute execution breakpoints. Large-page
                // entries at upper levels still go through the visitor.
                if walk.level == 1
                    && !batch.pages[..batch.code_page_count]
                        .contains(&(entry & 0x000f_ffff_ffff_f000))
                {
                    continue;
                }
                let slot = (offset / 8 + part as u64) as u16;
                visit_alias_entry(ctx, batch, hazards, walk, slot, entry, &mut count)?;
            }
        }
        cursor += 1;
    }
    let proof = &mut ctx.state_mut().svm_guard.alias_proof;
    proof.valid = true;
    proof.key_hash = key_hash;
    proof.root = root;
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
    let cache = &mut ctx.state_mut().svm_guard;
    let cursor = (set..set + SVM_ALIAS_PROOF_WAYS)
        .find(|&slot| !cache.alias_proofs[slot].valid)
        .unwrap_or(set + cache.alias_cursor % SVM_ALIAS_PROOF_WAYS);
    let proof = cache.alias_proof;
    cache.alias_proofs[cursor] = proof;
    cache.alias_last_hit = cursor;
    cache.alias_cursor = cache.alias_cursor.wrapping_add(1);
    COLLECT_WALK_SUCCESS.fetch_add(1, Ordering::Relaxed);
    Some(())
}

/// A whole-page byte scan may need more than four breakpoints across virtual
/// aliases. In that case follow only decoded control flow from this entry.
/// Unknown instructions become breakpoints before execution; indirect jumps
/// cannot silently switch to another executable alias of the same GPA.
fn outgoing_region_target<C: VmContext>(
    ctx: &C,
    physical: u64,
    address: u64,
    outgoing: &mut [u64; 16],
    count: &mut usize,
) -> Option<()> {
    if super::svm::physical(ctx, address)
        .ok()
        .is_some_and(|p| p.as_u64() & !4095 == physical)
    {
        return None;
    }
    if !outgoing[..*count].contains(&address) {
        *outgoing.get_mut(*count)? = address;
        *count += 1;
    }
    Some(())
}

fn enqueue_region_target<C: VmContext>(
    ctx: &C,
    physical: u64,
    virtual_page: u64,
    target: usize,
    queue: &mut [u16; 256],
    queued: &mut usize,
    outgoing: &mut [u64; 16],
    outgoing_count: &mut usize,
) -> Option<()> {
    if target < 4096 {
        *queue.get_mut(*queued)? = target as u16;
        *queued += 1;
    } else {
        let address = virtual_page.checked_add(target as u64)?;
        outgoing_region_target(ctx, physical, address, outgoing, outgoing_count)?;
    }
    Some(())
}

fn collect_reachable_breakpoints<C: VmContext>(
    ctx: &mut C,
    batch: &mut InstructionBatch,
    linear: u64,
) -> Option<()> {
    if batch.code_page_count != 1 {
        return None;
    }
    let physical = batch.pages[0];
    let memo = ctx
        .state()
        .svm_guard
        .hazard_memos
        .iter()
        .find(|memo| memo.valid && memo.proof.page == physical)?;
    let revision = memo.revision;
    let cache = &ctx.state().svm_guard;
    if let Some(proof) = cache.region_proofs.iter().find(|proof| {
        proof.valid
            && proof.revision == revision
            && proof.page == physical
            && proof.linear == linear
            && proof.outgoing[..proof.outgoing_count]
                .iter()
                .all(|&address| {
                    !super::svm::physical(ctx, address)
                        .ok()
                        .is_some_and(|p| p.as_u64() & !4095 == physical)
                })
    }) {
        batch.page_breakpoints = proof.breakpoints;
        batch.page_breakpoint_count = proof.count;
        return Some(());
    }
    let bytes = &memo.bytes;
    let virtual_page = linear & !4095;
    let mut seen = [0u8; 512];
    let mut queue = [0u16; 256];
    let mut queued = 1;
    let mut cursor = 0;
    let mut outgoing = [0u64; 16];
    let mut outgoing_count = 0;
    queue[0] = (linear & 4095) as u16;
    batch.page_breakpoint_count = 0;
    while cursor < queued {
        let offset = usize::from(queue[cursor]);
        cursor += 1;
        let bit = 1u8 << (offset & 7);
        if seen[offset >> 3] & bit != 0 {
            continue;
        }
        seen[offset >> 3] |= bit;
        let tail = &bytes[offset..];
        if let Some((length, displacement)) = relative_branch(tail, true, false) {
            let next = offset.checked_add(length)?;
            let target = (virtual_page as i128) + (next as i128) + (displacement as i128);
            if !(0..=u64::MAX as i128).contains(&target) {
                return None;
            }
            let target = target as u64;
            if target & !4095 == virtual_page {
                enqueue_region_target(
                    ctx,
                    physical,
                    virtual_page,
                    (target & 4095) as usize,
                    &mut queue,
                    &mut queued,
                    &mut outgoing,
                    &mut outgoing_count,
                )?;
            } else {
                outgoing_region_target(ctx, physical, target, &mut outgoing, &mut outgoing_count)?;
            }
            let prefix = usize::from(tail[0] == 0x2e);
            let opcode = *tail.get(prefix)?;
            if !matches!(opcode, 0xeb | 0xe9) {
                enqueue_region_target(
                    ctx,
                    physical,
                    virtual_page,
                    next,
                    &mut queue,
                    &mut queued,
                    &mut outgoing,
                    &mut outgoing_count,
                )?;
            }
            continue;
        }
        if let Some((length, _, _)) = safe_len(tail, true, false) {
            enqueue_region_target(
                ctx,
                physical,
                virtual_page,
                offset.checked_add(length)?,
                &mut queue,
                &mut queued,
                &mut outgoing,
                &mut outgoing_count,
            )?;
            continue;
        }
        let address = virtual_page.checked_add(offset as u64)?;
        if !batch.page_breakpoints[..batch.page_breakpoint_count].contains(&address) {
            if batch.page_breakpoint_count == batch.page_breakpoints.len() {
                return None;
            }
            batch.page_breakpoints[batch.page_breakpoint_count] = address;
            batch.page_breakpoint_count += 1;
        }
    }
    let cache = &mut ctx.state_mut().svm_guard;
    let index = cache.region_cursor;
    cache.region_proofs[index] = super::super::vm_state::SvmRegionProof {
        valid: true,
        revision,
        page: physical,
        linear,
        breakpoints: batch.page_breakpoints,
        count: batch.page_breakpoint_count,
        outgoing,
        outgoing_count,
    };
    cache.region_cursor = (index + 1) % cache.region_proofs.len();
    Some(())
}

fn edge_prefix(byte: u8) -> bool {
    matches!(
        byte,
        0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0x40..=0x4f | 0xf2 | 0xf3
    )
}

fn boundary_start_hazard(last: &[u8], start: usize) -> bool {
    let suffix = &last[start..];
    if suffix.len() > 14 {
        return false;
    }
    (suffix.iter().all(|&byte| edge_prefix(byte))
        && suffix.iter().any(|&byte| matches!(byte, 0xf2 | 0xf3)))
        || (suffix.ends_with(&[0x0f])
            && suffix[..suffix.len() - 1].iter().all(|&byte| edge_prefix(byte)))
        || (suffix.ends_with(&[0x0f, 0xc7])
            && suffix[..suffix.len() - 2].iter().all(|&byte| edge_prefix(byte)))
}

fn boundary_starts_covered(hazard: &PageHazards) -> bool {
    (2..16).all(|start| {
        !boundary_start_hazard(&hazard.boundary[16..], start)
            || hazard.offsets[..hazard.count].contains(&((4080 + start) as u16))
    })
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
    if boundary_starts_covered(left) {
        return true;
    }
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

// These flag-independent operations cannot observe the temporarily clamped
// loop counter or change the pointer used by the range proof. Memory reads
// are limited to [RDI], so the counter cannot redirect their address either.
fn counted_store_payload(bytes: &[u8], counter: u8) -> bool {
    let Some((p, rex, _)) = opcode_start(bytes, true) else {
        return false;
    };
    if bytes[..p].iter().any(|b| matches!(b, 0x64 | 0x65 | 0x67)) {
        return false;
    }
    let op = bytes[p];
    if op == 0x90 {
        return rex & 1 == 0 && p + 1 == bytes.len();
    }
    let Some(&modrm) = bytes.get(p + 1) else {
        return false;
    };
    let mode = modrm >> 6;
    let reg = ((modrm >> 3) & 7) | ((rex & 4) << 1);
    let rm = (modrm & 7) | ((rex & 1) << 3);
    let payload = |register| register != 7 && register != counter;
    match op {
        0x8b if mode == 0 => rm == 7 && payload(reg),
        0x8b | 0x31 | 0x33 if mode == 3 => payload(reg) && payload(rm),
        0x81 | 0x83 | 0xc1 if mode == 3 => {
            rex & 4 == 0 && modrm >> 3 & 7 == 0 && payload(rm)
        }
        _ => false,
    }
}

// A counted MOV-store loop can use entry-time range validation when RDI
// advances once per iteration and RCX/RDX decreases once before JNZ. Other
// writes or control transfers require the ordinary conservative planner.
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
        } else if instruction.len() == 4
            && instruction[..3] == [0x48, 0x83, 0xc7]
            && instruction[3] > 0
            && instruction[3] < 128
            && stride.is_none()
            && decrement.is_none()
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
        } else if !writes
            && stride.is_none()
            && decrement.is_none()
            && counted_store_payload(instruction, 1)
            && counted_store_payload(instruction, 2)
        {
            // The eventual loop counter may be RCX or RDX. Until DEC is
            // decoded, exclude both from payload operands.
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

fn page_plan_slot(linear: u64, root: u64) -> usize {
    let key = linear ^ root.rotate_left(17);
    ((key ^ key.rotate_right(23)) as usize) & 127
}

fn cached_page_plan<C: VmContext>(
    ctx: &C,
    can_loop: bool,
    can_guard_page_tables: bool,
    window: &super::svm::InstructionWindow,
) -> Option<InstructionBatch> {
    if !can_loop || !can_guard_page_tables || repeat_len(&window.bytes).is_some() {
        return None;
    }
    let state = ctx.state();
    let v = &state.vmcs;
    let cache = &state.svm_guard;
    if !cache.valid
        || state.mtf_enabled
        || v.read32(VmcsField32::GuestCsAccessRights).ok()? & (1 << 13) == 0
        || v.read_natural(VmcsFieldNatural::GuestCr0).ok()? & (1 << 31) == 0
        || v.read_natural(VmcsFieldNatural::GuestRflags).ok()? & ((1 << 8) | (1 << 16)) != 0
        || v.read_natural(VmcsFieldNatural::GuestDr7).ok()? & 0x20ff != 0
        || state
            .svm_rejected_pages
            .contains(&(window.physical.as_u64() & !4095))
    {
        return None;
    }
    let budget = instruction_budget(ctx);
    if budget <= InstructionBatch::COUNTER_DEADLINE_MARGIN {
        return None;
    }
    let root = v.read_natural(VmcsFieldNatural::GuestCr3).ok()? & 0x000f_ffff_ffff_f000;
    if cache.root != root {
        return None;
    }
    let key = window.linear ^ root.rotate_left(17);
    let plan = &cache.page_plans[page_plan_slot(window.linear, root)];
    if !plan.valid
        || plan.key != key
        || plan.tree_generation != cache.tree_generation
        || plan.code_epoch != cache.code_epoch
    {
        return None;
    }
    // SAFETY: valid is set only after the batch has been initialized.
    let cached = unsafe { plan.batch.assume_init_ref() };
    if !cached.page_execution
        || cached.start != v.read_natural(VmcsFieldNatural::GuestRip).ok()?
        || cached.pages[0] != window.physical.as_u64() & !4095
        || window.linear
            != cached
                .start
                .checked_add(v.read_natural(VmcsFieldNatural::GuestCsBase).ok()?)?
        || !cached.pages[..cached.code_page_count].iter().all(|&page| {
            cache.code[..cache.code_count]
                .iter()
                .any(|proof| proof.page == page)
        })
    {
        return None;
    }
    let mut batch = *cached;
    batch.instruction_budget = budget;
    Some(batch)
}

fn remember_page_plan<C: VmContext>(
    ctx: &mut C,
    batch: &InstructionBatch,
    window: &super::svm::InstructionWindow,
) {
    if !batch.page_execution || !ctx.state().svm_guard.valid {
        return;
    }
    let root = ctx.state().svm_guard.root;
    let cache = &mut ctx.state_mut().svm_guard;
    let tree_generation = cache.tree_generation;
    let code_epoch = cache.code_epoch;
    let plan = &mut cache.page_plans[page_plan_slot(window.linear, root)];
    plan.valid = false;
    plan.key = window.linear ^ root.rotate_left(17);
    plan.tree_generation = tree_generation;
    plan.code_epoch = code_epoch;
    plan.batch.write(*batch);
    plan.valid = true;
}

pub(crate) fn prepare<C: VmContext>(
    ctx: &mut C,
    can_loop: bool,
    can_guard_page_tables: bool,
    window: &super::svm::InstructionWindow,
) -> Option<InstructionBatch> {
    if let Some(batch) = cached_page_plan(ctx, can_loop, can_guard_page_tables, window) {
        return Some(batch);
    }
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
    let repeat_entry = long && repeat_len(&window.bytes).is_some();
    if long
        && !repeat_entry
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
        // When the global table guard is dirty, a full alias walk cannot be
        // reused. Try the bounded reachable-code proof first for a hazardous
        // page; it checks outgoing translations and validated code bytes.
        if (hazards[0].count == batch.page_breakpoints.len()
            || (hazards[0].count != 0
                && ctx.state().svm_guard.gate_ready
                && ctx.state().svm_guard.gate_dirty)
            || ctx
                .state()
                .svm_guard
                .region_proofs
                .iter()
                .any(|proof| proof.valid && proof.page == page))
            && collect_reachable_breakpoints(ctx, &mut batch, window.linear).is_some()
        {
            remember_page_plan(ctx, &batch, window);
            return Some(batch);
        }
        let current = window.linear & !4095;
        let recent = ctx.state().svm_recent_pages;
        let mut updated = [u64::MAX; SVM_RECENT_PAGE_CAPACITY];
        updated[0] = current;
        let mut next = 1;
        // Code-page locality decays quickly. Limit speculative code scans to
        // the most recent pages; every omitted page still traps on entry.
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
                if collect_reachable_breakpoints(ctx, &mut batch, window.linear).is_some() {
                    break;
                }
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
    remember_page_plan(ctx, &batch, window);
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
    let cs = v.read32(VmcsField32::GuestCsAccessRights).ok()?;
    let long = cs & (1 << 13) != 0;
    // A restarted REP may carry RF until the whole string operation retires.
    // Its bounded chunk finishes before the endpoint breakpoint, clearing
    // RF in hardware. Keep the conservative RF rule for other instructions.
    let repeat_entry = long && repeat_len(&window.bytes).is_some();
    if flags & (1 << 8) != 0
        || (flags & (1 << 16) != 0 && !repeat_entry)
        || state.mtf_enabled
        || v.read_natural(VmcsFieldNatural::GuestDr7).ok()? & 0x20ff != 0
    {
        return None;
    }
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
        pages: [0; 68],
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
        global_execution: false,
        endpoint_intercepted: false,
        instruction_budget: budget,
    };
    batch.pages[0] = physical.as_u64() & !4095;
    // A page guard would stop immediately at REP and force one scalar
    // iteration. Prefer its validated, deadline-bounded native chunk instead.
    if long
        && allow_page
        && can_loop
        && budget > InstructionBatch::COUNTER_DEADLINE_MARGIN
        && repeat_len(&bytes[..available]).is_none()
    {
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
    // Counted store loops reconstruct retirements from the loop register and
    // decoded boundary; they do not need a virtualized PMU.
    if long && paged {
        if counted_store_loop(ctx, linear, &bytes[..available], &gprs, &mut batch).is_some() {
            return Some(batch);
        }
    }
    let mut changed = 0u16;
    // Consecutive PUSH instructions have a predictable stack destination
    // even though each one changes RSP. Stop tracking after any other RSP
    // write, so an unknown adjustment cannot enter the store proof.
    let mut stack_push_bytes = Some(0u64);
    let mut stores = StorePlan::default();
    let mut offset = 0;
    let mut branch_targets = [0i64; 64];
    // Outgoing branch breakpoints need a retirement counter, but no table
    // guard when this batch has made no memory writes. Without a table guard
    // we stop at the first store before decoding any later branch.
    let mut allow_branch_exits = long && can_loop;
    let mut branch_sources = [0u16; 64];
    let mut branch_count = 0;
    let mut first_branch = 0;
    if long {
        if let Some(length) = repeat_len(&bytes[..available]) {
            let original_count = state.gprs.rcx;
            let width = match bytes[length - 1] {
                0xa4 | 0xaa => 1u64,
                _ if bytes[..length].iter().any(|b| b & 0xf8 == 0x48) => 8,
                _ if bytes[..length].contains(&0x66) => 2,
                _ => 4,
            };
            let offset = state.gprs.rdi & 4095;
            // The next destination page may be unmapped or CoW. Run the
            // current mapped page as a bounded chunk, then replan.
            let page_iterations = if flags & (1 << 10) != 0 {
                if offset + width > 4096 { 0 } else { offset / width + 1 }
            } else {
                (4096 - offset) / width
            };
            let iterations = original_count.min(limit as u64).min(page_iterations);
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
            // MOVS/STOS write RAM, including unrelated code or table frames.
            // Their range proof protects this execution, not cached RAM proofs.
            batch.writes_memory = true;
        }
    }
    while batch.repeat.is_none() && batch.count < limit.min(64) {
        // Relative transfers may only enter decoded boundaries or the
        // endpoint. The retired-instruction counter accounts for their paths.
        if can_loop {
            let tail = &bytes[offset..available];
            if let Some((length, displacement)) = relative_branch(tail, long, default32) {
                if paged && batch.writes_memory && !allow_guarded_stores {
                    // Re-enter after a store before control can reach a
                    // translation changed by that store.
                    break;
                }
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
                // A jump at the entry of a rejected code page has no
                // fallthrough path. Decoding padding after it can exhaust the
                // outgoing traps and force an otherwise exact scalar step.
                if (batch.count == 1
                    && !allow_page
                    && (matches!(tail[0], 0xe9 | 0xeb)
                        || tail.starts_with(&[0x2e, 0xe9])
                        || tail.starts_with(&[0x2e, 0xeb])))
                    || offset == available
                {
                    break;
                }
                continue;
            }
        }
        let Some((length, memory, writes)) = safe_len(&bytes[offset..available], long, default32)
        else {
            break;
        };
        let instruction = &bytes[offset..offset + length];
        let modified = modified_gprs(instruction, long);
        let push = writes
            && modified & (1 << 4) != 0
            && opcode_start(instruction, long)
                .is_some_and(|(prefix, _, _)| matches!(instruction[prefix], 0x50..=0x57));
        let mut validated_push_width = None;
        if paged && batch.writes_memory && memory && !writes && !allow_guarded_stores {
            // A prior store may have rewritten this load's PTE. End the
            // batch and flush translations before executing the load.
            break;
        }
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
                let stack_depth = if push { stack_push_bytes } else { None };
                let store_changed = if stack_depth.is_some() {
                    changed & !(1 << 4)
                } else {
                    changed
                };
                let Some((start, width)) = store_range(
                    instruction,
                    rip + offset as u64,
                    &gprs,
                    store_changed,
                )
                .and_then(|(start, width)| {
                    start
                        .checked_sub(stack_depth.unwrap_or(0))
                        .map(|start| (start, width))
                })
                else {
                    break;
                };
                collect_code_tables(ctx, &mut batch, linear)?;
                if validate_store(ctx, &batch, &mut stores, start, width).is_none() {
                    break;
                }
                if push {
                    validated_push_width = Some(width);
                }
                batch.validated_stores = true;
            }
        }
        if modified & (1 << 4) != 0 {
            stack_push_bytes = stack_push_bytes
                .zip(validated_push_width)
                .and_then(|(depth, width)| depth.checked_add(width));
        }
        changed |= modified;
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
    if batch.uses_counter
        && !batch.counter_bounded
        && budget <= InstructionBatch::COUNTER_DEADLINE_MARGIN
    {
        return None;
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
        // The bounded destination proof excludes every code and translation
        // frame this instruction can write. Keeping those tables writable
        // avoids a nested fault on every page walk without ROGPT.
        batch.validated_stores = true;
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
    if batch.global_execution {
        if batch.page_breakpoint_count != 0 {
            for &page in &batch.pages[..batch.code_page_count] {
                match ctx
                    .state_mut()
                    .ept
                    .restrict_write_4k(allocator, GuestPhysAddr::new(page))
                {
                    Ok(Some(write_guard)) => {
                        let slot = guard.saved_count;
                        ctx.state_mut().svm_guard.saved[slot] =
                            super::super::vm_state::SvmGuardSaved {
                                guest: page,
                                write_guard,
                                valid: true,
                            };
                        guard.saved_count += 1;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        guard.restore(ctx, allocator);
                        return None;
                    }
                }
            }
        }
        return Some(guard);
    }
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
    if batch.page_execution || batch.guarded_stores {
        // Keep earlier hazard scans valid even for code omitted from this
        // entry's executable set. Guard its bytes as data, within the existing
        // workspace capacity; unguarded proofs are discarded before entry.
        for index in 0..ctx.state().svm_guard.code_count {
            if guard.saved_count == ctx.state().svm_guard.saved.len() {
                break;
            }
            let page = ctx.state().svm_guard.code[index].page;
            if batch.pages[..protected].contains(&page)
                || ctx.state().svm_guard.tables[..tables].contains(&page)
            {
                continue;
            }
            let slot = guard.saved_count;
            guard.saved_count += 1;
            ctx.state_mut().svm_guard.saved[slot].valid = false;
            if let Ok(Some(write_guard)) = ctx
                .state_mut()
                .ept
                .restrict_write_4k(allocator, GuestPhysAddr::new(page))
            {
                ctx.state_mut().svm_guard.saved[slot] = super::super::vm_state::SvmGuardSaved {
                    guest: page,
                    write_guard,
                    valid: true,
                };
            }
        }
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
        let fault_page = v.read64(VmcsField64::GuestPhysicalAddr).unwrap_or(0) & !4095;
        let fetch_boundary = v.read32(VmcsField32::VmExitReason).ok() == Some(37)
            && v.read_natural(VmcsFieldNatural::ExitQualification)
                .ok()
                .is_some_and(|q| q & InstructionBatch::PAGE_FETCH_BOUNDARY != 0)
            && super::svm::InstructionWindow::read(ctx)
                .ok()
                .is_some_and(|window| {
                    // The GPA must name the page containing RIP. If the fault
                    // is for the next page of a split instruction, replay it
                    // with unrestricted scalar execution instead.
                    let starts_on_fault_page = window.physical.as_u64() & !4095 == fault_page;
                    // Entry stores often need scalar replay for A/D or table
                    // writes anyway. Replanning first only adds another guard.
                    starts_on_fault_page
                        && (window.bytes[0] == 0xc3
                            || relative_branch(&window.bytes, true, false).is_some()
                            || safe_len(&window.bytes, true, false)
                                .is_some_and(|(_, _, writes)| !writes))
                });
        let write_fault = v.read32(VmcsField32::VmExitReason).ok() == Some(48)
            && v.read_natural(VmcsFieldNatural::ExitQualification)
                .unwrap_or(0)
                & 2
                != 0;
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
    fn global_counter_can_guard_two_recent_hazardous_pages() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x3000].fill(0x90);
        for page in [0x1000, 0x2000] {
            ctx.memory[page + 0x100..page + 0x103]
                .copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        }
        ctx.state_mut().svm_guard.gate_ready = true;
        ctx.state_mut().svm_guard.gate_root = 0x3000;
        ctx.state_mut().svm_recent_pages[..2].copy_from_slice(&[0x1000, 0x2000]);
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator,
            bedrock_ept::PageTableFormat::AmdNpt,
        )
        .unwrap();
        for page in [0x1000, 0x2000] {
            ctx.state_mut()
                .ept
                .map_4k(
                    &mut allocator,
                    GuestPhysAddr::new(page as u64),
                    HostPhysAddr::new(page as u64 + 0x100000),
                    bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
                    bedrock_ept::EptMemoryType::WriteBack,
                )
                .unwrap();
        }
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_global(&mut ctx, &allocator, true, true, &window).unwrap();
        assert_eq!(&batch.pages[..batch.code_page_count], &[0x1000, 0x2000]);
        assert_eq!(batch.page_breakpoint_count, 2);
        let guard = protect(&mut ctx, &allocator, &batch).unwrap();
        for page in [0x1000, 0x2000] {
            assert_eq!(
                ctx.state()
                    .ept
                    .lookup(&allocator, GuestPhysAddr::new(page))
                    .unwrap()
                    .1
                    .bits()
                    & 2,
                0
            );
        }
        guard.restore(&mut ctx, &allocator);
        for page in [0x1000, 0x2000] {
            assert_ne!(
                ctx.state()
                    .ept
                    .lookup(&allocator, GuestPhysAddr::new(page))
                    .unwrap()
                    .1
                    .bits()
                    & 2,
                0
            );
        }
    }

    #[test]
    fn global_counter_guards_a_trailing_rep_prefix() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1fff] = 0xf3;
        ctx.state_mut().svm_guard.gate_ready = true;
        ctx.state_mut().svm_guard.gate_root = 0x3000;
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator,
            bedrock_ept::PageTableFormat::AmdNpt,
        )
        .unwrap();
        ctx.state_mut().ept.map_4k(
            &mut allocator,
            GuestPhysAddr::new(0x1000),
            HostPhysAddr::new(0x101000),
            bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
            bedrock_ept::EptMemoryType::WriteBack,
        ).unwrap();
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_global(&mut ctx, &allocator, true, true, &window).unwrap();
        assert_eq!(batch.page_breakpoint_count, 1);
        assert_eq!(batch.page_breakpoints[0], 0x1fff);
        assert!(!ctx.state().ept.npt_trusted_code_4k(
            &allocator, GuestPhysAddr::new(0x1000)));
        let guard = protect(&mut ctx, &allocator, &batch).unwrap();
        assert_eq!(ctx.state().ept.lookup(&allocator, GuestPhysAddr::new(0x1000))
            .unwrap().1.bits() & 2, 0);
        guard.restore(&mut ctx, &allocator);
    }

    #[test]
    fn boundary_prefixes_use_one_breakpoint_per_possible_entry() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1ffe..0x2000].copy_from_slice(&[0x66, 0xf3]);
        let hazards = page_hazards(&mut ctx, 0x1000, None).unwrap().unwrap();
        assert_eq!(&hazards.offsets[..hazards.count], &[4094, 4095]);
        assert!(boundary_starts_covered(&hazards));

        ctx.memory[0x1ffb..0x2000].copy_from_slice(&[0x66, 0x67, 0x2e, 0xf2, 0xf3]);
        assert!(page_hazards(&mut ctx, 0x1000, None).unwrap().is_none());
    }

    #[test]
    fn global_counter_promotes_a_safe_scalar_page_with_a_write_guard() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.state_mut().svm_guard.gate_ready = true;
        ctx.state_mut().svm_guard.gate_root = 0x3000;
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator,
            bedrock_ept::PageTableFormat::AmdNpt,
        )
        .unwrap();
        ctx.state_mut()
            .ept
            .map_4k(
                &mut allocator,
                GuestPhysAddr::new(0x1000),
                HostPhysAddr::new(0x101000),
                bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
                bedrock_ept::EptMemoryType::WriteBack,
            )
            .unwrap();
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_global(&mut ctx, &allocator, true, true, &window).unwrap();
        assert!(batch.global_execution);
        assert_eq!(batch.page_breakpoint_count, 0);
        assert!(ctx.state().ept.npt_trusted_code_4k(&allocator, GuestPhysAddr::new(0x1000)));
        assert_eq!(
            ctx.state().ept.lookup(&allocator, GuestPhysAddr::new(0x1000)).unwrap().1,
            bedrock_ept::EptPermissions::READ_EXECUTE,
        );
        ctx.state_mut()
            .ept
            .invalidate_npt_code_4k(&allocator, GuestPhysAddr::new(0x1000))
            .unwrap();
        assert!(!ctx.state().ept.npt_trusted_code_4k(&allocator, GuestPhysAddr::new(0x1000)));
        assert_eq!(
            ctx.state().ept.lookup(&allocator, GuestPhysAddr::new(0x1000)).unwrap().1,
            bedrock_ept::EptPermissions::READ_WRITE,
        );
        // A faulted store to this code page must retire before the write
        // guard can be rearmed, even though the page has no code hazards.
        ctx.memory[0x1000..0x1002].copy_from_slice(&[0x89, 0x07]);
        ctx.state_mut().svm_guard.valid = false;
        let store = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(safe_len(&store.bytes, true, false).is_some_and(|(_, _, writes)| writes));
        assert!(prepare_global(&mut ctx, &allocator, true, true, &store).is_none());
        assert!(!ctx.state().ept.npt_trusted_code_4k(&allocator, GuestPhysAddr::new(0x1000)));
    }

    #[test]
    fn consecutive_pushes_use_the_updated_stack_destination() {
        let code = [0x41, 0x55, 0x41, 0x54, 0x53, 0x0f, 0x01, 0xd9];
        let ctx = paged_context(&code);
        let batch = planned(&ctx).unwrap();
        assert_eq!(batch.count, 3);
        assert!(batch.validated_stores && !batch.uses_counter);
        assert_eq!(batch.endpoint(), 0x1005);

        // A stack write into the active translation tree cannot share this
        // exact batch, even when later PUSH destinations are predictable.
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestRsp, 0x3010);
        assert!(!planned(&ctx).is_some_and(|batch| batch.count >= 3));

        let changed_rsp = paged_context(&[
            0x53, 0x48, 0x83, 0xec, 0x10, 0x55, 0x0f, 0x01, 0xd9,
        ]);
        assert!(!planned(&changed_rsp).is_some_and(|batch| batch.count >= 3));
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
    fn global_gate_instruction_fetch_rewalks_after_guard_revocation() {
        let mut ctx = paged_context(&[0x90]);
        ctx.state_mut().svm_guard.gate_root = 0x3000;
        ctx.state_mut().svm_guard.gate_ready = true;
        let first = super::super::svm::InstructionWindow::read_cached(&mut ctx).unwrap();
        assert_eq!(first.physical.as_u64(), 0x1000);
        assert_eq!(ctx.state().svm_guard.translation_count, 1);

        // A trapped page-table write makes the cached translation unusable.
        ctx.memory[0x6008..0x6010].copy_from_slice(&0x9007u64.to_le_bytes());
        ctx.memory[0x9000] = 0xcc;
        ctx.state_mut().svm_guard.gate_dirty = true;
        let second = super::super::svm::InstructionWindow::read_cached(&mut ctx).unwrap();
        assert_eq!(second.physical.as_u64(), 0x9000);
        assert_eq!(second.bytes[0], 0xcc);

        // A CR3 switch must not reuse a translation from the previous root.
        ctx.state_mut().svm_guard.gate_dirty = false;
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x2000);
        ctx.memory[0x2000..0x2008].copy_from_slice(&0x4007u64.to_le_bytes());
        ctx.memory[0x6008..0x6010].copy_from_slice(&0xa007u64.to_le_bytes());
        ctx.memory[0xa000] = 0x90;
        let third = super::super::svm::InstructionWindow::read_cached(&mut ctx).unwrap();
        assert_eq!(third.physical.as_u64(), 0xa000);
        assert_eq!(third.bytes[0], 0x90);
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
    fn paged_read_modify_write_stores_use_validated_destinations() {
        let code = [
            0x48, 0x83, 0x07, 1, // add qword [rdi],1
            0x48, 0xff, 0x07, // inc qword [rdi]
            0x0f, 0x94, 0x47, 8, // sete byte [rdi+8]
            0x0f, 0x01, 0xd9,
        ];
        let mut ctx = paged_context(&code);
        let batch = planned(&ctx).unwrap();
        assert_eq!(batch.count, 3);
        assert!(batch.validated_stores && batch.writes_memory);
        for destination in [0x1000, 0x3000, 0x6000] {
            ctx.state_mut().gprs.rdi = destination;
            assert!(planned(&ctx).is_none());
        }
        assert_eq!(store_range(&[0x48, 0x87, 0x07], 0x1000, &[0; 16], 0), None);
    }

    #[test]
    fn movsxd_tracks_its_register_write_before_a_store() {
        let ctx = paged_context(&[
            0x48, 0x63, 0x07, // movsxd rax,dword [rdi]
            0x48, 0x89, 0x47, 8, // mov [rdi+8],rax
            0x0f, 0x01, 0xd9,
        ]);
        let batch = planned(&ctx).unwrap();
        assert_eq!(batch.count, 2);
        assert!(batch.validated_stores);
        let ctx = paged_context(&[
            0x48, 0x63, 0x3f, // movsxd rdi,dword [rdi]
            0x48, 0x89, 0x07, // mov [rdi],rax: address changed
            0x0f, 0x01, 0xd9,
        ]);
        assert!(planned(&ctx).is_none());
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
        assert_eq!(b.branch_exit_count, 1); // Trap the target inside MOV.
        assert_eq!(b.branch_exits[0], 0x1005);
        let ctx = paged_context(&[0x90, 0x90, 0xeb, 0x7f, 0x0f, 0xc7, 0xf0]);
        let b = planned(&ctx).unwrap();
        assert!(!b.uses_counter);
        assert_eq!(b.branch_exit_count, 1);
        assert_eq!(b.branch_exits[0], 0x1083);
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
    fn rejected_page_entry_jump_ignores_unreachable_fallthrough() {
        let ctx = paged_context(&[
            0xe9, 0xfb, 0x00, 0x00, 0x00, // jump to 0x1100
            0x74, 0x70, 0x74, 0x70, 0x74, 0x70, 0x74, 0x70,
        ]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_verified(&ctx, true, false, true, &window).unwrap();
        assert_eq!(batch.count, 1);
        assert_eq!(&batch.branch_exits[..batch.branch_exit_count], &[0x1100]);
        assert!(!batch.uses_counter);
        let ctx = paged_context(&[
            0x2e, 0xe9, 0xfa, 0x00, 0x00, 0x00, // CS:JMP to 0x1100
            0x74, 0x70, 0x74, 0x70, 0x74, 0x70, 0x74, 0x70,
        ]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let prefixed = prepare_verified(&ctx, true, false, true, &window).unwrap();
        assert_eq!(prefixed.count, 1);
        assert_eq!(
            &prefixed.branch_exits[..prefixed.branch_exit_count],
            &[0x1100]
        );
        assert!(!prefixed.uses_counter);
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
        assert!(cached_page_hazards(&mut ctx, 0xb000).is_some());
        assert!(cached_page_hazards(&mut ctx, 0xc000).is_some());
        let mappings = [
            0x1000, 0x3000, 0x4000, 0x5000, 0x6000, 0x7000, 0x8000, 0x9000, 0xa000, 0xb000,
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
        retain_translation_cache(&mut ctx, Some(&batch), Some(&window), false);
        assert!(
            ctx.state().svm_guard.code[..ctx.state().svm_guard.code_count]
                .iter()
                .any(|proof| proof.page == 0xb000)
        );
        assert!(
            !ctx.state().svm_guard.code[..ctx.state().svm_guard.code_count]
                .iter()
                .any(|proof| proof.page == 0xc000)
        ); // Unmapped pages cannot retain their proof.
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
        let guard = protect(&mut ctx, &allocator, &batch).unwrap();
        ctx.state()
            .vmcs
            .write64(VmcsField64::GuestPhysicalAddr, 0xb000)
            .unwrap();
        assert!(guard.restore(&mut ctx, &allocator)); // Cached-only code writes require scalar replay too.
        assert_eq!(
            ctx.state()
                .ept
                .lookup(&allocator, GuestPhysAddr::new(0xb000))
                .unwrap()
                .1,
            bedrock_ept::EptPermissions::READ_WRITE_EXECUTE
        );
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
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(prepare_verified(&ctx, false, false, false, &window)
            .is_some_and(|batch| batch.counted_loop.is_some()));
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
    fn counted_store_loops_accept_read_modify_write_without_counter_payloads() {
        let code = [
            0x4c, 0x8b, 0x0f, // mov r9,[rdi]
            0x49, 0x31, 0xc1, // xor r9,rax
            0x49, 0xc1, 0xc1, 0x0d, // rol r9,13
            0x49, 0x81, 0xc1, 1, 0, 0, 0, // add r9,1
            0x4c, 0x89, 0x0f, // mov [rdi],r9
            0x4c, 0x31, 0xc8, // xor rax,r9
            0x48, 0x83, 0xc7, 8, // add rdi,8
            0x48, 0xff, 0xc9, // dec rcx
            0x75, 0xe0, // jnz to the first load
        ];
        let mut ctx = paged_context(&code);
        ctx.state_mut().gprs.rcx = 4096;
        let batch = planned(&ctx).unwrap();
        assert!(batch.counted_loop.is_some() && batch.validated_stores);
        assert_eq!(batch.counted_loop.unwrap().iterations, 4096);
        assert_eq!(batch.count, 9);

        // A payload read of the clamped counter would change the store value.
        ctx.memory[0x1003..0x1006].copy_from_slice(&[0x49, 0x31, 0xc9]); // xor r9,rcx
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        // A changed source pointer would invalidate the prechecked range.
        ctx.memory[0x1003..0x1006].copy_from_slice(&[0x49, 0x31, 0xf9]); // xor r9,rdi
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        // ADD after DEC would replace the flags consumed by JNZ.
        ctx.memory[0x1003..0x1006].copy_from_slice(&code[3..6]);
        ctx.memory[0x1017..0x101e]
            .copy_from_slice(&[0x48, 0xff, 0xc9, 0x48, 0x83, 0xc7, 8]);
        assert!(!planned(&ctx).is_some_and(|b| b.counted_loop.is_some()));
        assert!(!counted_store_payload(&[0x49, 0x90], 1)); // XCHG, not NOP.
    }

    #[test]
    fn paged_store_without_table_guards_cannot_batch_a_later_load() {
        let ctx = paged_context(&[
            0x48, 0x8b, 0x1e, // mov rbx,[rsi]
            0x48, 0x89, 0x07, // mov [rdi],rax: may replace the read PTE
            0x48, 0x8b, 0x16, // mov rdx,[rsi]
            0x31, 0xc0, 0x0f, 0x01, 0xd9,
        ]);
        let batch = planned(&ctx).unwrap();
        assert!(batch.writes_memory && batch.validated_stores);
        assert_eq!(batch.endpoint(), 0x1006); // Stop before the second load.
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
    fn recent_page_window_keeps_the_four_breakpoint_limit() {
        extern crate std;
        let mut ctx = paged_context(&[0x90]);
        ctx.memory.resize(0x60000, 0);
        ctx.memory[0x10000..0x50000].fill(0x90);
        for index in 0..SVM_CODE_PAGE_CAPACITY {
            let virtual_page = (index + 1) * 4096;
            let physical_page = 0x10000 + index * 4096;
            ctx.memory[0x6000 + (index + 1) * 8..0x6008 + (index + 1) * 8]
                .copy_from_slice(&(physical_page as u64 | 7).to_le_bytes());
            if index < SVM_RECENT_PAGE_CAPACITY {
                ctx.state_mut().svm_recent_pages[index] = virtual_page as u64;
            }
        }
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(batch.code_page_count, SVM_RECENT_PAGE_CAPACITY);
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
        // A breakpoint on the partial opcode stops either virtual adjacency
        // before RDRAND can execute across the physical boundary.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x1ffe..0x2000].copy_from_slice(&[0x0f, 0xc7]);
        ctx.memory[0x2000] = 0xf0;
        assert!(page_safe(&ctx, 0x1000).is_some());
        assert!(page_safe(&ctx, 0x2000).is_some());
        let guarded = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(guarded.code_page_count, 2);
        assert!(guarded.page_breakpoints[..guarded.page_breakpoint_count]
            .contains(&0x1ffe));
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
        ctx.memory.resize(8 * 1024 * 1024, 0);
        ctx.memory[0x1000..0x2000].fill(0x90);
        for index in 1..150 {
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
        assert_eq!(ctx.state().svm_guard.count, 153);
        assert!(ctx.state().svm_guard.tables[..94].contains(&(0x7000 + 90 * 4096)));
        // Simulate returning to userspace before changing RAM.
        ctx.state_mut().svm_guard.valid = false;
        for index in 150..512 {
            let table = 0x7000 + index * 4096;
            ctx.memory[0x5000 + index * 8..0x5008 + index * 8]
                .copy_from_slice(&(table as u64 | 7).to_le_bytes());
        }
        ctx.memory[0x4008..0x4010].copy_from_slice(&0x300007u64.to_le_bytes());
        for index in 0..512 {
            let table = 0x400000 + index * 4096;
            ctx.memory[0x300000 + index * 8..0x300008 + index * 8]
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
    fn alias_proofs_reuse_multiple_hazardous_pages() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        ctx.memory[0x7100..0x7103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        ctx.state_mut().svm_recent_pages.fill(u64::MAX);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let first = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(
            &first.page_breakpoints[..first.page_breakpoint_count],
            &[0x1100]
        );
        let first_slot = ctx.state().svm_guard.alias_last_hit;

        ctx.set_guest_rip(0x7000);
        ctx.state_mut().svm_recent_pages.fill(u64::MAX);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let second = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(
            &second.page_breakpoints[..second.page_breakpoint_count],
            &[0x7100]
        );
        assert_ne!(ctx.state().svm_guard.alias_last_hit, first_slot);

        ctx.state_mut().svm_guard.aliases[0].table = 0;
        ctx.set_guest_rip(0x1000);
        ctx.state_mut().svm_recent_pages.fill(u64::MAX);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let first_again = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(
            &first_again.page_breakpoints[..first_again.page_breakpoint_count],
            &[0x1100]
        );
        assert_eq!(ctx.state().svm_guard.alias_last_hit, first_slot);
        assert_eq!(ctx.state().svm_guard.aliases[0].table, 0);
    }

    #[test]
    fn alias_proof_cache_retains_more_than_thirty_two_distinct_keys() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        prepare(&mut ctx, true, true, &window).unwrap();
        let mut hazard = cached_page_hazards(&mut ctx, 0x1000).unwrap();
        let mut batch = planned(&ctx).unwrap();
        assert_eq!(batch.code_page_count, 1);

        let mut first_slot = None;
        for offset in 0x100..0x130 {
            hazard.offsets[0] = offset;
            collect_page_breakpoints(&mut ctx, &mut batch, &[hazard]).unwrap();
            if first_slot.is_none() {
                first_slot = Some(ctx.state().svm_guard.alias_last_hit);
            }
        }
        assert!(ctx.state().svm_guard.alias_proofs[first_slot.unwrap()].valid);
        let cursor = ctx.state().svm_guard.alias_cursor;
        hazard.offsets[0] = 0x100;
        collect_page_breakpoints(&mut ctx, &mut batch, &[hazard]).unwrap();
        assert_eq!(ctx.state().svm_guard.alias_cursor, cursor);
        assert_eq!(ctx.state().svm_guard.alias_last_hit, first_slot.unwrap());
    }

    #[test]
    fn alias_walk_covers_more_than_five_hundred_twelve_table_paths() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory.resize(8 * 1024 * 1024, 0);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        for index in 1..512 {
            let table = 0x100000 + index * 4096;
            ctx.memory[0x5000 + index * 8..0x5008 + index * 8]
                .copy_from_slice(&(table as u64 | 7).to_le_bytes());
        }
        ctx.memory[0x4008..0x4010].copy_from_slice(&0x500007u64.to_le_bytes());
        for index in 0..100 {
            let table = 0x600000 + index * 4096;
            ctx.memory[0x500000 + index * 8..0x500008 + index * 8]
                .copy_from_slice(&(table as u64 | 7).to_le_bytes());
        }
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(&batch.page_breakpoints[..batch.page_breakpoint_count], &[0x1100]);
        assert!(ctx.state().svm_guard.alias_proof.valid);
        assert!(ctx.state().svm_guard.aliases[600].table != 0);
    }

    #[test]
    fn alias_proofs_survive_bounded_guard_expiry_only_while_global_gate_holds() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let first = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(&first.page_breakpoints[..first.page_breakpoint_count], &[0x1100]);
        let proof_slot = ctx.state().svm_guard.alias_last_hit;

        let root = ctx.state().svm_guard.root;
        ctx.state_mut().svm_guard.gate_root = root;
        ctx.state_mut().svm_guard.gate_ready = true;
        ctx.state_mut().svm_guard.valid = false;
        collect_translation_tree_pages(&mut ctx, &[0x1000]).unwrap();
        assert!(ctx.state().svm_guard.alias_proofs[proof_slot].valid);

        let hazard = cached_page_hazards(&mut ctx, 0x1000).unwrap();
        let cursor = ctx.state().svm_guard.alias_cursor;
        ctx.state_mut().svm_guard.aliases[0].table = 0;
        let mut reused = first;
        collect_page_breakpoints(&mut ctx, &mut reused, &[hazard]).unwrap();
        assert_eq!(ctx.state().svm_guard.alias_cursor, cursor);
        assert_eq!(ctx.state().svm_guard.aliases[0].table, 0);

        // Releasing a table write invalidates the alias proof; the new
        // executable mapping must be enumerated before another page batch.
        ctx.state_mut().svm_guard.gate_dirty = true;
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x1007u64.to_le_bytes());
        ctx.state_mut().svm_guard.valid = false;
        collect_translation_tree_pages(&mut ctx, &[0x1000]).unwrap();
        assert!(!ctx.state().svm_guard.alias_proofs[proof_slot].valid);
        let hazard = cached_page_hazards(&mut ctx, 0x1000).unwrap();
        let mut rebuilt = first;
        collect_page_breakpoints(&mut ctx, &mut rebuilt, &[hazard]).unwrap();
        assert_eq!(
            &rebuilt.page_breakpoints[..rebuilt.page_breakpoint_count],
            &[0x1100, 0x9100]
        );
    }

    #[test]
    fn bounded_tree_reuses_global_gate_until_table_write() {
        let mut ctx = paged_context(&[0x90]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        prepare(&mut ctx, true, true, &window).unwrap();
        let root = ctx.state().svm_guard.root;
        let count = ctx.state().svm_guard.count;
        for index in 0..count {
            ctx.state_mut().svm_guard.gate_tables[index] = ctx.state().svm_guard.tables[index];
        }
        ctx.state_mut().svm_guard.gate_count = count;
        ctx.state_mut().svm_guard.gate_root = root;
        ctx.state_mut().svm_guard.gate_ready = true;

        ctx.state_mut().svm_guard.valid = false;
        ctx.state_mut().svm_guard.tables[0] = 0;
        ctx.state_mut().svm_guard.levels[0] = 0;
        ctx.state_mut().svm_guard.translation_count = 1;
        ctx.state_mut().svm_guard.translations[0] = (0x1000, 0x9000);
        collect_translation_tree_pages(&mut ctx, &[0x1000]).unwrap();
        assert_eq!(ctx.state().svm_guard.tables[0], root);
        assert_eq!(ctx.state().svm_guard.count, count);
        assert_eq!(ctx.state().svm_guard.levels[0], 0); // No table walk.
        assert_eq!(ctx.state().svm_guard.translation_count, 0);

        ctx.state_mut().svm_guard.valid = false;
        assert!(collect_translation_tree_pages(&mut ctx, &[0x3000]).is_none());

        ctx.state_mut().svm_guard.gate_dirty = true;
        ctx.state_mut().svm_guard.valid = false;
        collect_translation_tree_pages(&mut ctx, &[0x1000]).unwrap();
        assert_eq!(ctx.state().svm_guard.levels[0], 4); // Rewalked.
    }

    #[test]
    fn leaf_refresh_requires_only_guest_leaf_writes_on_the_same_root() {
        let mut ctx = paged_context(&[0x90]);
        let guard = &mut ctx.state_mut().svm_guard;
        guard.gate_ready = true;
        guard.gate_dirty = true;
        guard.gate_root = 0x3000;
        guard.gate_mapping_generation = 7;
        guard.gate_count = 2;
        guard.gate_levels[0] = 4;
        guard.gate_levels[1] = 1;
        guard.gate_guards[0].valid = true;
        guard.gate_guards[1].valid = false;
        assert!(can_refresh_leaf_only(guard, 0x3000, 7));
        assert!(!can_refresh_leaf_only(guard, 0x8000, 7));
        assert!(!can_refresh_leaf_only(guard, 0x3000, 8));

        guard.gate_links_untrusted = true;
        assert!(!can_refresh_leaf_only(guard, 0x3000, 7));
        guard.gate_links_untrusted = false;
        guard.gate_guards[0].valid = false;
        assert!(!can_refresh_leaf_only(guard, 0x3000, 7));
        guard.gate_guards[0].valid = true;
        guard.gate_guards[1].valid = true;
        assert!(!can_refresh_leaf_only(guard, 0x3000, 7));
    }

    #[test]
    fn dirty_root_switch_drops_old_alias_proofs() {
        let mut ctx = paged_context(&[0x90]);
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator,
            bedrock_ept::PageTableFormat::AmdNpt,
        ).unwrap();
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        let count = ctx.state().svm_guard.count;
        let pages = ctx.state().svm_guard.tables[..count].to_vec();
        for page in pages {
            ctx.state_mut().ept.map_4k(
                &mut allocator,
                GuestPhysAddr::new(page),
                HostPhysAddr::new(page + 0x1000000),
                bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
                bedrock_ept::EptMemoryType::WriteBack,
            ).unwrap();
        }
        let guard = &mut ctx.state_mut().svm_guard;
        guard.gate_ready = true;
        guard.gate_dirty = true;
        guard.gate_root = 0x9000;
        guard.alias_proofs[0].valid = true;
        guard.alias_proofs[0].root = 0x9000;
        refresh_global_tree(&mut ctx, &allocator, true);
        assert!(!ctx.state().svm_guard.alias_proofs[0].valid);
    }

    #[test]
    fn leaf_rearm_keeps_retained_guard_registry_in_sync() {
        let mut ctx = paged_context(&[0x90]);
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator,
            bedrock_ept::PageTableFormat::AmdNpt,
        )
        .unwrap();
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        let count = ctx.state().svm_guard.count;
        let pages = ctx.state().svm_guard.tables[..count].to_vec();
        for page in pages {
            ctx.state_mut().ept.map_4k(
                &mut allocator,
                GuestPhysAddr::new(page),
                HostPhysAddr::new(page + 0x1000000),
                bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
                bedrock_ept::EptMemoryType::WriteBack,
            ).unwrap();
        }
        refresh_global_tree(&mut ctx, &allocator, true);
        assert!(ctx.state().svm_guard.gate_ready);
        let leaf = (0..ctx.state().svm_guard.gate_count)
            .find(|&index| ctx.state().svm_guard.gate_levels[index] == 1)
            .unwrap();
        let page = ctx.state().svm_guard.gate_tables[leaf];
        ctx.state_mut().svm_guard.alias_proofs[0].valid = true;
        ctx.state_mut().svm_guard.alias_proofs[0].page_count = 1;
        ctx.state_mut().svm_guard.alias_proofs[0].pages[0] = 0xdead000;
        assert!(release_global_table_write(&mut ctx, &allocator, page));
        assert!(!ctx.state().svm_guard.gate_guards[leaf].valid);
        assert!(ctx.state().svm_guard.alias_proofs[0].valid);
        refresh_global_tree(&mut ctx, &allocator, true);
        assert!(ctx.state().svm_guard.gate_ready);
        assert!(ctx.state().svm_guard.gate_guards[leaf].valid);
        assert!(ctx.state().svm_guard.alias_proofs[0].valid);
        assert!(retained_guard_for(&ctx.state().svm_guard, page).is_some());
        assert!(release_global_table_write(&mut ctx, &allocator, page));
        ctx.memory[page as usize + 0x100..page as usize + 0x108]
            .copy_from_slice(&0xdead007u64.to_le_bytes());
        refresh_global_tree(&mut ctx, &allocator, true);
        assert!(!ctx.state().svm_guard.alias_proofs[0].valid);
    }

    #[test]
    fn leaf_write_retains_only_unaffected_alias_proofs() {
        let mut ctx = paged_context(&[0x90]);
        let guard = &mut ctx.state_mut().svm_guard;
        guard.gate_count = 1;
        guard.gate_tables[0] = 0x6000;
        guard.gate_levels[0] = 1;
        guard.gate_guards[0].valid = false;
        guard.alias_proofs[0].valid = true;
        guard.alias_proofs[0].page_count = 1;
        guard.alias_proofs[0].pages[0] = 0xdead000;

        snapshot_released_leaf(&mut ctx, 0x6000).unwrap();
        invalidate_alias_proofs_for_released_leaves(&mut ctx).unwrap();
        assert!(ctx.state().svm_guard.alias_proofs[0].valid);
        snapshot_released_leaf(&mut ctx, 0x6000).unwrap();
        ctx.memory[0x6080..0x6088].copy_from_slice(&(0xdead007u64 | (1 << 63)).to_le_bytes());
        invalidate_alias_proofs_for_released_leaves(&mut ctx).unwrap();
        assert!(ctx.state().svm_guard.alias_proofs[0].valid);
        snapshot_released_leaf(&mut ctx, 0x6000).unwrap();
        ctx.memory[0x6080..0x6088].copy_from_slice(&0xdead007u64.to_le_bytes());
        invalidate_alias_proofs_for_released_leaves(&mut ctx).unwrap();
        assert!(!ctx.state().svm_guard.alias_proofs[0].valid);

        ctx.state_mut().svm_guard.alias_proofs[0].valid = true;
        snapshot_released_leaf(&mut ctx, 0x6000).unwrap();
        ctx.memory[0x6090..0x6098].copy_from_slice(&0xbeef007u64.to_le_bytes());
        invalidate_alias_proofs_for_released_leaves(&mut ctx).unwrap();
        assert!(ctx.state().svm_guard.alias_proofs[0].valid);
    }

    #[test]
    fn released_leaf_index_distinguishes_colliding_physical_pages() {
        let mut set = [0; SVM_LEAF_PAGE_INDEX_CAPACITY];
        let collision = (SVM_LEAF_PAGE_INDEX_CAPACITY as u64) << 12;
        leaf_page_insert(&mut set, 0).unwrap();
        leaf_page_insert(&mut set, collision).unwrap();
        assert!(leaf_page_contains(&set, 0));
        assert!(leaf_page_contains(&set, collision));
        assert!(!leaf_page_contains(&set, collision * 2));
    }

    #[test]
    fn full_rebuild_retains_proofs_only_without_new_executable_aliases() {
        let mut ctx = paged_context(&[0x90]);
        let generation = ctx.state().ept.mapping_generation();
        let guard = &mut ctx.state_mut().svm_guard;
        guard.gate_ready = true;
        guard.gate_dirty = true;
        guard.gate_root = 0x3000;
        guard.gate_mapping_generation = generation;
        guard.gate_count = 1;
        guard.gate_tables[0] = 0x6000;
        guard.gate_levels[0] = 2;
        guard.gate_guards[0].valid = false;
        guard.gate_upper_count = 1;
        guard.gate_upper_starts[0] = 0;
        guard.gate_upper_lengths[0] = 1;
        guard.gate_upper_edges[0] = SvmAliasEdge { slot: 9, entry: 0x7007 };
        guard.alias_proofs[0].valid = true;
        guard.alias_proofs[0].root = 0x3000;
        guard.alias_proofs[0].page_count = 1;
        guard.alias_proofs[0].pages[0] = 0x1000;

        ctx.memory[0x6000..0x7000].fill(0);
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x7007u64.to_le_bytes());
        assert!(alias_proof_retained(&alias_proofs_safe_across_rebuild(&ctx, 0x3000).unwrap(), 0));
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x8007u64.to_le_bytes());
        assert!(alias_proofs_safe_across_rebuild(&ctx, 0x3000).is_none());
        ctx.memory[0x6048..0x6050].copy_from_slice(&(0x8007u64 | (1 << 63)).to_le_bytes());
        assert!(alias_proof_retained(&alias_proofs_safe_across_rebuild(&ctx, 0x3000).unwrap(), 0));

        ctx.state_mut().svm_guard.gate_levels[0] = 1;
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x1007u64.to_le_bytes());
        assert!(!alias_proof_retained(&alias_proofs_safe_across_rebuild(&ctx, 0x3000).unwrap(), 0));
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x2007u64.to_le_bytes());
        assert!(alias_proof_retained(&alias_proofs_safe_across_rebuild(&ctx, 0x3000).unwrap(), 0));
    }

    #[test]
    fn guarded_table_links_follow_root_changes_and_write_invalidation() {
        let mut ctx = paged_context(&[0x90]);
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        let count = ctx.state().svm_guard.count;
        for index in 0..count {
            let guard = &mut ctx.state_mut().svm_guard;
            guard.gate_tables[index] = guard.tables[index];
            guard.gate_levels[index] = guard.levels[index];
            guard.gate_children[index] = guard.children[index];
            guard.gate_guards[index].guest = guard.tables[index];
            guard.gate_guards[index].valid = true;
        }
        ctx.state_mut().svm_guard.gate_count = count;
        ctx.state_mut().svm_guard.gate_root = 0x3000;
        ctx.state_mut().svm_guard.gate_ready = true;
        ctx.memory[0x8000..0x8008].copy_from_slice(&0x4007u64.to_le_bytes());
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x8000);

        // An unchanged guarded table can reuse its child links under another
        // CR3. Its bytes are intentionally changed here to model a host write
        // that bypassed NPT, which must then revoke that reuse.
        ctx.memory[0x4000..0x4008].copy_from_slice(&0x9007u64.to_le_bytes());
        ctx.state_mut().svm_guard.valid = false;
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        assert!(ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&0x5000));
        assert!(!ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&0x9000));

        ctx.state_mut().svm_guard.gate_links_untrusted = true;
        ctx.state_mut().svm_guard.valid = false;
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        assert!(ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&0x9000));
        assert!(!ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&0x5000));

        // A guest write releases the affected table's guard; other guarded
        // tables may still reuse their links while this one is read again.
        ctx.memory[0x4000..0x4008].copy_from_slice(&0xa007u64.to_le_bytes());
        let written = ctx.state().svm_guard.gate_tables[..count]
            .iter().position(|&page| page == 0x4000).unwrap();
        ctx.state_mut().svm_guard.gate_links_untrusted = false;
        ctx.state_mut().svm_guard.gate_guards[written].valid = false;
        ctx.state_mut().svm_guard.gate_dirty = true;
        ctx.state_mut().svm_guard.valid = false;
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        assert!(ctx.state().svm_guard.tables[..ctx.state().svm_guard.count].contains(&0xa000));
    }

    #[test]
    fn guarded_table_links_reuse_children_beyond_first_bitmap_word() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory.resize(4 * 1024 * 1024, 0);
        for index in 1..150 {
            let table = 0x7000 + index * 4096;
            ctx.memory[0x5000 + index * 8..0x5008 + index * 8]
                .copy_from_slice(&(table as u64 | 7).to_le_bytes());
        }
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        let count = ctx.state().svm_guard.count;
        assert_eq!(count, 153);
        for index in 0..count {
            let guard = &mut ctx.state_mut().svm_guard;
            guard.gate_tables[index] = guard.tables[index];
            guard.gate_levels[index] = guard.levels[index];
            guard.gate_children[index] = guard.children[index];
            guard.gate_guards[index].valid = true;
        }
        ctx.state_mut().svm_guard.gate_count = count;
        ctx.state_mut().svm_guard.gate_root = 0x3000;
        ctx.state_mut().svm_guard.gate_ready = true;
        ctx.memory[0x300000..0x300008].copy_from_slice(&0x4007u64.to_le_bytes());
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x300000);
        collect_translation_tree_pages(&mut ctx, &[]).unwrap();
        assert_eq!(ctx.state().svm_guard.count, count);
        assert!(ctx.state().svm_guard.tables[..count].contains(&(0x7000 + 149 * 4096)));
    }

    #[test]
    fn guarded_alias_edges_follow_host_write_invalidation() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        collect_translation_tree_pages(&mut ctx, &[0x1000]).unwrap();
        let count = ctx.state().svm_guard.count;
        let edge_count = ctx.state().svm_guard.upper_count;
        for index in 0..count {
            let guard = &mut ctx.state_mut().svm_guard;
            guard.gate_tables[index] = guard.tables[index];
            guard.gate_levels[index] = guard.levels[index];
            guard.gate_upper_starts[index] = guard.upper_starts[index];
            guard.gate_upper_lengths[index] = guard.upper_lengths[index];
            guard.gate_guards[index].valid = true;
        }
        for index in 0..edge_count {
            let guard = &mut ctx.state_mut().svm_guard;
            guard.gate_upper_edges[index] = guard.upper_edges[index];
        }
        ctx.state_mut().svm_guard.gate_count = count;
        ctx.state_mut().svm_guard.gate_upper_count = edge_count;

        let hazard = cached_page_hazards(&mut ctx, 0x1000).unwrap();
        let mut batch = planned(&ctx).unwrap();
        collect_page_breakpoints(&mut ctx, &mut batch, &[hazard]).unwrap();
        assert_eq!(&batch.page_breakpoints[..batch.page_breakpoint_count], &[0x1100]);

        // A host write bypasses NPT. The next walk must read the changed
        // table instead of trusting its previously guarded edge list.
        ctx.memory[0x4000..0x4008].fill(0);
        ctx.memory[0x4008..0x4010].copy_from_slice(&0x5007u64.to_le_bytes());
        ctx.state_mut().svm_guard.gate_links_untrusted = true;
        for proof in &mut ctx.state_mut().svm_guard.alias_proofs {
            proof.valid = false;
        }
        collect_page_breakpoints(&mut ctx, &mut batch, &[hazard]).unwrap();
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x4000_1100]
        );

        // A guest write drops that table's NPT guard and must likewise force
        // a fresh read, even when the other upper tables stay guarded.
        ctx.memory[0x4008..0x4010].fill(0);
        ctx.memory[0x4010..0x4018].copy_from_slice(&0x5007u64.to_le_bytes());
        let written = ctx.state().svm_guard.gate_tables[..count]
            .iter().position(|&page| page == 0x4000).unwrap();
        ctx.state_mut().svm_guard.gate_links_untrusted = false;
        ctx.state_mut().svm_guard.gate_guards[written].valid = false;
        for proof in &mut ctx.state_mut().svm_guard.alias_proofs {
            proof.valid = false;
        }
        collect_page_breakpoints(&mut ctx, &mut batch, &[hazard]).unwrap();
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x8000_1100]
        );
    }

    #[test]
    fn cached_page_plan_requires_live_code_tree_and_deadline() {
        let mut ctx = paged_context(&[0x90]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert!(cached_page_plan(&ctx, true, true, &window).is_some());

        ctx.state_mut().stop_at_tsc = Some(InstructionBatch::COUNTER_DEADLINE_MARGIN);
        assert!(cached_page_plan(&ctx, true, true, &window).is_none());
        ctx.state_mut().stop_at_tsc = None;

        let code_count = ctx.state().svm_guard.code_count;
        ctx.state_mut().svm_guard.code_count = 0;
        assert!(cached_page_plan(&ctx, true, true, &window).is_none());
        ctx.state_mut().svm_guard.code_count = code_count;

        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x2000);
        assert!(cached_page_plan(&ctx, true, true, &window).is_none());
        ctx.vmcs_setup()
            .set_field_natural(VmcsFieldNatural::GuestCr3, 0x3000);

        ctx.state_mut().svm_guard.valid = false;
        assert!(cached_page_plan(&ctx, true, true, &window).is_none());
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
    fn reachable_region_traps_unknowns_without_guarding_unreachable_alias_hazards() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1000..0x1003].copy_from_slice(&[0x90, 0xeb, 0x2e]);
        ctx.memory[0x1020..0x1022].copy_from_slice(&[0xf3, 0xa4]);
        ctx.memory[0x1031] = 0xc3;
        for offset in [0x100, 0x200] {
            ctx.memory[0x1000 + offset..0x1002 + offset].copy_from_slice(&[0xf3, 0xa4]);
        }
        // A second executable virtual alias needs six hazard breakpoints.
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x1007u64.to_le_bytes());
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x1031]
        );
        // An indirect transfer must stop before it can select the alias.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x1001..0x1003].copy_from_slice(&[0xff, 0xe0]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(
            &batch.page_breakpoints[..batch.page_breakpoint_count],
            &[0x1001]
        );
        // A direct branch to the executable alias cannot use the NPT fetch
        // boundary, because both virtual addresses map to one physical page.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x1000..0x1005].copy_from_slice(&[0xe9, 0xfb, 0x7f, 0, 0]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(!prepare(&mut ctx, true, true, &window).is_some_and(|b| b.page_execution));
    }

    #[test]
    fn dirty_global_gate_prefers_reachable_proof_over_alias_enumeration() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1000..0x1003].copy_from_slice(&[0x90, 0xeb, 0x2e]);
        ctx.memory[0x1020..0x1022].copy_from_slice(&[0xf3, 0xa4]);
        ctx.memory[0x1031] = 0xc3;
        ctx.memory[0x1100..0x1102].copy_from_slice(&[0xf3, 0xa4]);
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x1007u64.to_le_bytes());
        ctx.state_mut().svm_guard.gate_ready = true;
        ctx.state_mut().svm_guard.gate_dirty = true;
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.page_execution);
        assert_eq!(&batch.page_breakpoints[..batch.page_breakpoint_count], &[0x1031]);
    }

    #[test]
    fn scalar_table_store_rearm_requires_decoded_store() {
        let ctx = paged_context(&[0x48, 0x89, 0x07]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert_eq!(scalar_table_store_next_rip(&ctx, &window), Some(0x1003));

        let ctx = paged_context(&[0xf3, 0xaa]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert_eq!(scalar_table_store_next_rip(&ctx, &window), None);
    }

    #[test]
    fn transient_resume_flag_does_not_blacklist_safe_page() {
        let mut ctx = paged_context(&[0x90]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        ctx.set_guest_rflags(2 | (1 << 16));
        assert!(prepare(&mut ctx, true, true, &window).is_none());
        assert!(!ctx.state().svm_rejected_pages.contains(&0x1000));
        ctx.set_guest_rflags(2);
        assert!(
            prepare(&mut ctx, true, true, &window)
                .unwrap()
                .page_execution
        );
    }

    #[test]
    fn reachable_region_cache_rechecks_outgoing_aliases() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1000..0x1005].copy_from_slice(&[0xe9, 0xfb, 0x7f, 0, 0]);
        for offset in [0x100, 0x200, 0x300] {
            ctx.memory[0x1000 + offset..0x1002 + offset].copy_from_slice(&[0xf3, 0xa4]);
        }
        ctx.memory[0x6050..0x6058].copy_from_slice(&0x1007u64.to_le_bytes());
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let first = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(first.page_execution);
        assert!(ctx.state().svm_guard.region_proofs.iter().any(|proof| {
            proof.valid
                && proof.linear == 0x1000
                && proof.outgoing[..proof.outgoing_count] == [0x9000]
        }));
        // Remap the outgoing address to this same physical code page. Its
        // cached proof must not bypass the new executable alias.
        ctx.state_mut().svm_guard.valid = false;
        ctx.memory[0x6048..0x6050].copy_from_slice(&0x1007u64.to_le_bytes());
        assert!(!prepare(&mut ctx, true, true, &window).is_some_and(|b| b.page_execution));
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
    fn rep_entries_prefer_bounded_chunks_without_rejecting_the_page() {
        let mut ctx = paged_context(&[0xf3, 0xaa]);
        ctx.state_mut().gprs.rdi = 0x7000;
        ctx.state_mut().gprs.rcx = 16;
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert!(batch.repeat.is_some() && batch.validated_stores && !batch.page_execution);
        assert!(!ctx.state().svm_rejected_pages.contains(&0x1000));
        for count in [0, 1] {
            ctx.state_mut().gprs.rcx = count;
            assert!(prepare(&mut ctx, true, true, &window).is_none());
            assert!(!ctx.state().svm_rejected_pages.contains(&0x1000));
        }
    }

    #[test]
    fn resumed_rep_can_batch_while_other_rf_instructions_stay_scalar() {
        let mut ctx = paged_context(&[0xf3, 0xaa]);
        ctx.memory[0x6040..0x6048].fill(0); // Next destination page is unmapped.
        ctx.state_mut().gprs.rdi = 0x7ff0;
        ctx.state_mut().gprs.rcx = 5000;
        ctx.set_guest_rflags(2 | (1 << 16));
        assert!(super::super::svm::physical(&ctx, 0x8000).is_err());
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare(&mut ctx, true, true, &window).unwrap();
        assert_eq!(batch.repeat.unwrap().iterations, 16);
        assert!(batch.validated_stores);

        ctx.memory[0x1000..0x1002].copy_from_slice(&[0x90, 0x90]);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        assert!(prepare(&mut ctx, true, true, &window).is_none());
    }

    #[test]
    fn native_rep_stores_revoke_cached_ram_proofs() {
        for destination in [0xb000, 0x7000, 0xa000] {
            let mut ctx = paged_context(&[0xf3, 0xaa]);
            ctx.state_mut().gprs.rcx = 16;
            ctx.state_mut().gprs.rdi = destination;
            for (address, entry) in [(0x3008, 0x8007u64), (0x8000, 0x9007), (0x9000, 0xa007)] {
                ctx.memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
            }
            let batch = planned(&ctx).unwrap();
            collect_translation_tree(&mut ctx, &batch).unwrap();
            assert!(cached_page_hazards(&mut ctx, 0x7000).is_some());
            assert!(batch.repeat.is_some() && batch.writes_memory && batch.validated_stores);
            let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
            retain_translation_cache(&mut ctx, Some(&batch), Some(&window), false);
            assert!(!ctx.state().svm_guard.valid);
            assert_eq!(ctx.state().svm_guard.code_count, 0);
        }
    }

    #[test]
    fn register_only_intercepts_keep_ram_proofs() {
        let mut ctx = paged_context(&[0x90]);
        let mut window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        prepare(&mut ctx, true, true, &window).unwrap();
        let code_count = ctx.state().svm_guard.code_count;
        for opcode in [&[0x0f, 0x31][..], &[0x0f, 0x01, 0xf9], &[0x0f, 0xa2]] {
            window.bytes[..opcode.len()].copy_from_slice(opcode);
            retain_translation_cache(&mut ctx, None, Some(&window), false);
            assert!(ctx.state().svm_guard.valid);
            assert_eq!(ctx.state().svm_guard.code_count, code_count);
        }
        // Control-register writes may change the page-table interpretation.
        window.bytes[..3].copy_from_slice(&[0x0f, 0x22, 0xd8]);
        retain_translation_cache(&mut ctx, None, Some(&window), false);
        assert!(!ctx.state().svm_guard.valid);
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
                // CALL writes a return address, but this stack slot is
                // disjoint from the guarded table and code pages.
                window.bytes[..5].copy_from_slice(&[0xe8, 0, 0, 0, 0]);
                retain_translation_cache(&mut ctx, None, Some(&window), false);
                assert!(ctx.state().svm_guard.valid);
                assert_eq!(ctx.state().svm_guard.code_count, code_count);
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
        for opcode in [&[0x50][..], &[0xe8, 0, 0, 0, 0][..]] {
            for (rsp, keep) in [(0x8008, true), (0x6008, false)] {
                let mut ctx = paged_context(opcode);
                let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
                prepare(&mut ctx, true, true, &window).unwrap();
                ctx.vmcs_setup()
                    .set_field_natural(VmcsFieldNatural::GuestRsp, rsp);
                retain_translation_cache(&mut ctx, None, Some(&window), false);
                assert_eq!(
                    ctx.state().svm_guard.valid,
                    keep,
                    "opcode={opcode:02x?} rsp={rsp:#x}"
                );
            }
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
    fn hazard_memos_keep_a_larger_working_set_and_recover_from_stale_hints() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory.resize(0x800000, 0x90);
        for page in 1..=96u64 {
            assert_eq!(page_hazards_memo(&mut ctx, page * 4096).unwrap().count, 0);
        }
        let (key, Some(index)) = hazard_memo_lookup(&ctx, 0x1000) else {
            panic!("first page was evicted");
        };
        let revision = ctx.state().svm_guard.hazard_memos[index].revision;
        // An index hint can be replaced by a colliding page, but the full
        // lookup must still find the original memo and restore the hint.
        ctx.state_mut().svm_guard.hazard_lookup[key] = 96;
        assert_eq!(page_hazards_memo(&mut ctx, 0x1000).unwrap().count, 0);
        assert_eq!(ctx.state().svm_guard.hazard_memos[index].revision, revision);
        assert_eq!(ctx.state().svm_guard.hazard_lookup[key], (index + 1) as u16);
    }

    #[test]
    fn recurrent_code_guard_survives_reads_and_revokes_on_writes() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator, bedrock_ept::PageTableFormat::AmdNpt,
        ).unwrap();
        ctx.state_mut().ept.map_4k(
            &mut allocator, GuestPhysAddr::new(0x1000), HostPhysAddr::new(0x101000),
            bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
            bedrock_ept::EptMemoryType::WriteBack,
        ).unwrap();
        for _ in 0..17 {
            assert!(!globally_safe_code(&mut ctx, 0x1000));
        }
        protect_recurrent_code(&mut ctx, &allocator, 0x1000);
        assert!(ctx.state().svm_guard.hazard_memos[0].guarded);
        assert_eq!(ctx.state().ept.lookup(&allocator, GuestPhysAddr::new(0x1000))
            .unwrap().1.bits() & 2, 0);
        let revision = ctx.state().svm_guard.hazard_memos[0].revision;
        assert_eq!(page_hazards_memo(&mut ctx, 0x1000).unwrap().count, 1);
        assert_eq!(ctx.state().svm_guard.hazard_memos[0].revision, revision);

        ctx.state_mut().svm_guard.note_gate_host_write(0x1100, 3);
        ctx.memory[0x1100..0x1103].fill(0x90);
        assert!(!ctx.state().svm_guard.hazard_memos[0].valid);
        assert_eq!(page_hazards_memo(&mut ctx, 0x1000).unwrap().count, 0);
        assert!(ctx.state().svm_guard.hazard_memos[0].revision > revision);
        protect_recurrent_code(&mut ctx, &allocator, 0x1000);
        assert!(!ctx.state().svm_guard.hazard_memos[0].guarded);
        assert_ne!(ctx.state().ept.lookup(&allocator, GuestPhysAddr::new(0x1000))
            .unwrap().1.bits() & 2, 0);

        ctx.state_mut().svm_guard.note_gate_host_write(0x1100, 3);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        for _ in 0..17 {
            assert!(!globally_safe_code(&mut ctx, 0x1000));
        }
        protect_recurrent_code(&mut ctx, &allocator, 0x1000);
        assert!(ctx.state().svm_guard.hazard_memos[0].guarded);
        assert!(release_recurrent_code_write(&mut ctx, &allocator, 0x1000));
        assert!(!ctx.state().svm_guard.hazard_memos[0].guarded);
        assert!(!ctx.state().svm_guard.hazard_memos[0].valid);
        assert_ne!(ctx.state().ept.lookup(&allocator, GuestPhysAddr::new(0x1000))
            .unwrap().1.bits() & 2, 0);
    }

    #[test]
    fn recurrent_code_guard_does_not_trust_a_replaced_mapping() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        ctx.memory[0x1100..0x1103].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let mut allocator = crate::test_mocks::MockFrameAllocator::new();
        ctx.state_mut().ept = bedrock_ept::EptPageTable::new_with_format(
            &mut allocator, bedrock_ept::PageTableFormat::AmdNpt,
        ).unwrap();
        ctx.state_mut().ept.map_4k(
            &mut allocator, GuestPhysAddr::new(0x1000), HostPhysAddr::new(0x101000),
            bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
            bedrock_ept::EptMemoryType::WriteBack,
        ).unwrap();
        for _ in 0..17 {
            assert!(!globally_safe_code(&mut ctx, 0x1000));
        }
        protect_recurrent_code(&mut ctx, &allocator, 0x1000);
        let revision = ctx.state().svm_guard.hazard_memos[0].revision;
        ctx.state_mut().ept.remap_4k(
            &allocator, GuestPhysAddr::new(0x1000), HostPhysAddr::new(0x201000),
            bedrock_ept::EptPermissions::READ_WRITE_EXECUTE,
            bedrock_ept::EptMemoryType::WriteBack,
        ).unwrap();
        ctx.memory[0x1100..0x1103].fill(0x90);
        assert_eq!(page_hazards_memo(&mut ctx, 0x1000).unwrap().count, 0);
        assert!(ctx.state().svm_guard.hazard_memos[0].revision > revision);
        protect_recurrent_code(&mut ctx, &allocator, 0x1000);
        assert!(!ctx.state().svm_guard.hazard_memos[0].guarded);
        assert_ne!(ctx.state().ept.lookup(&allocator, GuestPhysAddr::new(0x1000))
            .unwrap().1.bits() & 2, 0);
    }

    #[test]
    fn rejected_hazard_memo_rechecks_code_before_trusting_it() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x2000].fill(0x90);
        for offset in [0x100, 0x200, 0x300, 0x400, 0x500] {
            ctx.memory[0x1000 + offset..0x1003 + offset]
                .copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        }
        assert!(!globally_safe_code(&mut ctx, 0x1000));
        let revision = ctx.state().svm_guard.hazard_memos[0].revision;
        assert!(ctx.state().svm_guard.hazard_memos[0].rejected);
        assert!(!globally_safe_code(&mut ctx, 0x1000));
        assert!(cached_page_hazards(&mut ctx, 0x1000).is_none());
        assert_eq!(ctx.state().svm_guard.hazard_memos[0].revision, revision);

        ctx.memory[0x1000..0x2000].fill(0x90);
        assert!(globally_safe_code(&mut ctx, 0x1000));
        assert!(!ctx.state().svm_guard.hazard_memos[0].rejected);
        assert_eq!(ctx.state().svm_guard.hazard_memos[0].revision, revision + 1);
    }

    #[test]
    fn hazard_revisions_are_unique_across_memo_slots() {
        let mut ctx = paged_context(&[0x90]);
        ctx.memory[0x1000..0x3000].fill(0x90);
        assert!(page_hazards_memo(&mut ctx, 0x1000).is_some());
        assert!(page_hazards_memo(&mut ctx, 0x2000).is_some());
        let memos = &ctx.state().svm_guard.hazard_memos;
        assert_ne!(memos[0].revision, memos[1].revision);
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
                let hazards = page_hazards(&mut ctx, 0x1000, None).unwrap().unwrap();
                assert_eq!(hazards.count, expected.len());
                for &prefix in expected {
                    assert!(hazards.offsets[..hazards.count].contains(&((offset + prefix) as u16)));
                }
            }
        }
    }

    #[test]
    fn forward_store_paths_require_addresses_stable_on_every_path() {
        // Without table guards, stop before a branch following a store.
        let mut ctx = paged_context(&[
            0x48, 0x89, 0x07, 0x74, 4, 0x48, 0x89, 0x47, 8, 0x90, 0x0f, 0x01, 0xd9,
        ]);
        ctx.state_mut().stop_at_tsc = Some(4);
        assert!(planned(&ctx).is_none());
        // With table guards, both forward paths remain in the batch.
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let b = prepare_verified(&ctx, true, false, true, &window).unwrap();
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
        // A smaller deadline stops on the branch's exact exit breakpoint.
        ctx.state_mut().stop_at_tsc = Some(2);
        let b = planned(&ctx).unwrap();
        assert!(!b.uses_counter && b.branch_exit_count == 1);
        assert!(b.count <= 2);
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
    fn outgoing_branch_uses_exact_breakpoints_inside_deadline_margin() {
        let mut ctx = paged_context(&[0x90, 0x75, 0x20, 0x90, 0x90, 0x90]);
        ctx.state_mut().stop_at_tsc = Some(4);
        let window = super::super::svm::InstructionWindow::read(&ctx).unwrap();
        let batch = prepare_verified(&ctx, true, false, true, &window).unwrap();
        assert!(!batch.uses_counter);
        assert_eq!(batch.count, 4);
        assert_eq!(&batch.branch_exits[..batch.branch_exit_count], &[0x1023]);
        let mut backward = paged_context(&[0x90, 0x75, 0xfc]);
        backward.state_mut().stop_at_tsc = Some(4);
        let window = super::super::svm::InstructionWindow::read(&backward).unwrap();
        assert!(prepare_verified(&backward, true, false, true, &window).is_none());
    }

    #[test]
    fn relative_branch_lengths_and_targets_match_independent_decoder() {
        use iced_x86::{Decoder, DecoderOptions, FlowControl};
        for bytes in [
            &[0x2e, 0xe9, 0xfa, 0x00, 0x00, 0x00][..],
            &[0x2e, 0xeb, 0xfe],
            &[0x2e, 0x75, 0xfc],
        ] {
            let (length, relative) = relative_branch(bytes, true, false).unwrap();
            let instruction = Decoder::new(64, bytes, DecoderOptions::NONE).decode();
            assert_eq!(length, instruction.len());
            assert_eq!(
                (length as i64 + relative) as u64,
                instruction.near_branch_target()
            );
        }
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
            pages: [0; 68],
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
            global_execution: false,
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
            pages: [0; 68],
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
            global_execution: false,
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
