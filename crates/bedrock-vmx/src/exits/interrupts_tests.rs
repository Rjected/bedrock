// SPDX-License-Identifier: GPL-2.0

//! Tests for deterministic instruction-granular preemption (`check_preempt`)
//! and the `ApicState` preemption helpers.

use super::*;
use crate::tests::MockVmContext;

/// Software-enable the APIC and give the LVT timer an unmasked, deliverable
/// one-shot vector.
fn enable_timer_vector(ctx: &mut MockVmContext, vector: u8) {
    let apic = &mut ctx.state_mut().devices.apic;
    apic.svr |= 1 << 8;
    apic.lvt_timer = u32::from(vector);
}

fn irr_set(ctx: &MockVmContext, vector: u8) -> bool {
    ctx.state().devices.apic.irr[(vector / 32) as usize] & (1 << (vector % 32)) != 0
}

#[test]
fn preempt_interval_is_deterministic_and_in_range() {
    let mut a = ApicState::default();
    let mut b = ApicState::default();
    a.configure_preempt(1000, 0x1234_5678);
    b.configure_preempt(1000, 0x1234_5678);
    let mut c = ApicState::default();
    c.configure_preempt(1000, 0x1234_5679);

    let mut differs = false;
    for _ in 0..64 {
        let ia = a.next_preempt_interval();
        assert_eq!(ia, b.next_preempt_interval(), "same seed, same stream");
        assert!((1000..2000).contains(&ia), "{ia} outside [P, 2P)");
        differs |= ia != c.next_preempt_interval();
    }
    assert!(differs, "different seeds should give different streams");
}

#[test]
fn configure_preempt_zero_seed_and_disable() {
    let mut a = ApicState::default();
    a.configure_preempt(1000, 0);
    assert_ne!(a.preempt_rng, 0, "0 is a xorshift fixed point");
    assert!(a.next_preempt_interval() >= 1000);

    a.preempt_deadline = 5;
    a.configure_preempt(0, 42);
    assert_eq!(a.preempt_period, 0);
    assert_eq!(a.preempt_deadline, 0);
    assert_eq!(a.next_preempt_interval(), 0);
}

#[test]
fn check_preempt_noop_when_disabled() {
    let mut ctx = MockVmContext::new();
    enable_timer_vector(&mut ctx, 0x40);
    ctx.set_emulated_tsc(1_000_000);
    check_preempt(&mut ctx);
    assert_eq!(ctx.state().devices.apic.preempt_deadline, 0);
    assert!(!irr_set(&ctx, 0x40));
}

#[test]
fn check_preempt_lazy_arms_then_fires_and_reschedules() {
    let mut ctx = MockVmContext::new();
    enable_timer_vector(&mut ctx, 0x40);
    ctx.state_mut()
        .devices
        .apic
        .configure_preempt(1000, 0x9e37_79b9);

    ctx.set_emulated_tsc(10_000);
    check_preempt(&mut ctx);
    let first = ctx.state().devices.apic.preempt_deadline;
    assert!((11_000..12_000).contains(&first));
    assert!(!irr_set(&ctx, 0x40), "arming pass injects nothing");

    ctx.set_emulated_tsc(first - 1);
    check_preempt(&mut ctx);
    assert!(!irr_set(&ctx, 0x40));
    assert_eq!(ctx.state().devices.apic.preempt_deadline, first);

    ctx.set_emulated_tsc(first);
    check_preempt(&mut ctx);
    assert!(irr_set(&ctx, 0x40), "raises the LVT timer vector");
    let second = ctx.state().devices.apic.preempt_deadline;
    assert!((first + 1000..first + 2000).contains(&second));
}

#[test]
fn check_preempt_holds_off_until_vector_usable() {
    let mut ctx = MockVmContext::new();
    ctx.state_mut().devices.apic.configure_preempt(1000, 1);
    ctx.set_emulated_tsc(5000);
    check_preempt(&mut ctx);
    assert_eq!(ctx.state().devices.apic.preempt_deadline, 0);

    enable_timer_vector(&mut ctx, 0x40);
    check_preempt(&mut ctx);
    assert_ne!(ctx.state().devices.apic.preempt_deadline, 0);
}

#[test]
fn preempt_state_is_inherited_by_clone_and_independent() {
    // Forks deep-copy `DeviceStates`; reconfiguring a copy must leave the
    // original untouched.
    let mut parent = ApicState::default();
    parent.configure_preempt(1000, 7);
    parent.preempt_deadline = 1234;
    let mut child = parent.clone();
    assert_eq!(child.preempt_deadline, 1234);
    assert_eq!(child.preempt_rng, parent.preempt_rng);
    child.configure_preempt(50, 9);
    assert_eq!(parent.preempt_period, 1000);
    assert_eq!(parent.preempt_deadline, 1234);
}
