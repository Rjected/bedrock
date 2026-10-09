// SPDX-License-Identifier: GPL-2.0

//! The driver's side of the workload contract (`bedrock-dst-contract`).
//!
//! A workload is a guest command (`--workload-cmd`) whose hooks the driver
//! runs in the guest (`warmup`, `start`, `finalize`, and a branch's mid-run
//! action) and a host-side planner (`--workload-planner`) whose pure hooks
//! (`parse-args`, `config`, `observe`) it runs on the host. The driver
//! instantiates the contract's workload-specific payloads (arguments, guest
//! config, decisions, swarm record) with [`Opaque`]: JSON it stores and
//! passes back verbatim, never looks into.

use std::error::Error;
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use bedrock_dst_contract::{
    ArgsRequest, Observation, ObserveRequest, Plan, PlanRequest, WarmupStatus,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// A workload-defined JSON payload, kept byte for byte (a guest config, a
/// scenario, a swarm record, the workload's arguments).
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Opaque(Box<RawValue>);

impl Opaque {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(Opaque(RawValue::from_string(text.trim().to_string())?))
    }

    pub fn get(&self) -> &str {
        self.0.get()
    }
}

impl PartialEq for Opaque {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl fmt::Debug for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.get())
    }
}

impl fmt::Display for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.get())
    }
}

/// The contract's types as the driver sees them.
pub type WorkloadPlan = Plan<Opaque, Opaque, Opaque>;
pub type WorkloadPlanRequest = PlanRequest<Opaque, Opaque>;
pub type WorkloadObservation = Observation<Opaque>;

/// A workload: its guest command and host planner.
#[derive(Debug, Clone)]
pub struct Workload {
    pub cmd: String,
    pub planner: PathBuf,
}

impl Workload {
    /// `<planner> <hook> -` with `req` on stdin; its stdout, parsed.
    pub(crate) fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        hook: &str,
        req: &Req,
    ) -> Result<Resp> {
        let what = || format!("{} {hook}", self.planner.display());
        let mut child = Command::new(&self.planner)
            .args([hook, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e} (pass --workload-planner)", what()))?;
        child
            .stdin
            .take()
            .ok_or("no planner stdin")?
            .write_all(&serde_json::to_vec(req)?)?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(format!(
                "{} failed ({}): {}",
                what(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into());
        }
        Ok(serde_json::from_slice(&out.stdout).map_err(|e| format!("{}: output: {e}", what()))?)
    }

    /// `parse-args`: the workload's typed arguments.
    pub fn parse_args(&self, req: &ArgsRequest) -> Result<Opaque> {
        self.call("parse-args", req)
    }

    /// `config`: a seed's (or recorded decisions') plan.
    pub fn config(&self, req: &WorkloadPlanRequest) -> Result<WorkloadPlan> {
        self.call("config", req)
    }

    /// `observe`: a finished run's decisions with what it reported.
    pub fn observe(&self, req: &ObserveRequest<Opaque>) -> Result<WorkloadObservation> {
        self.call("observe", req)
    }

    /// A guest command running hook `hook`, with `arg` shell-quoted.
    pub fn guest_cmd(&self, hook: &str, arg: Option<&str>) -> String {
        match arg {
            Some(a) => format!("{} {hook} {}", self.cmd, sh_quote(a)),
            None => format!("{} {hook}", self.cmd),
        }
    }
}

/// The last line of a `warmup` hook's output that is its status.
pub fn warmup_status(out: &str) -> Option<WarmupStatus> {
    out.lines()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
}

/// Single-quotes `s` for bash (`'` becomes `'\''`).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_payloads_round_trip_verbatim() {
        let text = "{\n  \"b\": 1,\n  \"a\": [2, 3]\n}";
        let o = Opaque::parse(text).unwrap();
        assert_eq!(o.get(), text);
        // Nested in a response, still byte for byte.
        #[derive(Deserialize, Serialize)]
        struct R {
            x: Opaque,
        }
        let r: R = serde_json::from_str(&format!("{{\"x\": {text}}}")).unwrap();
        assert_eq!(r.x, o);
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            format!("{{\"x\":{text}}}")
        );
        assert!(Opaque::parse("{").is_err());
    }

    #[test]
    fn quoting_and_warmup_status() {
        assert_eq!(sh_quote(r#"{"a":1}"#), r#"'{"a":1}'"#);
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        let st =
            warmup_status("log\n{\"ready\": false, \"detail\": \"head 3\"}\nmore log\n").unwrap();
        assert!(!st.ready);
        assert_eq!(st.detail, "head 3");
        assert!(warmup_status("no json\n42").is_none());
        let w = Workload {
            cmd: "wl".into(),
            planner: "p".into(),
        };
        assert_eq!(w.guest_cmd("finalize", None), "wl finalize");
        assert_eq!(w.guest_cmd("start", Some("{}")), "wl start '{}'");
    }
}
