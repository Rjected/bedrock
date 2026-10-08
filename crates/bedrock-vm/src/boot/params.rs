// SPDX-License-Identifier: GPL-2.0

//! Linux boot_params (zero page) setup, per Documentation/arch/x86/boot.rst and
//! arch/x86/include/uapi/asm/bootparam.h.

use super::constants::boot_params_offsets as offsets;
use super::constants::boot_protocol::{self, loadflags};
use super::constants::e820;
use super::constants::memory::{BOOT_PARAMS_ADDR, CMDLINE_ADDR, PAGE_SIZE};

/// E820 memory map entry (matches Linux struct e820_entry).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub type_: u32,
}

/// Set up boot_params (zero page) at BOOT_PARAMS_ADDR.
pub fn setup_boot_params(
    memory: &mut [u8],
    memory_size: usize,
    cmdline: &str,
    initramfs_addr: Option<u64>,
    initramfs_size: Option<usize>,
) {
    let boot_params = &mut memory[BOOT_PARAMS_ADDR as usize..][..PAGE_SIZE];
    boot_params.fill(0);

    // Setup header
    write_u8(boot_params, offsets::SETUP_SECTS, 0);
    write_u16(boot_params, offsets::BOOT_FLAG, boot_protocol::BOOT_FLAG);
    write_u32(boot_params, offsets::HEADER_MAGIC, boot_protocol::HDR_MAGIC);
    write_u16(
        boot_params,
        offsets::PROTOCOL_VERSION,
        boot_protocol::VERSION_2_15,
    );
    write_u8(
        boot_params,
        offsets::TYPE_OF_LOADER,
        boot_protocol::LOADER_TYPE_UNDEFINED,
    );
    write_u8(
        boot_params,
        offsets::LOADFLAGS,
        loadflags::LOADED_HIGH | loadflags::CAN_USE_HEAP,
    );

    // Ramdisk (initramfs)
    if let Some(addr) = initramfs_addr {
        write_u32(boot_params, offsets::RAMDISK_IMAGE, addr as u32);
    }
    if let Some(size) = initramfs_size {
        write_u32(boot_params, offsets::RAMDISK_SIZE, size as u32);
    }

    // Heap and command line
    write_u16(boot_params, offsets::HEAP_END_PTR, 0xFE00);
    write_u32(boot_params, offsets::CMD_LINE_PTR, CMDLINE_ADDR as u32);
    write_u32(boot_params, offsets::CMDLINE_SIZE, cmdline.len() as u32);

    setup_e820_table(boot_params, memory_size);
}

/// Base of the 32-bit MMIO hole (IOAPIC at 0xFEC00000, LAPIC at 0xFEE00000)
/// through 4 GiB. Guest memory is identity-mapped (GPA = offset), but
/// `RootVm::new` leaves the APIC pages out of the EPT so accesses fault into
/// the APIC emulation; the guest must never use them as RAM.
const MMIO_HOLE_START: u64 = 0xFEC0_0000;
const MMIO_HOLE_END: u64 = 0x1_0000_0000;

/// The guest's E820 map: low memory, the legacy hole, then RAM from 1 MiB to
/// `memory_size` with the 32-bit MMIO hole carved out as reserved.
///
/// Without the carve-out, a guest of exactly 4 GiB has its top of RAM, where
/// memblock allocates top-down during early boot, on the unmapped APIC pages,
/// and the first such access fails the run with EIO before the console comes
/// up; larger guests merely hand those pages out later.
pub fn e820_entries(memory_size: usize) -> Vec<E820Entry> {
    let mem = memory_size as u64;
    let mut entries = vec![
        // Low memory (0 - 0x9FC00) - conventional memory
        E820Entry {
            addr: 0,
            size: 0x9FC00,
            type_: e820::RAM,
        },
        // Reserved (0x9FC00 - 0xA0000) - EBDA
        E820Entry {
            addr: 0x9FC00,
            size: 0x400,
            type_: e820::RESERVED,
        },
        // Reserved (0xA0000 - 0x100000) - video memory + ROM
        E820Entry {
            addr: 0xA0000,
            size: 0x60000,
            type_: e820::RESERVED,
        },
        // Main RAM below the MMIO hole
        E820Entry {
            addr: 0x100000,
            size: mem.min(MMIO_HOLE_START) - 0x100000,
            type_: e820::RAM,
        },
    ];
    if mem > MMIO_HOLE_START {
        entries.push(E820Entry {
            addr: MMIO_HOLE_START,
            size: MMIO_HOLE_END - MMIO_HOLE_START,
            type_: e820::RESERVED,
        });
    }
    if mem > MMIO_HOLE_END {
        entries.push(E820Entry {
            addr: MMIO_HOLE_END,
            size: mem - MMIO_HOLE_END,
            type_: e820::RAM,
        });
    }
    entries
}

fn setup_e820_table(boot_params: &mut [u8], memory_size: usize) {
    let entries = e820_entries(memory_size);

    write_u8(boot_params, offsets::E820_ENTRIES, entries.len() as u8);

    for (i, entry) in entries.iter().enumerate() {
        let offset = offsets::E820_TABLE + i * offsets::E820_ENTRY_SIZE;
        boot_params[offset..][..8].copy_from_slice(&{ entry.addr }.to_le_bytes());
        boot_params[offset + 8..][..8].copy_from_slice(&{ entry.size }.to_le_bytes());
        boot_params[offset + 16..][..4].copy_from_slice(&{ entry.type_ }.to_le_bytes());
    }
}

pub fn write_cmdline(memory: &mut [u8], cmdline: &str) {
    let cmdline_bytes = cmdline.as_bytes();
    let dest = &mut memory[CMDLINE_ADDR as usize..][..cmdline_bytes.len() + 1];
    dest[..cmdline_bytes.len()].copy_from_slice(cmdline_bytes);
    dest[cmdline_bytes.len()] = 0; // null terminator
}

fn write_u8(buf: &mut [u8], offset: usize, val: u8) {
    buf[offset] = val;
}

fn write_u16(buf: &mut [u8], offset: usize, val: u16) {
    buf[offset..][..2].copy_from_slice(&val.to_le_bytes());
}

fn write_u32(buf: &mut [u8], offset: usize, val: u32) {
    buf[offset..][..4].copy_from_slice(&val.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram(memory_size: usize) -> Vec<(u64, u64)> {
        e820_entries(memory_size)
            .into_iter()
            .filter(|e| e.type_ == e820::RAM)
            .map(|e| (e.addr, e.addr + e.size))
            .collect()
    }

    const MB: usize = 1 << 20;

    #[test]
    fn e820_below_hole_is_unchanged() {
        assert_eq!(ram(3072 * MB), [(0, 0x9FC00), (0x100000, 0xC000_0000)]);
    }

    #[test]
    fn e820_never_reports_apic_pages_as_ram() {
        for mb in [4064, 4096, 6144, 16384] {
            for (start, end) in ram(mb * MB) {
                for apic in [0xFEC0_0000u64, 0xFEE0_0000] {
                    assert!(!(start..end).contains(&apic), "{mb} MB: {apic:#x} in RAM");
                }
                assert!(end <= (mb * MB) as u64);
            }
        }
        assert_eq!(
            ram(6144 * MB),
            [
                (0, 0x9FC00),
                (0x100000, 0xFEC0_0000),
                (0x1_0000_0000, 0x1_8000_0000)
            ]
        );
        assert_eq!(ram(4096 * MB).last(), Some(&(0x100000, 0xFEC0_0000)));
    }

    #[test]
    fn e820_table_is_written() {
        let mut bp = vec![0u8; PAGE_SIZE];
        setup_e820_table(&mut bp, 6144 * MB);
        assert_eq!(bp[offsets::E820_ENTRIES], 6);
        let last = offsets::E820_TABLE + 5 * offsets::E820_ENTRY_SIZE;
        assert_eq!(&bp[last..][..8], &0x1_0000_0000u64.to_le_bytes());
    }
}
