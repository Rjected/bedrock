//! Feedback buffers: a guest process registers a buffer and the lab reads it
//! back, including from CoW-forked children.

use bedrock_lab::BashTarget;

use crate::common;

/// The driver inside the integration-tests container, the id and size it
/// registers, and the payload this test asks it to write. Kept in sync with
/// `workloads/integration-tests/ready/test_feedback_buffers.c`.
const DRIVER: &str = "/opt/bedrock/drivers/test_feedback_buffers";
const FB_ID: &[u8] = b"bedrock-test-fb";
const FB_SIZE: usize = 4096;
const PAYLOAD: &str = "bedrock-feedback-roundtrip";

/// The "counter" driver and its id: it registers a buffer and then bumps a
/// 64-bit little-endian counter at the front forever. Kept in sync with
/// `workloads/integration-tests/ready/test_feedback_buffer_counter.c`.
const COUNTER_DRIVER: &str = "/opt/bedrock/drivers/test_feedback_buffer_counter";
const COUNTER_FB_ID: &[u8] = b"bedrock-test-fb-counter";

#[test]
fn feedback_buffer_round_trips_from_guest() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("feedback_buffer_round_trips_from_guest");
    };

    let mut branch = ready.branch().expect("fork branch");

    let probe = branch
        .bash(
            BashTarget::container("idle"),
            &format!("test -x {DRIVER}"),
            false,
        )
        .expect("probe driver");
    assert!(
        probe.success(),
        "expected the driver at {DRIVER} (rebuild the integration-tests workload \
         image): exit={}",
        probe.exit_code,
    );

    // Detached and kept alive so its pages stay pinned (otherwise the GPA could
    // be reused); stdio redirected so it doesn't hold the output pipe open.
    branch
        .bash(
            BashTarget::container("idle"),
            &format!("{DRIVER} {PAYLOAD} >/dev/null 2>&1 &"),
            false,
        )
        .expect("launch feedback-buffer driver");

    // Deterministic, so this lands at the same point every run; the budget is
    // a safety net.
    let deadline = branch.current_time() + vt_dur!(5 s);
    while !registered(&branch) {
        assert!(
            branch.current_time() < deadline,
            "driver never registered a feedback buffer under {:?}",
            String::from_utf8_lossy(FB_ID),
        );
        branch.run_for(vt_dur!(50 ms)).expect("advance guest");
    }

    // The payload must survive GVA->GPA->host mapping; the rest stays zeroed.
    let bufs = branch
        .feedback_buffers_to_vec(FB_ID)
        .expect("read feedback buffers");
    assert_eq!(bufs.len(), 1, "expected exactly one buffer under the id");
    assert!(
        bufs[0].len() >= FB_SIZE,
        "mapped buffer shorter than registered size: {} < {FB_SIZE}",
        bufs[0].len(),
    );

    let mut expected = vec![0u8; FB_SIZE];
    expected[..PAYLOAD.len()].copy_from_slice(PAYLOAD.as_bytes());
    assert_eq!(
        &bufs[0][..FB_SIZE],
        expected.as_slice(),
        "feedback buffer content did not match what the guest wrote",
    );

    // A fork sees the same bytes without running: its mapping resolves through
    // the CoW chain to the snapshot's frames.
    let checkpoint = branch.checkpoint().expect("checkpoint parent branch");
    let mut child = checkpoint.branch().expect("branch off checkpoint");
    let child_bufs = child
        .feedback_buffers_to_vec(FB_ID)
        .expect("read child feedback buffers");
    assert_eq!(
        child_bufs.len(),
        1,
        "child should inherit exactly one buffer under the id",
    );
    assert_eq!(
        child_bufs[0], bufs[0],
        "forked branch's feedback buffer diverged from its parent's",
    );
}

/// The number of feedback buffers is unbounded: many driver instances each get
/// their own readable buffer.
#[test]
fn feedback_buffer_count_is_unbounded() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("feedback_buffer_count_is_unbounded");
    };

    let mut branch = ready.branch().expect("fork branch");

    let probe = branch
        .bash(
            BashTarget::container("idle"),
            &format!("test -x {DRIVER}"),
            false,
        )
        .expect("probe driver");
    assert!(
        probe.success(),
        "expected the driver at {DRIVER} (rebuild the integration-tests workload \
         image): exit={}",
        probe.exit_code,
    );

    // All under the shared id (each registration gets a fresh slot); each
    // stays alive (`&`) to keep its page pinned.
    const COUNT: usize = 20;
    branch
        .bash(
            BashTarget::container("idle"),
            &format!("for i in $(seq 1 {COUNT}); do {DRIVER} {PAYLOAD} >/dev/null 2>&1 & done"),
            false,
        )
        .expect("launch feedback-buffer drivers");

    let deadline = branch.current_time() + vt_dur!(10 s);
    loop {
        let n = branch
            .feedback_buffers_to_vec(FB_ID)
            .expect("read feedback buffers")
            .len();
        if n >= COUNT {
            break;
        }
        assert!(
            branch.current_time() < deadline,
            "only {n}/{COUNT} feedback buffers registered before the deadline",
        );
        branch.run_for(vt_dur!(50 ms)).expect("advance guest");
    }

    let bufs = branch
        .feedback_buffers_to_vec(FB_ID)
        .expect("read feedback buffers");
    assert!(
        bufs.len() >= COUNT,
        "expected at least {COUNT} feedback buffers, got {}",
        bufs.len(),
    );

    let mut expected = vec![0u8; FB_SIZE];
    expected[..PAYLOAD.len()].copy_from_slice(PAYLOAD.as_bytes());
    for (i, buf) in bufs.iter().enumerate() {
        assert!(
            buf.len() >= FB_SIZE,
            "buffer {i} shorter than registered size: {} < {FB_SIZE}",
            buf.len(),
        );
        assert_eq!(
            &buf[..FB_SIZE],
            expected.as_slice(),
            "feedback buffer {i} content did not match what the guest wrote",
        );
    }
}

/// Distinct payloads in many buffers read back as exactly that set: no buffer
/// aliases another's pages.
#[test]
fn feedback_buffers_hold_distinct_content() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("feedback_buffers_hold_distinct_content");
    };

    let mut branch = ready.branch().expect("fork branch");

    let probe = branch
        .bash(
            BashTarget::container("idle"),
            &format!("test -x {DRIVER}"),
            false,
        )
        .expect("probe driver");
    assert!(
        probe.success(),
        "expected the driver at {DRIVER} (rebuild the integration-tests workload \
         image): exit={}",
        probe.exit_code,
    );

    const COUNT: usize = 18;
    let expected: Vec<String> = (1..=COUNT).map(|i| format!("fb-distinct-{i}")).collect();
    branch
        .bash(
            BashTarget::container("idle"),
            &format!(
                "for i in $(seq 1 {COUNT}); do {DRIVER} \"fb-distinct-$i\" >/dev/null 2>&1 & done"
            ),
            false,
        )
        .expect("launch feedback-buffer drivers");

    let deadline = branch.current_time() + vt_dur!(10 s);
    loop {
        let n = branch
            .feedback_buffers_to_vec(FB_ID)
            .expect("read feedback buffers")
            .len();
        if n >= COUNT {
            break;
        }
        assert!(
            branch.current_time() < deadline,
            "only {n}/{COUNT} feedback buffers registered before the deadline",
        );
        branch.run_for(vt_dur!(50 ms)).expect("advance guest");
    }

    // Payload = bytes up to the first NUL.
    let bufs = branch
        .feedback_buffers_to_vec(FB_ID)
        .expect("read feedback buffers");
    let mut got: Vec<String> = bufs
        .iter()
        .map(|b| {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        })
        .collect();
    got.sort();

    let mut want = expected.clone();
    want.sort();

    assert_eq!(
        got, want,
        "feedback buffers did not hold the distinct per-buffer content that was written",
    );
}

/// A buffer mapped in a fresh fork *before* the child writes it stays coherent
/// with later writes. Requires the kernel to CoW the pages into the child at
/// map time; otherwise the mapping would point at the parent's frames and go
/// stale.
#[test]
fn feedback_buffer_reflects_writes_after_mapping() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("feedback_buffer_reflects_writes_after_mapping");
    };

    let mut parent = ready.branch().expect("fork parent branch");

    let probe = parent
        .bash(
            BashTarget::container("idle"),
            &format!("test -x {COUNTER_DRIVER}"),
            false,
        )
        .expect("probe driver");
    assert!(
        probe.success(),
        "expected the driver at {COUNTER_DRIVER} (rebuild the integration-tests \
         workload image): exit={}",
        probe.exit_code,
    );

    parent
        .bash(
            BashTarget::container("idle"),
            &format!("{COUNTER_DRIVER} >/dev/null 2>&1 &"),
            false,
        )
        .expect("launch counter driver");

    let deadline = parent.current_time() + vt_dur!(5 s);
    while !registered_id(&parent, COUNTER_FB_ID) {
        assert!(
            parent.current_time() < deadline,
            "counter driver never registered its buffer",
        );
        parent.run_for(vt_dur!(50 ms)).expect("advance parent");
    }

    // None of the buffer's pages are CoW'd in the child yet.
    let checkpoint = parent.checkpoint().expect("checkpoint parent");
    let mut child = checkpoint.branch().expect("fork child");

    // Map before the child runs.
    let c1 = read_counter(&mut child).expect("counter buffer mapped in child");

    child.run_for(vt_dur!(200 ms)).expect("advance child");

    // Same mapping, no remap.
    let c2 = read_counter(&mut child).expect("counter buffer still mapped");

    assert!(
        c2 > c1,
        "feedback buffer mapping did not reflect guest writes made after mapping: \
         c1={c1}, c2={c2} (the mapping went stale)",
    );
}

/// Whether a feedback buffer under `id` is registered on `branch`.
fn registered_id(branch: &bedrock_lab::Branch, id: &[u8]) -> bool {
    branch
        .feedback_buffer_ids()
        .expect("list feedback ids")
        .iter()
        .any(|i| i == id)
}

/// Read the counter buffer's leading LE u64 (mapped once, then re-read).
fn read_counter(branch: &mut bedrock_lab::Branch) -> Option<u64> {
    let bufs = branch
        .feedback_buffers_to_vec(COUNTER_FB_ID)
        .expect("read counter buffer");
    let b = bufs.first()?;
    if b.len() < 8 {
        return None;
    }
    Some(u64::from_le_bytes(b[..8].try_into().unwrap()))
}

/// Whether a feedback buffer under [`FB_ID`] is registered on `branch`.
fn registered(branch: &bedrock_lab::Branch) -> bool {
    branch
        .feedback_buffer_ids()
        .expect("list feedback ids")
        .iter()
        .any(|id| id == FB_ID)
}
