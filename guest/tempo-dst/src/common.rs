// SPDX-License-Identifier: GPL-2.0

//! Shared plumbing: run config, the assertion and event sinks, guest time, and
//! node RPC.
//!
//! Everything here must stay deterministic under Bedrock: time comes from the
//! guest's emulated clock, and randomness (`rand`, seeded via getrandom) from
//! Bedrock's controlled getrandom stream.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::Duration;

use bedrock_assertions::{Assertion, Condition, Location};
use nix::time::{clock_gettime, ClockId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const NODE_CONTAINER: &str = "tempo";
const RPC_URL: &str = "http://127.0.0.1:8545";
pub const CONFIG_PATH: &str = "/bedrock/in/config.json";
pub const EVENTS_PATH: &str = "/bedrock/events.jsonl";
pub const ASSERTIONS_PATH: &str = "/bedrock/assertions.jsonl";
pub const OUT_DIR: &str = "/bedrock/out";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NemesisConfig {
    pub enabled: bool,
    pub max_kills: u32,
    pub min_gap_secs: u32,
}

impl Default for NemesisConfig {
    fn default() -> Self {
        NemesisConfig {
            enabled: true,
            max_kills: 3,
            min_gap_secs: 20,
        }
    }
}

/// txgen load container; `count == 0` disables it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct LoadConfig {
    pub seed: u64,
    pub count: u64,
    pub tps: u64,
    /// Load image; empty means the plain txgen image.
    pub image: String,
    /// txgen spec inside the image; empty means the image's default.
    pub spec: String,
}

/// A RawStorage contract whose storage root E5 checks against an
/// independent reference at every block.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct TrieConfig {
    pub address: String,
    pub slots: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Virtual seconds the branch runs before finalize.
    pub run_secs: u64,
    pub nemesis: NemesisConfig,
    /// Guest seconds the head may stall before liveness fails.
    pub liveness_secs: u64,
    pub load: LoadConfig,
    pub trie: Option<TrieConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            run_secs: 120,
            nemesis: NemesisConfig::default(),
            liveness_secs: 60,
            load: LoadConfig::default(),
            trie: None,
        }
    }
}

impl Config {
    pub fn load() -> Config {
        match std::fs::read_to_string(CONFIG_PATH) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| panic!("bad {CONFIG_PATH}: {e}")),
            Err(_) => Config::default(),
        }
    }
}

/// Guest `CLOCK_MONOTONIC` in nanoseconds, derived from Bedrock's emulated TSC.
pub fn guest_time_ns() -> u64 {
    let t = clock_gettime(ClockId::CLOCK_MONOTONIC).expect("clock_gettime");
    t.tv_sec() as u64 * 1_000_000_000 + t.tv_nsec() as u64
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DstEvent {
    pub source: String,
    pub kind: String,
    pub container: Option<String>,
    pub guest_time_ns: u64,
    #[serde(default)]
    pub detail: Value,
}

fn append_line(path: &str, line: &str) {
    match OpenOptions::new().create(true).append(true).open(path) {
        // One write per line keeps concurrent appenders from interleaving.
        Ok(mut f) => {
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
        Err(e) => eprintln!("cannot append to {path}: {e}"),
    }
}

pub fn emit_event(source: &str, kind: &str, container: Option<&str>, detail: Value) {
    let event = DstEvent {
        source: source.into(),
        kind: kind.into(),
        container: container.map(Into::into),
        guest_time_ns: guest_time_ns(),
        detail,
    };
    append_line(EVENTS_PATH, &serde_json::to_string(&event).unwrap());
}

pub fn read_events() -> Vec<DstEvent> {
    std::fs::read_to_string(EVENTS_PATH)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Records an assertion. `signature` is a stable id the driver dedups by; it
/// prefixes the message.
pub fn assert_always(cond: Condition, component: &str, signature: &str, detail: &str) {
    write_assertion(Assertion::always(
        cond,
        message(signature, detail),
        location(component),
    ));
}

pub fn assert_sometimes(cond: Condition, component: &str, signature: &str, detail: &str) {
    write_assertion(Assertion::sometimes(
        cond,
        message(signature, detail),
        location(component),
    ));
}

fn location(component: &str) -> Location {
    Location::new(format!("tempo-dst/{component}"), 0, 0)
}

fn message(signature: &str, detail: &str) -> String {
    if detail.is_empty() {
        signature.to_string()
    } else {
        format!("{signature}: {detail}")
    }
}

fn write_assertion(a: Assertion) {
    append_line(ASSERTIONS_PATH, &serde_json::to_string(&a).unwrap());
}

fn rpc(method: &str, params: Value) -> Result<Value, String> {
    let resp: Value = ureq::post(RPC_URL)
        .timeout(Duration::from_secs(10))
        .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;
    match resp.get("error") {
        Some(err) => Err(err.to_string()),
        None => resp
            .get("result")
            .cloned()
            .ok_or_else(|| "no result".into()),
    }
}

pub fn head_number() -> Result<u64, String> {
    let head = rpc("eth_blockNumber", json!([]))?;
    head.as_str()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| format!("bad eth_blockNumber: {head}"))
}

/// Number of transactions in `block`.
pub fn tx_count(block: u64) -> Result<u64, String> {
    let n = rpc(
        "eth_getBlockTransactionCountByNumber",
        json!([format!("0x{block:x}")]),
    )?;
    n.as_str()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| format!("bad transaction count: {n}"))
}

/// The node's finalized block as `(number, hash)`; `None` before any.
pub fn finalized_block() -> Result<Option<(u64, String)>, String> {
    let block = rpc("eth_getBlockByNumber", json!(["finalized", false]))?;
    if block.is_null() {
        return Ok(None);
    }
    let number = block
        .get("number")
        .and_then(Value::as_str)
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| format!("bad finalized block: {block}"))?;
    let hash = block
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("bad finalized block: {block}"))?;
    Ok(Some((number, hash.to_lowercase())))
}

/// Values of `slots` of `address` at `block`.
pub fn storage_at(
    address: &str,
    slots: &[u64],
    block: u64,
) -> Result<crate::trie_ref::Storage, String> {
    slots
        .iter()
        .map(|slot| {
            let value = rpc(
                "eth_getStorageAt",
                json!([address, format!("0x{slot:x}"), format!("0x{block:x}")]),
            )?;
            Ok((
                *slot,
                serde_json::from_value(value).map_err(|e| e.to_string())?,
            ))
        })
        .collect()
}

/// `eth_getProof` for `slots` of `address` at `block`.
pub fn proof(
    address: &str,
    slots: &[u64],
    block: u64,
) -> Result<crate::trie_ref::AccountProof, String> {
    let keys: Vec<String> = slots.iter().map(|s| format!("0x{s:x}")).collect();
    let proof = rpc(
        "eth_getProof",
        json!([address, keys, format!("0x{block:x}")]),
    )?;
    serde_json::from_value(proof.clone()).map_err(|e| format!("bad eth_getProof: {e}: {proof}"))
}

/// Header `stateRoot` of `block`.
pub fn state_root(block: u64) -> Result<alloy_primitives::B256, String> {
    let header = rpc(
        "eth_getBlockByNumber",
        json!([format!("0x{block:x}"), false]),
    )?;
    serde_json::from_value(header["stateRoot"].clone())
        .map_err(|e| format!("bad stateRoot: {e}: {header}"))
}

pub fn block_hash(number: u64) -> Result<Option<String>, String> {
    let block = rpc(
        "eth_getBlockByNumber",
        json!([format!("0x{number:x}"), false]),
    )?;
    Ok(block
        .get("hash")
        .and_then(Value::as_str)
        .map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_fill_missing_fields() {
        // Seeds are recorded for the driver; guest randomness comes from Bedrock.
        let c: Config = serde_json::from_str(r#"{"seed": 7, "liveness_secs": 9}"#).unwrap();
        assert_eq!(c.liveness_secs, 9);
        assert_eq!(c.run_secs, 120);
        assert_eq!(c.nemesis, NemesisConfig::default());
        assert_eq!(c.trie, None);
    }
}
