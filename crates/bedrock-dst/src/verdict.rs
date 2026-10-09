// SPDX-License-Identifier: GPL-2.0

//! Aggregates a run's assertion records into a verdict.
//!
//! A run fails if any `Always` record has `result == false`. `Sometimes`
//! records are coverage: a signature is satisfied if any of its records holds.
//! Signatures are the message up to the first `": "`, so records that differ
//! only in detail dedup together.
//!
//! Some coverage is required: a run whose load or checks never took effect
//! proves nothing, so a required signature that is never satisfied fails the
//! run as `C/missing/<signature>`.

use std::collections::{BTreeMap, BTreeSet};

use bedrock_dst_contract::Signature;
use serde::{Deserialize, Serialize};

use crate::workload::Opaque;

/// One assertion record, as far as the verdict needs it. Lenient: a record
/// without a `result` counts as failing, like one whose result is false.
#[derive(Deserialize)]
enum Record {
    Always(RecordData),
    Sometimes(RecordData),
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RecordData {
    result: bool,
    message: String,
    location: RecordLocation,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RecordLocation {
    file: String,
}

/// Just a verdict's outcome (reading a verdict.json back).
#[derive(Deserialize)]
pub struct VerdictPass {
    pub pass: bool,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Verdict {
    pub pass: bool,
    /// Failing `Always` signatures with their count and first message.
    pub failures: BTreeMap<String, Failure>,
    pub sometimes_satisfied: BTreeSet<String>,
    pub sometimes_unsatisfied: BTreeSet<String>,
    pub records: usize,
    pub unparsed: usize,
    /// The seed's swarm record (the workload's), so failures can be
    /// grouped by feature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swarm: Option<Opaque>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Failure {
    pub count: usize,
    pub first: String,
    pub location: String,
}

pub fn signature(message: &str) -> &str {
    message.split_once(": ").map_or(message, |(s, _)| s)
}

pub fn aggregate(assertions_jsonl: &str, required: &[Signature]) -> Verdict {
    let mut v = Verdict::default();
    let mut sometimes: BTreeMap<String, bool> = BTreeMap::new();
    for line in assertions_jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(rec) = serde_json::from_str::<Record>(line) else {
            v.unparsed += 1;
            continue;
        };
        let (always, data) = match rec {
            Record::Always(d) => (true, d),
            Record::Sometimes(d) => (false, d),
        };
        v.records += 1;
        let sig = signature(&data.message).to_string();
        if !always {
            *sometimes.entry(sig).or_insert(false) |= data.result;
        } else if !data.result {
            v.failures
                .entry(sig)
                .and_modify(|f| f.count += 1)
                .or_insert_with(|| Failure {
                    count: 1,
                    first: data.message.clone(),
                    location: data.location.file.clone(),
                });
        }
    }
    for Signature(sig) in required {
        if !sometimes.get(sig).copied().unwrap_or(false) {
            v.failures.insert(
                format!("C/missing/{sig}"),
                Failure {
                    count: 1,
                    first: format!("required coverage {sig} never satisfied"),
                    location: "bedrock-dst".into(),
                },
            );
        }
    }
    for (sig, ok) in sometimes {
        if ok {
            v.sometimes_satisfied.insert(sig);
        } else {
            v.sometimes_unsatisfied.insert(sig);
        }
    }
    v.pass = v.failures.is_empty();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sigs(s: &[&str]) -> Vec<Signature> {
        s.iter().map(|s| Signature::from(*s)).collect()
    }

    fn rec(kind: &str, result: bool, msg: &str) -> String {
        use bedrock_assertions::{Assertion, Condition, Location};
        let loc = Location::new("guest/oracle", 0, 0);
        let a = match kind {
            "Always" => Assertion::always(Condition::Bool(result), msg, loc),
            _ => Assertion::sometimes(Condition::Bool(result), msg, loc),
        };
        serde_json::to_string(&a).unwrap()
    }

    #[test]
    fn empty_run_passes() {
        assert!(aggregate("", &[]).pass);
    }

    #[test]
    fn missing_required_coverage_fails() {
        let v = aggregate("", &sigs(&["S/load-included"]));
        assert!(!v.pass);
        assert!(v.failures.contains_key("C/missing/S/load-included"));
        let log = [
            rec("Sometimes", false, "S/load-included"),
            rec("Sometimes", true, "S/kill"),
        ]
        .join("\n");
        let v = aggregate(&log, &sigs(&["S/load-included", "S/kill"]));
        assert_eq!(
            v.failures.keys().collect::<Vec<_>>(),
            ["C/missing/S/load-included"]
        );
        let log = rec("Sometimes", true, "S/load-included");
        assert!(aggregate(&log, &sigs(&["S/load-included"])).pass);
    }

    #[test]
    fn failing_always_fails_and_dedups_by_signature() {
        let log = [
            rec("Always", true, "E4/graceful-stop"),
            rec("Always", false, "E1/panic: at a.rs:1"),
            rec("Always", false, "E1/panic: at b.rs:2"),
            "garbage".to_string(),
        ]
        .join("\n");
        let v = aggregate(&log, &[]);
        assert!(!v.pass);
        assert_eq!(v.records, 3);
        assert_eq!(v.unparsed, 1);
        let f = &v.failures["E1/panic"];
        assert_eq!(f.count, 2);
        assert_eq!(f.first, "E1/panic: at a.rs:1");
        assert_eq!(f.location, "guest/oracle");
    }

    #[test]
    fn sometimes_is_coverage_not_failure() {
        let log = [
            rec("Sometimes", false, "S/recovered"),
            rec("Sometimes", true, "S/kill: persisted=None"),
            rec("Sometimes", false, "S/kill"),
        ]
        .join("\n");
        let v = aggregate(&log, &[]);
        assert!(v.pass);
        assert!(v.sometimes_satisfied.contains("S/kill"));
        assert!(v.sometimes_unsatisfied.contains("S/recovered"));
    }

    #[test]
    fn workload_monitor_records_parse() {
        // Shape written by workload-monitor's always_eq!.
        let line = r#"{"Always":{"condition":{"Eq":{"x":101,"y":0}},"result":false,"message":"container node exit code is zero","location":{"file":"guest/workload-monitor/src/main.rs","line":1,"column":1}}}"#;
        let v = aggregate(line, &[]);
        assert!(v.failures.contains_key("container node exit code is zero"));
    }
}
