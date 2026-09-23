//! Coverage feedback end to end: instrumented guest binaries record edges into
//! a libfeedback buffer that the lab reads back. One container per frontend:
//!   - `coverage-pcguard` — LLVM `-fsanitize-coverage=trace-pc-guard` via
//!     `guest/libpcguard.c`.
//!   - `coverage-go` — the Antithesis Go SDK via `guest/libvoidstar.c`.
//!
//! Buffers are found by their "cov-<build>" id shape and must contain nonzero
//! edge counts; one test also checks that running more code adds coverage.

use bedrock_lab::{BashTarget, Branch};

use crate::common;

const PCGUARD_CONTAINER: &str = "coverage-pcguard";
const PCGUARD_DRIVER: &str = "/opt/bedrock/drivers/cov_pcguard";
const GO_CONTAINER: &str = "coverage-go";
const GO_DRIVER: &str = "/opt/bedrock/drivers/cov_go";

/// Phase sentinels (sync with `coverage-pcguard/cov_target.c`): the driver
/// touches READY after its baseline and waits for MORE before going deeper.
const READY_PATH: &str = "/tmp/cov-ready";
const MORE_PATH: &str = "/tmp/cov-more";

/// trace-pc-guard coverage round-trips under "cov-<build-id>".
#[test]
fn pcguard_coverage_round_trips_from_guest() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("pcguard_coverage_round_trips_from_guest");
    };

    let mut branch = ready.branch().expect("fork branch");
    wait_for_driver(&mut branch, PCGUARD_CONTAINER, PCGUARD_DRIVER);

    let id = launch_and_find_id(
        &mut branch,
        PCGUARD_CONTAINER,
        &format!("{PCGUARD_DRIVER} >/dev/null 2>&1 &"),
        is_pcguard_id,
        "cov-<gnu-build-id-hex>",
    );
    advance_until_coverage(&mut branch, &id);
}

/// Running more of the target lights edges (bytes zero → nonzero) that the
/// baseline snapshot of the same buffer didn't have.
#[test]
fn pcguard_running_more_code_increases_observed_coverage() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("pcguard_running_more_code_increases_observed_coverage");
    };

    let mut branch = ready.branch().expect("fork branch");
    wait_for_driver(&mut branch, PCGUARD_CONTAINER, PCGUARD_DRIVER);

    let id = launch_and_find_id(
        &mut branch,
        PCGUARD_CONTAINER,
        &format!("{PCGUARD_DRIVER} >/dev/null 2>&1 &"),
        is_pcguard_id,
        "cov-<gnu-build-id-hex>",
    );

    wait_for_file(&mut branch, PCGUARD_CONTAINER, READY_PATH);
    let baseline = read_buffer(&mut branch, &id);
    let baseline_hits = baseline.iter().filter(|&&b| b != 0).count();
    assert!(baseline_hits > 0, "shallow baseline recorded no coverage");

    create_file(&mut branch, PCGUARD_CONTAINER, MORE_PATH);
    let deadline = branch.current_time() + vt_dur!(10 s);
    let deeper = loop {
        let buf = read_buffer(&mut branch, &id);
        let hits = buf.iter().filter(|&&b| b != 0).count();
        if hits > baseline_hits {
            break buf;
        }
        assert!(
            branch.current_time() < deadline,
            "coverage did not grow after releasing the deeper stages \
             (stuck at {baseline_hits} edges)",
        );
        branch.run_for(vt_dur!(50 ms)).expect("advance guest");
    };

    assert_eq!(
        baseline.len(),
        deeper.len(),
        "buffer size changed between snapshots",
    );
    let new_edges = deeper
        .iter()
        .zip(&baseline)
        .filter(|(d, s)| **d != 0 && **s == 0)
        .count();
    assert!(
        new_edges > 0,
        "running more code lit no new edges (baseline={baseline_hits} edges): \
         a coverage increase was not observed",
    );
}

/// Go SDK coverage round-trips under "cov-go-<hex>.sym.tsv".
#[test]
fn go_coverage_round_trips_from_guest() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("go_coverage_round_trips_from_guest");
    };

    let mut branch = ready.branch().expect("fork branch");
    wait_for_driver(&mut branch, GO_CONTAINER, GO_DRIVER);

    let id = launch_and_find_id(
        &mut branch,
        GO_CONTAINER,
        &format!("{GO_DRIVER} >/dev/null 2>&1 &"),
        is_go_id,
        "cov-go-<hex>.sym.tsv",
    );
    advance_until_coverage(&mut branch, &id);
}

/// "cov-" + lowercase-hex GNU build-id.
fn is_pcguard_id(id: &[u8]) -> bool {
    matches!(id.strip_prefix(b"cov-"), Some(rest) if is_lower_hex(rest))
}

/// "cov-go-<lowercase-hex>.sym.tsv".
fn is_go_id(id: &[u8]) -> bool {
    let Some(rest) = id.strip_prefix(b"cov-go-") else {
        return false;
    };
    matches!(rest.strip_suffix(b".sym.tsv"), Some(hash) if is_lower_hex(hash))
}

fn is_lower_hex(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Launch `cmd` and advance until an id matching `expected` registers.
/// `shape` is only for the failure message.
fn launch_and_find_id(
    branch: &mut Branch,
    container: &str,
    cmd: &str,
    expected: fn(&[u8]) -> bool,
    shape: &str,
) -> Vec<u8> {
    branch
        .bash(BashTarget::container(container), cmd, false)
        .expect("launch coverage driver");

    let deadline = branch.current_time() + vt_dur!(15 s);
    loop {
        let ids = branch.feedback_buffer_ids().expect("list feedback ids");
        if let Some(id) = ids.into_iter().find(|id| expected(id)) {
            return id;
        }
        assert!(
            branch.current_time() < deadline,
            "no feedback buffer matching {shape:?} registered in {container}",
        );
        branch.run_for(vt_dur!(50 ms)).expect("advance guest");
    }
}

/// Read the single buffer registered under `id`.
fn read_buffer(branch: &mut Branch, id: &[u8]) -> Vec<u8> {
    let mut bufs = branch
        .feedback_buffers_to_vec(id)
        .expect("read coverage buffer");
    assert_eq!(
        bufs.len(),
        1,
        "expected exactly one buffer under {:?}",
        String::from_utf8_lossy(id),
    );
    bufs.pop().unwrap()
}

/// Advance until the buffer has coverage (it is all-zero right after load).
fn advance_until_coverage(branch: &mut Branch, id: &[u8]) -> Vec<u8> {
    let deadline = branch.current_time() + vt_dur!(10 s);
    loop {
        let buf = read_buffer(branch, id);
        if buf.iter().any(|&b| b != 0) {
            return buf;
        }
        assert!(
            branch.current_time() < deadline,
            "buffer under {:?} never recorded any coverage",
            String::from_utf8_lossy(id),
        );
        branch.run_for(vt_dur!(50 ms)).expect("advance guest");
    }
}

/// Poll until `driver` is executable (containers may start around ready).
fn wait_for_driver(branch: &mut Branch, container: &str, driver: &str) {
    let deadline = branch.current_time() + vt_dur!(20 s);
    loop {
        if let Ok(probe) = branch.bash(
            BashTarget::container(container),
            &format!("test -x {driver}"),
            false,
        ) {
            if probe.success() {
                return;
            }
        }
        assert!(
            branch.current_time() < deadline,
            "driver {driver} never became available in {container} \
             (rebuild the integration-tests workload image)",
        );
        branch.run_for(vt_dur!(100 ms)).expect("advance guest");
    }
}

/// Advance the guest until `path` exists in `container`.
fn wait_for_file(branch: &mut Branch, container: &str, path: &str) {
    let deadline = branch.current_time() + vt_dur!(10 s);
    loop {
        if let Ok(probe) = branch.bash(
            BashTarget::container(container),
            &format!("test -f {path}"),
            false,
        ) {
            if probe.success() {
                return;
            }
        }
        assert!(
            branch.current_time() < deadline,
            "{path} never appeared in {container}",
        );
        branch.run_for(vt_dur!(50 ms)).expect("advance guest");
    }
}

/// Create `path` in `container` (the driver waits on it before its deeper stages).
fn create_file(branch: &mut Branch, container: &str, path: &str) {
    branch
        .bash(
            BashTarget::container(container),
            &format!(": > {path}"),
            false,
        )
        .expect("create release sentinel");
}
