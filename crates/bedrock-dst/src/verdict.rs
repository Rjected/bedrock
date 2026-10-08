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

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Verdict {
    pub pass: bool,
    /// Failing `Always` signatures with their count and first message.
    pub failures: BTreeMap<String, Failure>,
    pub sometimes_satisfied: BTreeSet<String>,
    pub sometimes_unsatisfied: BTreeSet<String>,
    pub records: usize,
    pub unparsed: usize,
    /// The seed's swarm record (the driver's `Swarm`), so failures can be
    /// grouped by feature.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub swarm: Value,
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

pub fn aggregate(assertions_jsonl: &str, required: &[&str]) -> Verdict {
    let mut v = Verdict::default();
    let mut sometimes: BTreeMap<String, bool> = BTreeMap::new();
    for line in assertions_jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            v.unparsed += 1;
            continue;
        };
        let (kind, data) = match (rec.get("Always"), rec.get("Sometimes")) {
            (Some(d), _) => ("always", d),
            (_, Some(d)) => ("sometimes", d),
            _ => {
                v.unparsed += 1;
                continue;
            }
        };
        v.records += 1;
        let result = data.get("result").and_then(Value::as_bool).unwrap_or(false);
        let message = data.get("message").and_then(Value::as_str).unwrap_or("");
        let sig = signature(message).to_string();
        if kind == "sometimes" {
            *sometimes.entry(sig).or_insert(false) |= result;
        } else if !result {
            let location = data
                .pointer("/location/file")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            v.failures
                .entry(sig)
                .and_modify(|f| f.count += 1)
                .or_insert_with(|| Failure {
                    count: 1,
                    first: message.to_string(),
                    location,
                });
        }
    }
    for sig in required {
        if !sometimes.get(*sig).copied().unwrap_or(false) {
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

    fn rec(kind: &str, result: bool, msg: &str) -> String {
        serde_json::json!({kind: {"condition": {"Bool": result}, "result": result, "message": msg,
            "location": {"file": "tempo-dst/oracle", "line": 0, "column": 0}}})
        .to_string()
    }

    #[test]
    fn empty_run_passes() {
        assert!(aggregate("", &[]).pass);
    }

    #[test]
    fn missing_required_coverage_fails() {
        let v = aggregate("", &["S/load-included"]);
        assert!(!v.pass);
        assert!(v.failures.contains_key("C/missing/S/load-included"));
        let log = [
            rec("Sometimes", false, "S/load-included"),
            rec("Sometimes", true, "S/kill"),
        ]
        .join("\n");
        let v = aggregate(&log, &["S/load-included", "S/kill"]);
        assert_eq!(
            v.failures.keys().collect::<Vec<_>>(),
            ["C/missing/S/load-included"]
        );
        let log = rec("Sometimes", true, "S/load-included");
        assert!(aggregate(&log, &["S/load-included"]).pass);
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
        assert_eq!(f.location, "tempo-dst/oracle");
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
        let line = r#"{"Always":{"condition":{"Eq":{"x":101,"y":0}},"result":false,"message":"container tempo exit code is zero","location":{"file":"guest/workload-monitor/src/main.rs","line":1,"column":1}}}"#;
        let v = aggregate(line, &[]);
        assert!(v.failures.contains_key("container tempo exit code is zero"));
    }
}
