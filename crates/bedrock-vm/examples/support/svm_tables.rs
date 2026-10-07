// SPDX-License-Identifier: GPL-2.0
use std::collections::{HashSet, VecDeque};

pub fn page_table_count(memory: &[u8], cr3: u64) -> Option<usize> {
    let root = cr3 & 0x000f_ffff_ffff_f000;
    let mut seen = HashSet::from([root]);
    let mut queue = VecDeque::from([(root, 4)]);
    while let Some((table, level)) = queue.pop_front() {
        let table = usize::try_from(table).ok()?;
        let bytes = memory.get(table..table.checked_add(4096)?)?;
        if level == 1 {
            continue;
        }
        for entry in bytes.chunks_exact(8) {
            let entry = u64::from_le_bytes(entry.try_into().ok()?);
            if entry & 1 == 0 || entry & (1 << 7) != 0 {
                continue;
            }
            let child = entry & 0x000f_ffff_ffff_f000;
            if seen.insert(child) {
                queue.push_back((child, level - 1));
            }
        }
    }
    Some(seen.len())
}
