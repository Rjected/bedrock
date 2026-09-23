// SPDX-License-Identifier: GPL-2.0

extern crate alloc;
use super::*;
use alloc::vec::Vec;

#[test]
fn test_vpid_allocation() {
    reset_vpid_counter();

    let vpid1 = allocate_vpid();
    assert_ne!(vpid1, 0, "VPID should never be 0");

    let vpid2 = allocate_vpid();
    assert_ne!(vpid2, 0);
    assert_ne!(vpid2, vpid1, "Each allocation should return a unique VPID");

    let vpid3 = allocate_vpid();
    assert_ne!(vpid3, 0);
    assert_ne!(vpid3, vpid1);
    assert_ne!(vpid3, vpid2);

    deallocate_vpid(vpid1);
    deallocate_vpid(vpid2);
    deallocate_vpid(vpid3);
}

#[test]
fn test_vpid_never_zero() {
    reset_vpid_counter();

    let mut vpids = Vec::new();
    for _ in 0..1000 {
        let vpid = allocate_vpid();
        assert_ne!(vpid, 0, "VPID should never be 0");
        vpids.push(vpid);
    }

    for vpid in vpids {
        deallocate_vpid(vpid);
    }
}

#[test]
fn test_vpid_recycling() {
    reset_vpid_counter();

    let vpid1 = allocate_vpid();
    assert_ne!(vpid1, 0);

    deallocate_vpid(vpid1);

    // Not necessarily the same VPID back, due to the search hint.
    let vpid2 = allocate_vpid();
    assert_ne!(vpid2, 0);

    deallocate_vpid(vpid2);
}

#[test]
fn test_vpid_high_churn() {
    reset_vpid_counter();

    // Recycling means churn never exhausts VPIDs.
    for _ in 0..10000 {
        let vpid = allocate_vpid();
        assert_ne!(vpid, 0);
        deallocate_vpid(vpid);
    }

    let vpid = allocate_vpid();
    assert_ne!(vpid, 0);
    deallocate_vpid(vpid);
}

#[test]
fn test_vpid_count() {
    // Tests share global VPID state in parallel, so only sanity-check counts.

    let vpid = allocate_vpid();
    let count = count_allocated_vpids();

    assert!(count >= 1, "Should have at least 1 VPID marked in use");

    deallocate_vpid(vpid);
}
