// SPDX-License-Identifier: GPL-2.0

//! Tests for IPI delivery through the emulated ICR.

use super::*;

const ICR: u32 = 0x300;
const ICR_HI: u32 = 0x310;

fn irr_set(apic: &ApicState, vector: u8) -> bool {
    apic.irr[(vector / 32) as usize] & (1 << (vector % 32)) != 0
}

fn send(apic: &mut ApicState, hi: u32, lo: u32) {
    write_apic_register(apic, ICR_HI, hi, 0);
    write_apic_register(apic, ICR, lo, 0);
}

#[test]
fn self_shorthand_queues_vector() {
    // Linux's irq_work self-IPI: shorthand self, fixed, vector 0xF6.
    let mut apic = ApicState::default();
    send(&mut apic, 0, (0b01 << 18) | 0xF6);
    assert!(irr_set(&apic, 0xF6));
}

#[test]
fn physical_destination_matching_own_id_queues_vector() {
    let mut apic = ApicState::default();
    send(&mut apic, 0, 0xFD);
    assert!(irr_set(&apic, 0xFD));

    let mut other = ApicState::default();
    send(&mut other, 1 << 24, 0xFD);
    assert!(
        !irr_set(&other, 0xFD),
        "IPI to APIC 1 must not reach APIC 0"
    );
}

#[test]
fn logical_destination_uses_ldr() {
    let mut apic = ApicState {
        ldr: 1 << 24,
        ..Default::default()
    };
    send(&mut apic, 1 << 24, (1 << 11) | 0xFB);
    assert!(irr_set(&apic, 0xFB));

    let mut miss = ApicState {
        ldr: 1 << 24,
        ..Default::default()
    };
    send(&mut miss, 2 << 24, (1 << 11) | 0xFB);
    assert!(!irr_set(&miss, 0xFB));
}

#[test]
fn all_including_self_queues_and_excluding_self_does_not() {
    let mut apic = ApicState::default();
    send(&mut apic, 0, (0b10 << 18) | 0xFC);
    assert!(irr_set(&apic, 0xFC));

    let mut apic = ApicState::default();
    send(&mut apic, 0, (0b11 << 18) | 0xFC);
    assert!(!irr_set(&apic, 0xFC));
}

#[test]
fn non_fixed_modes_and_illegal_vectors_are_dropped() {
    // NMI (100) and INIT (101) to self.
    for mode in [0b100u32, 0b101] {
        let mut apic = ApicState::default();
        send(&mut apic, 0, (0b01 << 18) | (mode << 8) | 0xF0);
        assert_eq!(apic.irr, [0; 8], "mode {mode:#b}");
    }
    let mut apic = ApicState::default();
    send(&mut apic, 0, (0b01 << 18) | 0x05);
    assert_eq!(apic.irr, [0; 8]);
    assert_ne!(apic.esr & (1 << 5), 0, "illegal vector sets ESR");
}

#[test]
fn icr_reads_back_idle() {
    let mut apic = ApicState::default();
    send(&mut apic, 0, ICR_DELIVERY_PENDING | (0b01 << 18) | 0xF6);
    assert_eq!(read_apic_register(&apic, ICR) & ICR_DELIVERY_PENDING, 0);
    assert_eq!(read_apic_register(&apic, ICR) & 0xFF, 0xF6);
}
