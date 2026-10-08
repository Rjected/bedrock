//! Per-branch deterministic preemption (`Branch::set_preempt`): forced
//! preemptions raise the guest's local timer vector, so they show up as extra
//! `LOC` interrupts, replay identically for equal `(period, seed)`, and stay
//! scoped to the branch that enabled them.

use bedrock_lab::BashTarget;

use crate::common;

/// Busy-loop in the guest shell and report how many local timer interrupts
/// (`LOC` in `/proc/interrupts`, summed over CPUs) arrived meanwhile.
const LOC_DELTA: &str = "loc() { awk '/LOC:/{s=0; for(i=2;i<=NF;i++) if ($i ~ /^[0-9]+$/) s+=$i; print s}' /proc/interrupts; }; \
     a=$(loc); i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done; b=$(loc); echo $((b-a))";

/// Fork a branch, optionally enable preemption, and return the LOC delta.
fn loc_delta(ready: &bedrock_lab::Checkpoint, preempt: Option<(u64, u64)>) -> u64 {
    let mut branch = ready.branch().expect("fork branch");
    if let Some((period, seed)) = preempt {
        branch.set_preempt(period, seed).expect("set_preempt");
    }
    let out = branch
        .bash(BashTarget::host(), LOC_DELTA, true)
        .expect("bash");
    assert!(out.success(), "loop failed: exit={}", out.exit_code);
    let text = String::from_utf8_lossy(&out.output);
    text.trim()
        .parse()
        .unwrap_or_else(|_| panic!("unexpected LOC output: {text:?}"))
}

#[test]
fn preempted_branches_replay_by_seed_and_stay_scoped() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("preempted_branches_replay_by_seed_and_stay_scoped");
    };

    let off_a = loc_delta(&ready, None);
    let on_a = loc_delta(&ready, Some((20_000, 1)));
    let on_b = loc_delta(&ready, Some((20_000, 1)));
    // Fork again after the preempted siblings: the checkpoint must be untouched.
    let off_b = loc_delta(&ready, None);
    let disabled = loc_delta(&ready, Some((0, 1)));

    assert_eq!(on_a, on_b, "equal (period, seed) should replay identically");
    assert_eq!(
        off_a, off_b,
        "preempting a branch must not leak into the checkpoint or siblings"
    );
    assert_eq!(off_a, disabled, "period 0 should disable preemption");
    assert!(
        on_a > off_a,
        "forced preemptions should add local timer interrupts: on={on_a} off={off_a}"
    );
}
