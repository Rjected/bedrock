// SPDX-License-Identifier: GPL-2.0

//! Shared plumbing: run config, the assertion and event sinks, guest time, and
//! node RPC.
//!
//! Everything here must stay deterministic under Bedrock: time comes from the
//! guest's emulated clock, and randomness (`rand`, seeded via getrandom) from
//! Bedrock's controlled getrandom stream.

use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256};
use bedrock_assertions::{Assertion, Condition, Location};
use nix::time::{clock_gettime, ClockId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const NODE_CONTAINER: &str = "tempo";
const RPC_URL: &str = "http://127.0.0.1:8545";
/// Overrides [`RPC_URL`], for running checks against a node outside Bedrock.
const RPC_URL_ENV: &str = "TEMPO_DST_RPC";
/// E7 reference node (compose service `tempo-ref`, `--reference`).
pub const REFERENCE_CONTAINER: &str = "tempo-ref";
pub const REFERENCE_RPC_URL: &str = "http://127.0.0.1:8547";
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

/// One kill of an explicit [`Config::nemesis_plan`], as the nemesis logs it
/// in its `plan` event.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlannedKill {
    /// Seconds after nemesis start.
    pub at_secs: u64,
    pub down_secs: u64,
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
    /// Explicit per-generation inputs, indexed by load generation (0 at
    /// branch start, one more per node restart). A generation past the end
    /// is derived ([`Config::load_generation`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub generations: Vec<LoadGeneration>,
    /// A derived generation draws its spec seed from getrandom (Bedrock's
    /// controlled stream, so it is on the input tape) instead of using the
    /// run seed. See [`Config::draw_load_generation`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub draw_spec_seeds: bool,
}

/// The inputs of one load generation: everything its txgen run and generated
/// spec (`trie_gen`, `tip20`, `cob_gen`) are a function of, besides the
/// load's count and the trie slots.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LoadGeneration {
    /// `TXGEN_SEED` (txgen's own transaction randomness).
    pub txgen_seed: u64,
    /// Seed of the generated spec (the run seed when derived).
    pub spec_seed: u64,
}

/// A RawStorage contract whose storage root E5 checks against an
/// independent reference at every block.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct TrieConfig {
    pub address: String,
    /// Slots the oracles read and prove. Empty: generated from the run seed
    /// (`trie_gen::slots`), along with the load spec.
    pub slots: Vec<u64>,
    /// The load spec comes from `trie_gen` over explicit `slots` (a
    /// scenario's recorded slots), instead of `load.spec`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub generated: bool,
}

impl TrieConfig {
    /// Whether the load spec (and, without explicit slots, the slots) come
    /// from `trie_gen`.
    pub fn generated(&self) -> bool {
        self.generated || self.slots.is_empty()
    }
}

/// A ChainOfBlocks contract whose hash chain E9 checks (see `cob`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct CobConfig {
    pub address: String,
}

impl Config {
    /// The trie config with generated slots filled in.
    pub fn trie(&self) -> Option<TrieConfig> {
        let mut trie = self.trie.clone()?;
        if trie.slots.is_empty() {
            trie.slots = crate::trie_gen::slots(self.seed);
        }
        Some(trie)
    }

    /// Load generation `generation`'s inputs: explicit in
    /// `load.generations`, else derived from the run (txgen seed
    /// `load.seed + generation`, spec seed the run seed).
    pub fn load_generation(&self, generation: u64) -> LoadGeneration {
        usize::try_from(generation)
            .ok()
            .and_then(|g| self.load.generations.get(g).copied())
            .unwrap_or(LoadGeneration {
                txgen_seed: self.load.seed + generation,
                spec_seed: self.seed,
            })
    }

    /// [`load_generation`](Self::load_generation) for the generation being
    /// started: with `load.draw_spec_seeds`, a derived generation's spec seed
    /// is drawn now from getrandom (served by Bedrock and recorded on the
    /// input tape under this process's pid). Call once per generation.
    pub fn draw_load_generation(&self, generation: u64) -> LoadGeneration {
        let mut inputs = self.load_generation(generation);
        let explicit = usize::try_from(generation).is_ok_and(|g| g < self.load.generations.len());
        if self.load.draw_spec_seeds && !explicit {
            let mut buf = [0u8; 8];
            let read = std::fs::File::open("/dev/urandom")
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf));
            match read {
                Ok(()) => inputs.spec_seed = u64::from_le_bytes(buf),
                Err(e) => eprintln!("draw spec seed: {e}; using the run seed"),
            }
        }
        inputs
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// The branch's seed (the driver's `--seeds` index).
    pub seed: u64,
    /// Virtual seconds the branch runs before finalize.
    pub run_secs: u64,
    pub nemesis: NemesisConfig,
    /// Guest seconds the head may stall before liveness fails.
    pub liveness_secs: u64,
    pub load: LoadConfig,
    pub trie: Option<TrieConfig>,
    /// TIP-20 load (`tip20::spec`) and its E8 oracle.
    pub tip20: bool,
    /// Chain-of-blocks load and E9.
    pub cob: Option<CobConfig>,
    /// The E7 reference node runs (compose service `tempo-ref`).
    pub reference: bool,
    /// Explicit kill schedule (driver-made decisions, `bedrock-dst replay
    /// --scenario`). Absent: drawn from Bedrock-controlled randomness
    /// (`nemesis::plan`) per `nemesis`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nemesis_plan: Option<Vec<PlannedKill>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            seed: 0,
            run_secs: 120,
            nemesis: NemesisConfig::default(),
            liveness_secs: 60,
            load: LoadConfig::default(),
            trie: None,
            tip20: false,
            cob: None,
            reference: false,
            nemesis_plan: None,
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

    /// Rejects an explicit nemesis plan whose kills overlap or go backwards
    /// (the nemesis sleeps from one restart to the next kill).
    pub fn validate(&self) -> Result<(), String> {
        let mut free = 0;
        for (i, k) in self.nemesis_plan.iter().flatten().enumerate() {
            if k.at_secs < free {
                return Err(format!(
                    "nemesis_plan[{i}] at {}s is before the previous restart at {free}s",
                    k.at_secs
                ));
            }
            free = k.at_secs + k.down_secs;
        }
        Ok(())
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

thread_local! {
    /// `None`: the primary node ([`RPC_URL`], or `TEMPO_DST_RPC`).
    static RPC_TARGET: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// Runs `f` with this thread's node queries sent to `url` instead of the
/// primary node (E7 asks the reference node the same questions).
pub fn with_rpc_url<T>(url: &'static str, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<&'static str>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RPC_TARGET.set(self.0);
        }
    }
    let _restore = Restore(RPC_TARGET.replace(Some(url)));
    f()
}

/// This thread's node RPC endpoint (see [`with_rpc_url`]).
fn rpc_url() -> String {
    match RPC_TARGET.get() {
        Some(url) => url.into(),
        None => std::env::var(RPC_URL_ENV).unwrap_or_else(|_| RPC_URL.into()),
    }
}

fn rpc(method: &str, params: Value) -> Result<Value, String> {
    let resp: Value = ureq::post(&rpc_url())
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

/// Number of transactions in `block`.
pub fn tx_count(block: u64) -> Result<u64, String> {
    let n = rpc(
        "eth_getBlockTransactionCountByNumber",
        json!([format!("0x{block:x}")]),
    )?;
    serde_json::from_value::<alloy_primitives::U64>(n.clone())
        .map(|n| n.to())
        .map_err(|e| format!("bad transaction count: {e}: {n}"))
}

/// Whether `address` has code at the latest block.
pub fn has_code(address: &str) -> Result<bool, String> {
    let code = rpc("eth_getCode", json!([address, "latest"]))?;
    Ok(code.as_str().is_some_and(|c| c.len() > 2))
}

/// reth's `eth_getMultiProof` for `slots` of `address` at `block`.
pub fn multiproof(
    address: &str,
    slots: &[u64],
    block: u64,
) -> Result<crate::trie_ref::AccountProof, String> {
    let keys: Vec<String> = slots.iter().map(|s| format!("0x{s:064x}")).collect();
    let proofs = rpc(
        "eth_getMultiProof",
        json!([[[address, keys]], format!("0x{block:x}")]),
    )?;
    serde_json::from_value(proofs[0].clone())
        .map_err(|e| format!("bad eth_getMultiProof: {e}: {proofs}"))
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

/// `(hash, parent hash)` of canonical block `number`; `None` past the head.
pub fn block_ref(number: u64) -> Result<Option<(B256, B256)>, String> {
    let block = rpc(
        "eth_getBlockByNumber",
        json!([format!("0x{number:x}"), false]),
    )?;
    if block.is_null() {
        return Ok(None);
    }
    let field = |name: &str| {
        serde_json::from_value::<B256>(block[name].clone())
            .map_err(|e| format!("bad block {name}: {e}: {block}"))
    };
    Ok(Some((field("hash")?, field("parentHash")?)))
}

/// `eth_call`s of `(to, data)` against the state of block `hash`, which must
/// still be canonical (EIP-1898), in one JSON-RPC batch.
pub fn calls_at(calls: &[(Address, Vec<u8>)], hash: B256) -> Result<Vec<Bytes>, String> {
    let batch: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(id, (to, data))| {
            json!({"jsonrpc": "2.0", "id": id, "method": "eth_call", "params": [
                {"to": to, "data": Bytes::copy_from_slice(data)},
                {"blockHash": hash, "requireCanonical": true}
            ]})
        })
        .collect();
    let resp: Vec<Value> = ureq::post(&rpc_url())
        .timeout(Duration::from_secs(10))
        .send_json(Value::Array(batch))
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;
    let mut out = vec![None; calls.len()];
    for r in resp {
        if let Some(err) = r.get("error") {
            return Err(err.to_string());
        }
        let id = r["id"]
            .as_u64()
            .map(|i| i as usize)
            .filter(|i| *i < out.len());
        let id = id.ok_or_else(|| format!("bad batch response: {r}"))?;
        out[id] = Some(
            serde_json::from_value(r["result"].clone())
                .map_err(|e| format!("bad eth_call result: {e}: {r}"))?,
        );
    }
    out.into_iter()
        .map(|r| r.ok_or_else(|| "missing batch response".to_string()))
        .collect()
}

/// One receipt log.
#[derive(Debug, Clone, Deserialize)]
pub struct ReceiptLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// Logs in block `hash`'s receipts, in order (`eth_getBlockReceipts`). A
/// reverted Tempo transaction keeps its fee-payment logs, so receipts are not
/// filtered by status: logs a revert should have discarded are kept too.
pub fn block_logs(hash: B256) -> Result<Vec<ReceiptLog>, String> {
    #[derive(Deserialize)]
    struct Receipt {
        logs: Vec<ReceiptLog>,
    }
    let receipts = rpc("eth_getBlockReceipts", json!([hash]))?;
    if receipts.is_null() {
        return Err(format!("no receipts for block {hash}"));
    }
    let receipts: Vec<Receipt> = serde_json::from_value(receipts.clone())
        .map_err(|e| format!("bad eth_getBlockReceipts: {e}: {receipts}"))?;
    Ok(receipts.into_iter().flat_map(|r| r.logs).collect())
}

/// The 32-byte storage word at `slot` of `address` at `block`.
pub fn storage_word(
    address: &str,
    slot: alloy_primitives::B256,
    block: u64,
) -> Result<alloy_primitives::B256, String> {
    let value = rpc(
        "eth_getStorageAt",
        json!([address, slot, format!("0x{block:x}")]),
    )?;
    serde_json::from_value::<alloy_primitives::U256>(value.clone())
        .map(alloy_primitives::B256::from)
        .map_err(|e| format!("bad eth_getStorageAt: {e}: {value}"))
}

/// One `eth_getLogs` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub block: u64,
    pub block_hash: String,
    pub data: Vec<u8>,
}

/// Logs of `address` with first topic `topic` in blocks `from..=to`, in
/// chain order.
pub fn logs(
    address: &str,
    topic: alloy_primitives::B256,
    from: u64,
    to: u64,
) -> Result<Vec<Log>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        block_number: alloy_primitives::U64,
        block_hash: String,
        data: alloy_primitives::Bytes,
    }
    let logs = rpc(
        "eth_getLogs",
        json!([{
            "address": address,
            "topics": [topic],
            "fromBlock": format!("0x{from:x}"),
            "toBlock": format!("0x{to:x}"),
        }]),
    )?;
    let raw: Vec<Raw> = serde_json::from_value(logs.clone())
        .map_err(|e| format!("bad eth_getLogs: {e}: {logs}"))?;
    Ok(raw
        .into_iter()
        .map(|r| Log {
            block: r.block_number.to(),
            block_hash: r.block_hash.to_lowercase(),
            data: r.data.to_vec(),
        })
        .collect())
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
        assert!(!c.tip20);
    }
}
