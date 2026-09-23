//! The guest-side assertion pipeline.
//!
//! The `workload-monitor` service emits an exit-code [`Assertion`] on every
//! container or `podman exec` death, surfaced on the serial console as journald
//! JSON (`{"SYSLOG_IDENTIFIER":"assertions","MESSAGE":<json>}`) — the stream the
//! host oracle reads. These tests decode assertions from there.
//!
//! Both death kinds are provoked: `exec_died` (every container `bash` command is
//! a `podman exec`) and `died` (a throwaway `podman run`).

use bedrock_assertions::{Assertion, Condition};
use bedrock_lab::{BashTarget, Branch};
use bedrock_vm::ConsoleLine;

use crate::common;

/// Keep in sync with `EXEC_DEATH_MSG` in `guest/workload-monitor/src/main.rs`.
const EXEC_DEATH_MSG: &str = "exec exit code is zero";

/// `SYSLOG_IDENTIFIER` of assertion records (`systemd-cat -t` in `guest/init`).
const ASSERTION_TAG: &str = "assertions";

/// Decode a serial line tagged [`ASSERTION_TAG`] into an [`Assertion`]. A tagged
/// line with an invalid payload fails the test.
fn assertion_from_serial(raw: &str) -> Option<Assertion> {
    let ConsoleLine::Journal { source, message } = ConsoleLine::parse(raw) else {
        return None;
    };
    if source != ASSERTION_TAG {
        return None;
    }
    let json = message.trim();
    Some(serde_json::from_str::<Assertion>(json).unwrap_or_else(|e| {
        panic!("`assertions` serial line is not a valid Assertion: {e}\njson: {json}")
    }))
}

/// Assertions on `branch`'s serial log so far; a fresh fork starts with none.
fn assertions_on_serial(branch: &Branch) -> Vec<Assertion> {
    common::capture_sink()
        .serial_lines(branch.id())
        .iter()
        .filter_map(|line| assertion_from_serial(line))
        .collect()
}

/// Advance until an assertion matching `pred` appears (the pipeline is async).
fn wait_for_assertion(
    branch: &mut Branch,
    what: &str,
    pred: impl Fn(&Assertion) -> bool,
) -> Assertion {
    let deadline = branch.current_time() + vt_dur!(20 s);
    loop {
        if let Some(found) = assertions_on_serial(branch).into_iter().find(|a| pred(a)) {
            return found;
        }
        assert!(
            branch.current_time() < deadline,
            "no assertion matching {what} reached the serial log within the budget",
        );
        branch.run_for(vt_dur!(200 ms)).expect("advance guest");
    }
}

/// An `always_eq!(code, 0, EXEC_DEATH_MSG)` record.
fn is_exec_death(a: &Assertion, code: i128) -> bool {
    matches!(a, Assertion::Always(_))
        && a.data().message == EXEC_DEATH_MSG
        && a.condition() == Condition::Eq { x: code, y: 0 }
}

/// An `Always` `x == 0` record for container `name`'s main process dying.
fn is_container_death(a: &Assertion, name: &str, code: i128) -> bool {
    matches!(a, Assertion::Always(_))
        && a.data().message == format!("container {name} exit code is zero")
        && a.condition() == Condition::Eq { x: code, y: 0 }
}

#[test]
fn workload_monitor_records_clean_exec_exit() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("workload_monitor_records_clean_exec_exit");
    };

    let mut branch = ready.branch().expect("fork branch");

    // Runs as `podman exec`, so its death is an `exec_died` event.
    let out = branch
        .bash(BashTarget::container("idle"), "exit 0", false)
        .expect("run clean exec");
    assert_eq!(out.exit_code, 0, "exec should exit 0");

    let recorded = wait_for_assertion(&mut branch, "a clean exec death", |a| is_exec_death(a, 0));
    assert!(
        recorded.holds(),
        "exit code 0 must satisfy the always-zero invariant: {recorded:?}",
    );
}

#[test]
fn workload_monitor_flags_failing_exec_exit() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("workload_monitor_flags_failing_exec_exit");
    };

    let mut branch = ready.branch().expect("fork branch");

    // Distinctive so our record is unambiguous.
    const FAIL_CODE: i128 = 42;
    let out = branch
        .bash(BashTarget::container("idle"), "exit 42", false)
        .expect("run failing exec");
    assert_eq!(out.exit_code, 42, "exec should exit 42");

    let recorded = wait_for_assertion(&mut branch, "a failing exec death", |a| {
        is_exec_death(a, FAIL_CODE)
    });
    assert!(
        !recorded.holds(),
        "a non-zero exec exit must violate the always-zero invariant: {recorded:?}",
    );
}

#[test]
fn workload_monitor_records_container_death() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("workload_monitor_records_container_death");
    };

    let mut branch = ready.branch().expect("fork branch");

    // A throwaway container on the already-loaded image whose entrypoint just
    // exits. `--network none` avoids netavark; `died` fires before `--rm`.
    const NAME: &str = "expiring-probe";
    const EXIT_CODE: i128 = 17;
    let out = branch
        .bash(
            BashTarget::host(),
            &format!(
                "podman run --rm --network none --name {NAME} --entrypoint /bin/sh \
                 bedrock/integration-tests-ready:latest -c 'exit {EXIT_CODE}'"
            ),
            false,
        )
        .expect("run throwaway container");
    assert_eq!(
        i128::from(out.exit_code),
        EXIT_CODE,
        "container main process should exit {EXIT_CODE}",
    );

    let recorded = wait_for_assertion(&mut branch, "the container death", |a| {
        is_container_death(a, NAME, EXIT_CODE)
    });
    assert!(
        !recorded.holds(),
        "a non-zero container exit must violate the always-zero invariant: {recorded:?}",
    );
}
