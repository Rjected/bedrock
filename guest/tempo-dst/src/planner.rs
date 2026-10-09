// SPDX-License-Identifier: GPL-2.0

//! The workload planner: the host side of the bedrock-dst workload contract
//! (`bedrock-dst-contract`), for the Tempo workload.
//!
//! `bedrock-dst` runs this binary on the host as `--workload-planner` (never
//! in the guest, so planning cannot perturb guest time):
//!
//! ```text
//! tempo-dst parse-args -  # ArgsRequest            -> TempoArgs
//! tempo-dst config -      # PlanRequest            -> Plan<Config, scenario, Swarm>
//! tempo-dst observe -     # ObserveRequest         -> Observation<scenario>
//! ```
//!
//! (requests on stdin, one JSON answer on stdout). All are pure functions of
//! their input: the same campaign, arguments, seed and derivation salt give
//! byte-identical output. Everything Tempo-specific about a campaign lives
//! here: the load kinds, their images, contracts and rates, the nemesis kill
//! plan, the per-generation load seeds, the guest config, the decisions a
//! branch re-draws, and the coverage a seed must reach. See
//! workloads/tempo-dst/README.md ("Workload contract").

use std::collections::BTreeMap;

use bedrock_dst_contract::{
    ArgsRequest, CampaignInfo, Moment, Observation, ObserveRequest, Plan, PlanRequest, Preempt,
    RunParams, Signature,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::common::{
    CobConfig, Config, LoadConfig, LoadGeneration, NemesisConfig, PlannedKill, TrieConfig,
};
use crate::nemesis::{self, MAX_DOWN_SECS, WARMUP_SECS};
use crate::scenario::{
    self, Decisions, Generation, Kill, LoadScenario, NemesisScenario, Observed, Scenario,
};

/// Salt of the planner-made nemesis plan (`decisions=explicit`).
const NEMESIS_SALT: u64 = 0x6e65_6d65_7369_7331; // "nemesis1"
/// Salt of a branch's re-drawn decisions.
const BRANCH_DECISION_SALT: u64 = 0x6272_616e_6368_6431; // "branchd1"

/// Trie load image (workloads/tempo-dst/trie).
pub const TRIE_IMAGE: &str = "bedrock/tempo-dst-trie:latest";
/// RawStorage deployed by trie/deploy.yaml during warmup: dev account 0's
/// first transaction on a fresh chain (CREATE address of nonce 0).
pub const TRIE_CONTRACT: &str = "0x5fbdb2315678afecb367f032d93f642f64180aa3";
/// TIP-20 load image (workloads/tempo-dst/tip20); its token is created and
/// minted during warmup.
pub const TIP20_IMAGE: &str = "bedrock/tempo-dst-tip20:latest";
/// Chain-of-blocks load image (workloads/tempo-dst/chain).
pub const CHAIN_IMAGE: &str = "bedrock/tempo-dst-chain:latest";
/// ChainOfBlocks deployed by chain/deploy.yaml during warmup: dev account 9's
/// first transaction.
pub const CHAIN_CONTRACT: &str = "0x700b6a60ce7eaaea56f065753d8dcb9653dbad35";
/// Nemesis warmup before the first kill plus its longest downtime.
const NEMESIS_SLACK_SECS: u64 = WARMUP_SECS + MAX_DOWN_SECS;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Load {
    /// pathUSD transfers (txgen).
    #[default]
    Transfers,
    /// RawStorage writes over slot shapes generated per seed (E5/E6).
    Trie,
    /// Transfers among a closed set of holders of a fresh TIP-20 (E8).
    Tip20,
    /// ChainOfBlocks appends (E9).
    Chain,
}

/// One seed's swarm record (the contract's `S`): every feature that shapes
/// the run, so verdicts can be grouped by feature. In config.json,
/// scenario.json and verdict.json. Fields in alphabetical order (see
/// [`LoadConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swarm {
    /// Campaign-wide, recorded so a verdict names its whole configuration.
    pub load: Load,
    pub nemesis: bool,
    /// Drawn per seed by the driver.
    pub preempt: Preempt,
    pub reference: bool,
}

/// The Tempo workload's arguments (the contract's `Args`; `bedrock-dst
/// campaign --workload-arg key=value`, e.g. `load=trie`). Defaults are
/// those of the driver flags they replace; unknown keys are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TempoArgs {
    /// txgen's transaction-generation seed, shared by every seed so seeds
    /// vary faults and schedules over a fixed workload.
    pub workload_seed: u64,
    pub max_kills: u32,
    pub min_gap_secs: u32,
    /// Disable the crash nemesis.
    pub no_nemesis: bool,
    /// E2 head-stall budget.
    pub liveness_secs: u64,
    /// Transfers for the `transfers` load (0 disables load).
    pub txgen_count: u64,
    pub txgen_tps: u64,
    pub load: Load,
    /// Trie load rate; ~5 tx/s puts about one step in each 200 ms block.
    pub trie_tps: u64,
    /// TIP-20 load rate; ~50 tx/s puts about ten transfers in each block.
    pub tip20_tps: u64,
    /// Chain load rate; ~10 tx/s puts about two appends in each block.
    pub chain_tps: u64,
    /// Run the E7 reference node (compose service `tempo-ref`; run.sh keeps
    /// it in the compose file only with this flag).
    pub reference: bool,
    /// Who makes the nemesis plan and per-generation load seeds: `guest`
    /// (drawn in the guest, read back by `observe`) or `explicit` (derived
    /// here from the seed and delivered in config.json, so `replay
    /// --scenario` is byte-identical).
    pub decisions: Decisions,
    /// Draw each generated load's spec seed from getrandom in the guest
    /// instead of using the run seed.
    pub spec_seeds_from_getrandom: bool,
}

impl Default for TempoArgs {
    fn default() -> Self {
        Self {
            workload_seed: 99,
            max_kills: 3,
            min_gap_secs: 30,
            no_nemesis: false,
            liveness_secs: 60,
            txgen_count: 20_000,
            txgen_tps: 100,
            load: Load::Transfers,
            trie_tps: 5,
            tip20_tps: 50,
            chain_tps: 10,
            reference: false,
            decisions: Decisions::Guest,
            spec_seeds_from_getrandom: false,
        }
    }
}

/// This planner's instance of the contract types.
pub type TempoPlanRequest = PlanRequest<TempoArgs, Scenario>;
/// Decisions go out as pretty JSON, so the driver's scenario.json (which
/// it writes verbatim) stays readable.
pub type TempoPlan = Plan<Config, Box<RawValue>, Swarm>;

pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// `s` as pretty JSON.
fn pretty<T: Serialize>(s: &T) -> Result<Box<RawValue>, String> {
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    RawValue::from_string(text).map_err(|e| e.to_string())
}

/// `tempo-dst parse-args`: `--workload-config` (a JSON object of
/// [`TempoArgs`] fields) with each `KEY=VALUE` over it. A value is taken as
/// JSON when it parses as JSON (`2`, `true`, `[1]`), else as a string
/// (`trie`); [`TempoArgs`] then checks every key and type.
pub fn parse_args(req: &ArgsRequest) -> Result<TempoArgs, String> {
    let mut fields: BTreeMap<String, Box<RawValue>> = match &req.config_file {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            serde_json::from_str(&text).map_err(|e| format!("{path}: want a JSON object: {e}"))?
        }
        None => BTreeMap::new(),
    };
    for kv in &req.args {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("workload arg {kv:?}: want KEY=VALUE"))?;
        let value = match RawValue::from_string(v.to_string()) {
            Ok(json) => json,
            Err(_) => serde_json::value::to_raw_value(v).map_err(|e| e.to_string())?,
        };
        fields.insert(k.to_string(), value);
    }
    let text = serde_json::to_string(&fields).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("workload args: {e}"))
}

/// The guest nemesis planner (`nemesis::plan`) for `decisions=explicit`,
/// drawn from a host PRNG of the seed instead of guest randomness.
pub fn nemesis_plan_for(wa: &TempoArgs, run_secs: u64, seed: u64, salt: u64) -> Vec<Kill> {
    let mut state = splitmix64(seed ^ salt);
    let draw = |lo: u64, hi: u64| {
        state = splitmix64(state);
        lo + state % (hi - lo + 1)
    };
    let cfg = NemesisConfig {
        enabled: !wa.no_nemesis,
        max_kills: wa.max_kills,
        min_gap_secs: wa.min_gap_secs,
    };
    nemesis::plan(&cfg, run_secs, draw)
        .into_iter()
        .map(|k| Kill {
            at_secs: k.at_secs,
            down_secs: k.down_secs,
        })
        .collect()
}

/// `(count, tps, image)` of the campaign's load.
fn load_params(wa: &TempoArgs, run_secs: u64) -> (u64, u64, Option<&'static str>) {
    // Generated loads get enough steps to outlast the run.
    let steps = |tps| (run_secs + 60) * tps;
    match wa.load {
        Load::Transfers => (wa.txgen_count, wa.txgen_tps, None),
        Load::Trie => (steps(wa.trie_tps), wa.trie_tps, Some(TRIE_IMAGE)),
        Load::Tip20 => (steps(wa.tip20_tps), wa.tip20_tps, Some(TIP20_IMAGE)),
        Load::Chain => (steps(wa.chain_tps), wa.chain_tps, Some(CHAIN_IMAGE)),
    }
}

/// Every decision of `seed`'s run. With `decisions=guest` the nemesis plan
/// and load seeds are left to the guest (filled in by `observe`).
pub fn scenario_for(
    c: &CampaignInfo,
    wa: &TempoArgs,
    seed: u64,
    preempt: Preempt,
    salt: Option<u64>,
) -> Scenario {
    let (count, tps, image) = load_params(wa, c.run_secs);
    let explicit = wa.decisions == Decisions::Explicit;
    let nemesis_salt = NEMESIS_SALT ^ salt.unwrap_or(0);
    let plan = explicit.then(|| nemesis_plan_for(wa, c.run_secs, seed, nemesis_salt));
    // One generation at start plus one per restart, each as the guest would
    // derive it (so explicit decisions do not change the load).
    let generations = match &plan {
        Some(plan) if !wa.spec_seeds_from_getrandom => (0..=plan.len() as u64)
            .map(|g| Generation {
                txgen_seed: wa.workload_seed + g,
                spec_seed: seed,
            })
            .collect(),
        _ => Vec::new(),
    };
    Scenario {
        version: scenario::SCENARIO_VERSION,
        decisions: wa.decisions,
        seed,
        rng_seed: seed,
        swarm: Swarm {
            load: wa.load,
            nemesis: !wa.no_nemesis,
            preempt,
            reference: wa.reference,
        },
        run_secs: c.run_secs,
        liveness_secs: wa.liveness_secs,
        nemesis: NemesisScenario {
            enabled: !wa.no_nemesis,
            max_kills: wa.max_kills,
            min_gap_secs: wa.min_gap_secs,
            plan,
        },
        load: LoadScenario {
            kind: wa.load,
            count,
            tps,
            image: image.map(Into::into),
            workload_seed: wa.workload_seed,
            draw_spec_seeds: wa.spec_seeds_from_getrandom,
            generations,
        },
        observed: Observed::default(),
        meaning: scenario::meaning(),
    }
}

/// The guest's `/bedrock/in/config.json` for scenario `s`. `deliver`: pass
/// the nemesis plan and per-generation load inputs explicitly (planner-made
/// decisions, scenario replays); otherwise the guest derives them.
pub fn guest_config(s: &Scenario, deliver: bool) -> Config {
    let kind = s.load.kind;
    Config {
        cob: (kind == Load::Chain).then(|| CobConfig {
            address: CHAIN_CONTRACT.into(),
        }),
        liveness_secs: s.liveness_secs,
        load: LoadConfig {
            count: s.load.count,
            draw_spec_seeds: s.load.draw_spec_seeds,
            generations: if deliver {
                s.load
                    .generations
                    .iter()
                    .map(|g| LoadGeneration {
                        spec_seed: g.spec_seed,
                        txgen_seed: g.txgen_seed,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            image: s.load.image.clone().unwrap_or_default(),
            seed: s.load.workload_seed,
            spec: String::new(),
            tps: s.load.tps,
        },
        nemesis: NemesisConfig {
            enabled: s.nemesis.enabled,
            max_kills: s.nemesis.max_kills,
            min_gap_secs: s.nemesis.min_gap_secs,
        },
        nemesis_plan: s.nemesis.plan.as_ref().filter(|_| deliver).map(|plan| {
            plan.iter()
                .map(|k| PlannedKill {
                    at_secs: k.at_secs,
                    down_secs: k.down_secs,
                })
                .collect()
        }),
        reference: s.swarm.reference,
        run_secs: s.run_secs,
        seed: s.seed,
        swarm: Some(s.swarm),
        tip20: kind == Load::Tip20,
        trie: (kind == Load::Trie).then(|| TrieConfig {
            address: TRIE_CONTRACT.into(),
            ..TrieConfig::default()
        }),
        workload_seed: s.load.workload_seed,
    }
}

/// Whether every run of `s` has at least one kill and its restart: the first
/// kill lands by warmup + one gap, and needs its downtime plus a full gap
/// after it for recovery (see `nemesis::plan`).
fn kills_planned(s: &Scenario) -> bool {
    s.nemesis.enabled
        && s.nemesis.max_kills > 0
        && s.run_secs >= 2 * u64::from(s.nemesis.min_gap_secs.max(1)) + NEMESIS_SLACK_SECS
}

/// Coverage every run of `s` must reach (see bedrock-dst's `verdict`): the
/// load landed, and its oracle checked it. With the trie load, its checks
/// ran. With the TIP-20 load: blocks carried several transfers, and a
/// snapshot pinned while in memory read the same after it was persisted and,
/// when the plan guarantees a kill, after a restart. With the chain load, an
/// append made during the run was checked against state and, when the run
/// kills the node, the chain grew past its pre-kill length after a restart.
/// With `reference`, the reference node caught up and was compared.
pub fn required(s: &Scenario) -> Vec<Signature> {
    let mut required = match s.load.kind {
        Load::Transfers if s.load.count == 0 => vec![],
        Load::Transfers => vec!["S/load-included"],
        Load::Trie => vec!["S/load-included", "S/trie-checked"],
        Load::Tip20 => {
            let mut r = vec![
                "S/load-included",
                "S/tip20-checked",
                "S/tip20-transfers-in-block",
                "S/tip20-pinned-read-survived-persistence",
            ];
            if kills_planned(s) {
                r.push("S/tip20-pinned-read-survived-restart");
            }
            r
        }
        Load::Chain if kills_planned(s) => vec![
            "S/load-included",
            "S/chain-appended",
            "S/chain-survived-restart",
        ],
        Load::Chain => vec!["S/load-included", "S/chain-appended"],
    };
    if s.swarm.reference {
        required.extend(["S/reference-compared", "S/reference-synced"]);
    }
    required.into_iter().map(Signature::from).collect()
}

/// A branch's decisions: the parent's up to the moment, re-drawn after it;
/// the argument of `tempo-dst redecide`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Redraw {
    pub nemesis_plan: Vec<Kill>,
    pub generations: Vec<Generation>,
    /// How many of each come from the parent.
    #[serde(skip)]
    pub kept_kills: usize,
    #[serde(skip)]
    pub kept_generations: usize,
}

/// Re-draws the decisions of `parent` not yet applied `t_secs` after its
/// nemesis started: kills that fired before stay, as do the load generations
/// started before (generation 0, and one per restart done); the rest of the
/// plan is drawn like the guest planner would, starting after the moment,
/// from `seed`, with fresh txgen and spec seeds for the later generations.
/// The guest has the last word on what already happened
/// (`tempo-dst redecide` keeps every kill that fired).
pub fn redraw_decisions(parent: &Scenario, t_secs: f64, seed: u64) -> Redraw {
    let plan0 = parent.nemesis.plan.clone().unwrap_or_default();
    let mut plan: Vec<Kill> = plan0
        .iter()
        .take_while(|k| (k.at_secs as f64) < t_secs)
        .copied()
        .collect();
    let kept_kills = plan.len();
    let restarted = plan
        .iter()
        .filter(|k| ((k.at_secs + k.down_secs) as f64) < t_secs)
        .count();
    let mut state = splitmix64(seed ^ BRANCH_DECISION_SALT);
    let mut next = || {
        state = splitmix64(state);
        state
    };
    let n = &parent.nemesis;
    if n.enabled {
        let gap = u64::from(n.min_gap_secs.max(1));
        let after = t_secs.ceil() as u64 + 1;
        let mut t = plan
            .last()
            .map_or(WARMUP_SECS, |k| k.at_secs + k.down_secs + gap)
            .max(after);
        while plan.len() < n.max_kills as usize {
            let at = t + next() % (gap + 1);
            let down = next() % (MAX_DOWN_SECS + 1);
            if at + down + gap > parent.run_secs {
                break;
            }
            plan.push(Kill {
                at_secs: at,
                down_secs: down,
            });
            t = at + down + gap;
        }
    }
    let gens0 = &parent.load.generations;
    let kept_generations = if gens0.is_empty() {
        0
    } else {
        gens0.len().min(1 + restarted)
    };
    let mut generations = gens0[..kept_generations].to_vec();
    if !gens0.is_empty() {
        for _ in kept_generations..=plan.len() {
            generations.push(Generation {
                txgen_seed: next() >> 32,
                spec_seed: next(),
            });
        }
    }
    Redraw {
        nemesis_plan: plan,
        generations,
        kept_kills,
        kept_generations,
    }
}

/// The plan of `req` with the decisions as a typed scenario.
pub fn plan(req: &TempoPlanRequest) -> Result<Plan<Config, Scenario, Swarm>, String> {
    let mut notes = Vec::new();
    let mut action = None;
    let (mut s, config) = match (&req.decisions, req.redraw) {
        (None, Some(_)) => return Err("redraw needs the parent's decisions".into()),
        (None, None) => {
            let s = scenario_for(
                &req.campaign,
                &req.args,
                req.seed,
                req.preempt,
                req.derivation_salt,
            );
            let deliver = s.decisions == Decisions::Explicit;
            let config = guest_config(&s, deliver);
            (s, config)
        }
        // Recorded decisions: delivered as they are, nothing derived.
        (Some(recorded), None) => (recorded.clone(), guest_config(recorded, true)),
        (Some(parent), Some(r)) => {
            let d = redraw_decisions(parent, r.after.0, r.seed);
            notes.push(format!(
                "decisions after the moment re-drawn: kills {:?} (kept {}), generations {:?} \
                 (kept {})",
                d.nemesis_plan, d.kept_kills, d.generations, d.kept_generations
            ));
            if Some(&d.nemesis_plan) == parent.nemesis.plan.as_ref()
                && d.generations == parent.load.generations
            {
                notes.push(
                    "nothing is left to re-draw after this moment (the next kill would not \
                     fit the run); only the randomness varies"
                        .into(),
                );
            }
            action = Some(format!(
                "redecide '{}'",
                serde_json::to_string(&d).map_err(|e| e.to_string())?
            ));
            // What the guest applied, read back from its events.
            let mut s = parent.clone();
            s.nemesis.plan = None;
            s.load.generations.clear();
            (s, guest_config(parent, true))
        }
    };
    if let Some(rng) = req.rng_seed {
        s.rng_seed = rng;
    }
    Ok(Plan {
        config,
        required: required(&s),
        replays_exactly: s.replays_exactly(),
        run: RunParams {
            seed: s.seed,
            rng_seed: s.rng_seed,
            run_secs: s.run_secs,
            preempt: s.swarm.preempt,
            swarm: s.swarm,
        },
        decisions: s,
        action,
        notes,
    })
}

/// `tempo-dst config`.
pub fn config(req: &TempoPlanRequest) -> Result<TempoPlan, String> {
    let p = plan(req)?;
    Ok(Plan {
        decisions: pretty(&p.decisions)?,
        config: p.config,
        run: p.run,
        required: p.required,
        replays_exactly: p.replays_exactly,
        action: p.action,
        notes: p.notes,
    })
}

/// Moments of a run from its events: nemesis kills and restarts, load
/// generations, re-decisions, each Always signature's first failure, 5 s
/// before it, and its last pass before it.
pub fn moments(events: &[crate::common::DstEvent]) -> Vec<Moment> {
    let mut out = Vec::new();
    for e in events {
        let ns = e.guest_time_ns as i64;
        let d = &e.detail;
        let node = e.container.as_deref() == Some(crate::common::NODE_CONTAINER);
        let mut push = |ns: i64, what: String| {
            out.push(Moment {
                guest_time_ns: ns,
                what,
            })
        };
        match (e.source.as_str(), e.kind.as_str()) {
            ("nemesis", "kill") if node => push(ns, format!("nemesis kill #{}", d["index"])),
            ("nemesis", "restart") if node => {
                push(ns, format!("nemesis restart #{}", d["index"]));
            }
            ("nemesis", "redecide") => push(ns, "decisions re-drawn (branch)".into()),
            ("load", "generation") if d["generation"].as_u64() != Some(0) => {
                push(ns, format!("load generation {}", d["generation"]));
            }
            ("assertion", "first-failure") => {
                let sig = d["signature"].as_str().unwrap_or("?");
                push(ns, format!("first failure: {sig}"));
                push(
                    ns - 5_000_000_000,
                    format!("5 s before first failure: {sig}"),
                );
                if let Some(p) = d["last_pass_ns"].as_u64() {
                    push(p as i64, format!("last pass before failure: {sig}"));
                }
            }
            _ => {}
        }
    }
    out
}

/// The events of an events.jsonl (unparsable lines skipped).
fn parse_events(text: &str) -> Vec<crate::common::DstEvent> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// `observe` with the decisions as a typed scenario.
pub fn observe_typed(req: &ObserveRequest<Scenario>, events: &str) -> Observation<Scenario> {
    let mut s = req.decisions.clone();
    s.record_observed(Observed::from_events(events));
    let decision_diff = req
        .original
        .as_ref()
        .map(|o| o.decision_diff(&s))
        .unwrap_or_default();
    let evs = parse_events(events);
    Observation {
        decisions: s,
        decision_diff,
        moments: moments(&evs),
        events_origin_ns: evs.first().map(|e| e.guest_time_ns),
    }
}

/// `tempo-dst observe`.
pub fn observe(req: &ObserveRequest<Scenario>) -> Result<Observation<Box<RawValue>>, String> {
    // A run that stopped under the driver has no events.
    let events = std::fs::read_to_string(&req.events_path).unwrap_or_default();
    let o = observe_typed(req, &events);
    Ok(Observation {
        decisions: pretty(&o.decisions)?,
        decision_diff: o.decision_diff,
        moments: o.moments,
        events_origin_ns: o.events_origin_ns,
    })
}

/// Reads a request: inline JSON, or `-` for stdin.
pub fn request<T: serde::de::DeserializeOwned>(arg: &str) -> Result<T, String> {
    let text = if arg == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(|e| e.to_string())?;
        s
    } else {
        arg.to_string()
    };
    serde_json::from_str(&text).map_err(|e| format!("bad request: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bedrock_dst_contract::{Redraw as RedrawReq, VirtSecs};
    use serde_json::{json, Value};
    use std::collections::BTreeSet;

    // Tests read the golden file, which is arbitrary JSON: `Value` here only.
    fn golden() -> Value {
        serde_json::from_str(include_str!("../testdata/planner-golden.json")).unwrap()
    }

    fn with_meaning(mut v: Value, meaning: &Value) -> Value {
        v["meaning"] = meaning.clone();
        v
    }

    fn request_of(case: &Value) -> TempoPlanRequest {
        PlanRequest {
            campaign: CampaignInfo {
                run_secs: case["campaign"]["run_secs"].as_u64().unwrap(),
                warm_blocks: case["campaign"]["warm_blocks"].as_u64().unwrap(),
                ..CampaignInfo::default()
            },
            args: serde_json::from_value(case["workload_args"].clone()).unwrap(),
            seed: case["seed"].as_u64().unwrap(),
            preempt: serde_json::from_value(case["preempt"].clone()).unwrap(),
            derivation_salt: case["derivation_salt"].as_u64(),
            decisions: None,
            redraw: None,
            rng_seed: None,
        }
    }

    fn recorded(s: Scenario) -> TempoPlanRequest {
        PlanRequest {
            campaign: CampaignInfo::default(),
            args: TempoArgs::default(),
            seed: 0,
            preempt: Preempt::default(),
            derivation_salt: None,
            decisions: Some(s),
            redraw: None,
            rng_seed: None,
        }
    }

    fn value<T: Serialize>(t: &T) -> Value {
        serde_json::to_value(t).unwrap()
    }

    /// The planner produces exactly what bedrock-dst produced before the
    /// workload contract (testdata generated from that driver): config.json
    /// bytes, scenario decisions and required coverage, live, after
    /// observing a run, on a scenario replay, and for re-drawn branches.
    #[test]
    fn planner_matches_the_pre_contract_driver() {
        let g = golden();
        let meaning = &g["meaning"];
        let cases = g["cases"].as_array().unwrap();
        assert!(cases.len() > 100);
        for case in cases {
            let name = format!("{} seed {}", case["args"], case["seed"]);
            // Live: derived from the seed.
            let out = config(&request_of(case)).unwrap();
            let live = &case["live"];
            assert_eq!(
                serde_json::to_string(&out.config).unwrap(),
                live["config"].as_str().unwrap(),
                "{name}: config"
            );
            let decisions: Scenario = serde_json::from_str(out.decisions.get()).unwrap();
            assert_eq!(
                value(&decisions),
                with_meaning(live["decisions"].clone(), meaning),
                "{name}: decisions"
            );
            assert_eq!(value(&out.required), live["required"], "{name}: required");
            assert_eq!(out.run.preempt, decisions.swarm.preempt);
            assert_eq!(
                (out.run.seed, out.run.rng_seed, out.run.run_secs),
                (decisions.seed, decisions.rng_seed, decisions.run_secs)
            );

            // Observing the run's events.
            let obs = observe_typed(
                &ObserveRequest {
                    decisions: decisions.clone(),
                    original: None,
                    events_path: String::new(),
                },
                case["events"].as_str().unwrap(),
            );
            let observed = with_meaning(case["observed"].clone(), meaning);
            assert_eq!(value(&obs.decisions), observed, "{name}: observed");

            // Scenario replay: the recorded decisions, nothing derived.
            let rec: Scenario = serde_json::from_value(observed.clone()).unwrap();
            let replay = plan(&recorded(rec.clone())).unwrap();
            let r = &case["replay"];
            assert_eq!(
                serde_json::to_string(&replay.config).unwrap(),
                r["config"].as_str().unwrap(),
                "{name}: replay config"
            );
            assert_eq!(value(&replay.required), r["required"], "{name}");
            assert_eq!(json!(replay.replays_exactly), r["replays_exactly"]);
            assert_eq!(value(&replay.decisions), observed);

            // Branch re-draws.
            for rd in case["redraws"].as_array().unwrap() {
                let mut req = recorded(rec.clone());
                req.redraw = Some(RedrawReq {
                    after: VirtSecs(rd["t_secs"].as_f64().unwrap()),
                    seed: rd["seed"].as_u64().unwrap(),
                });
                let b = plan(&req).unwrap();
                assert_eq!(
                    format!("tempo-dst {}", b.action.as_deref().unwrap()),
                    rd["command"].as_str().unwrap(),
                    "{name}: redraw"
                );
                assert_eq!(value(&b.required), rd["required"], "{name}");
                let mut want = rec.clone();
                want.nemesis.plan = None;
                want.load.generations.clear();
                assert_eq!(b.decisions, want);
            }
        }
    }

    #[test]
    fn workload_args_parse_typed_and_reject_unknown_keys() {
        let g = golden();
        let case = g["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["args"] == json!([]))
            .unwrap()
            .clone();
        // No arguments: the historical defaults.
        let defaults = parse_args(&ArgsRequest::default()).unwrap();
        assert_eq!(value(&defaults), case["workload_args"]);
        let a = parse_args(&ArgsRequest {
            config_file: None,
            args: vec![
                "load=trie".into(),
                "reference=true".into(),
                "max_kills=1".into(),
                "decisions=explicit".into(),
            ],
        })
        .unwrap();
        assert_eq!(a.load, Load::Trie);
        assert!(a.reference);
        assert_eq!(a.max_kills, 1);
        assert_eq!(a.decisions, Decisions::Explicit);
        let err = |args: &[&str]| {
            parse_args(&ArgsRequest {
                config_file: None,
                args: args.iter().map(|s| s.to_string()).collect(),
            })
            .unwrap_err()
        };
        assert!(err(&["lod=trie"]).contains("unknown field `lod`"));
        assert!(err(&["load=tree"]).contains("unknown variant"));
        assert!(err(&["max_kills=many"]).contains("invalid type"));
        assert!(err(&["load"]).contains("KEY=VALUE"));
        // A config file, with arguments over it.
        let dir = std::env::temp_dir().join(format!("tempo-dst-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("w.json");
        std::fs::write(&f, r#"{"load": "tip20", "tip20_tps": 7}"#).unwrap();
        let a = parse_args(&ArgsRequest {
            config_file: Some(f.to_string_lossy().into()),
            args: vec!["tip20_tps=9".into()],
        })
        .unwrap();
        assert_eq!((a.load, a.tip20_tps), (Load::Tip20, 9));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scenario_replay_ignores_a_changed_derivation() {
        let mut req = recorded(crate::scenario::tests::sample());
        req.decisions = None;
        req.seed = 6;
        req.args = TempoArgs {
            decisions: Decisions::Explicit,
            load: Load::Chain,
            ..Default::default()
        };
        // The original run: decisions derived, delivered, written out.
        let original = config(&req).unwrap();
        let delivered = serde_json::to_string(&original.config).unwrap();
        let file = original.decisions.get().to_string();

        // Later, the seed-to-decision derivation changes: re-deriving the
        // seed gives another run...
        let changed = TempoPlanRequest {
            derivation_salt: Some(0xbeef),
            ..req.clone()
        };
        let rederived = plan(&changed).unwrap();
        let orig: Scenario = serde_json::from_str(&file).unwrap();
        assert_ne!(rederived.decisions.nemesis.plan, orig.nemesis.plan);
        assert_ne!(serde_json::to_string(&rederived.config).unwrap(), delivered);
        // ...but the recorded decisions deliver exactly the original config,
        // without deriving anything (even under the changed derivation).
        let replayed = plan(&TempoPlanRequest {
            decisions: Some(orig),
            ..changed
        })
        .unwrap();
        assert_eq!(serde_json::to_string(&replayed.config).unwrap(), delivered);
        assert!(replayed.replays_exactly);

        // A guest-made scenario replays its observed plan explicitly...
        let guest = TempoPlanRequest {
            args: TempoArgs {
                load: Load::Chain,
                ..Default::default()
            },
            ..req.clone()
        };
        let live = plan(&guest).unwrap();
        assert!(live.config.nemesis_plan.is_none());
        let events = r#"{"source":"nemesis","kind":"plan","container":"tempo","guest_time_ns":1,"detail":{"kills":[{"at_secs":20,"down_secs":1}]}}"#;
        let obs = observe_typed(
            &ObserveRequest {
                decisions: live.decisions,
                original: None,
                events_path: String::new(),
            },
            events,
        );
        let replay = plan(&recorded(obs.decisions)).unwrap();
        assert_eq!(replay.config.nemesis_plan.unwrap()[0].at_secs, 20);
        assert!(!replay.replays_exactly);
        // A branch's fresh randomness is recorded in its decisions.
        let mut b = recorded(crate::scenario::tests::sample());
        b.rng_seed = Some(77);
        let p = plan(&b).unwrap();
        assert_eq!((p.run.rng_seed, p.decisions.rng_seed), (77, 77));
    }

    #[test]
    fn explicit_decisions_follow_the_guest_planner() {
        let wa = TempoArgs {
            decisions: Decisions::Explicit,
            load: Load::Trie,
            ..Default::default()
        };
        // Pinned, so a change to the derivation is noticed.
        let p = nemesis_plan_for(&wa, 180, 0, NEMESIS_SALT);
        assert_eq!(
            p.iter()
                .map(|k| (k.at_secs, k.down_secs))
                .collect::<Vec<_>>(),
            [(28, 3), (86, 0), (146, 3)]
        );
        for seed in 0..32 {
            let plan = nemesis_plan_for(&wa, 180, seed, NEMESIS_SALT);
            assert!(!plan.is_empty() && plan.len() <= 3, "{plan:?}");
            assert!(plan[0].at_secs >= WARMUP_SECS);
            for w in plan.windows(2) {
                assert!(w[1].at_secs >= w[0].at_secs + w[0].down_secs + 30);
            }
        }
        let quiet = TempoArgs {
            no_nemesis: true,
            ..wa
        };
        assert!(nemesis_plan_for(&quiet, 180, 0, NEMESIS_SALT).is_empty());
    }

    #[test]
    fn redraw_keeps_decisions_before_the_moment() {
        let mut p = crate::scenario::tests::sample();
        p.run_secs = 90;
        p.nemesis.min_gap_secs = 20;
        p.nemesis.plan = Some(vec![
            Kill {
                at_secs: 10,
                down_secs: 2,
            },
            Kill {
                at_secs: 40,
                down_secs: 1,
            },
        ]);
        p.load.generations = (0..3)
            .map(|g| Generation {
                txgen_seed: 99 + g,
                spec_seed: 3,
            })
            .collect();
        let plan0 = p.nemesis.plan.clone().unwrap();
        // 20 s in: kill 0 fired and restarted, so it and generations 0 and
        // 1 stay; everything after is re-drawn.
        let r = redraw_decisions(&p, 20.0, 1);
        assert_eq!((r.kept_kills, r.kept_generations), (1, 2));
        assert_eq!(r.nemesis_plan[0], plan0[0]);
        assert_eq!(r.generations[..2], p.load.generations[..2]);
        assert_eq!(r.generations.len(), r.nemesis_plan.len() + 1);
        for w in r.nemesis_plan.windows(2) {
            assert!(w[1].at_secs >= w[0].at_secs + w[0].down_secs + 20, "{r:?}");
        }
        for k in &r.nemesis_plan[1..] {
            assert!(k.at_secs > 20 && k.down_secs <= MAX_DOWN_SECS);
            assert!(k.at_secs + k.down_secs + 20 <= p.run_secs);
        }
        assert_eq!(redraw_decisions(&p, 20.0, 1), r);
        let others: BTreeSet<_> = (0..16)
            .map(|s| format!("{:?}", redraw_decisions(&p, 20.0, s).generations))
            .collect();
        assert!(others.len() > 8);
        let mid = redraw_decisions(&p, 11.0, 1);
        assert_eq!((mid.kept_kills, mid.kept_generations), (1, 1));
        let early = redraw_decisions(&p, 3.0, 2);
        assert_eq!((early.kept_kills, early.kept_generations), (0, 1));
        assert!(early.nemesis_plan.iter().all(|k| k.at_secs > 3));
        // After the last kill: nothing left to re-draw, and the planner says so.
        let late = redraw_decisions(&p, 80.0, 3);
        assert_eq!(late.nemesis_plan, plan0);
        assert_eq!(late.generations, p.load.generations);
        let mut req = recorded(p.clone());
        req.redraw = Some(RedrawReq {
            after: VirtSecs(80.0),
            seed: 3,
        });
        let out = plan(&req).unwrap();
        assert!(out.notes.iter().any(|n| n.contains("nothing is left")));
        assert!(out.action.unwrap().starts_with("redecide '{"));
        let mut quiet = p.clone();
        quiet.nemesis.enabled = false;
        quiet.nemesis.plan = Some(vec![]);
        quiet.load.generations.clear();
        let q = redraw_decisions(&quiet, 20.0, 4);
        assert!(q.nemesis_plan.is_empty() && q.generations.is_empty());
        // A redraw without the parent is an error.
        req.decisions = None;
        assert!(plan(&req).is_err());
    }

    #[test]
    fn observe_reports_decision_diffs_and_moments() {
        let s = crate::scenario::tests::sample();
        let s_ns = 1_000_000_000u64;
        let events = [
            format!(r#"{{"source":"load","kind":"generation","container":"txgen","guest_time_ns":{},"detail":{{"generation":0,"txgen_seed":99,"spec_seed":3,"steps":1}}}}"#, 1000 * s_ns),
            format!(r#"{{"source":"nemesis","kind":"plan","container":"tempo","guest_time_ns":{},"detail":{{"kills":[{{"at_secs":10,"down_secs":2}}]}}}}"#, 1000 * s_ns),
            format!(r#"{{"source":"nemesis","kind":"kill","container":"tempo","guest_time_ns":{},"detail":{{"index":0}}}}"#, 1010 * s_ns),
            format!(r#"{{"source":"nemesis","kind":"kill","container":"txgen","guest_time_ns":{},"detail":{{}}}}"#, 1012 * s_ns),
            format!(r#"{{"source":"nemesis","kind":"restart","container":"tempo","guest_time_ns":{},"detail":{{"index":0}}}}"#, 1012 * s_ns),
            format!(r#"{{"source":"load","kind":"generation","container":"txgen","guest_time_ns":{},"detail":{{"generation":1,"txgen_seed":100,"spec_seed":3,"steps":1}}}}"#, 1013 * s_ns),
            format!(r#"{{"source":"assertion","kind":"first-failure","guest_time_ns":{},"detail":{{"signature":"E5/storage-root-mismatch","last_pass_ns":{}}}}}"#, 1030 * s_ns, 1029 * s_ns),
        ]
        .join("\n");
        let req = |original| ObserveRequest {
            decisions: s.clone(),
            original,
            events_path: String::new(),
        };
        let out = observe_typed(&req(None), &events);
        let what: Vec<_> = out.moments.iter().map(|m| m.what.as_str()).collect();
        assert_eq!(
            what,
            [
                "nemesis kill #0",
                "nemesis restart #0",
                "load generation 1",
                "first failure: E5/storage-root-mismatch",
                "5 s before first failure: E5/storage-root-mismatch",
                "last pass before failure: E5/storage-root-mismatch",
            ]
        );
        assert_eq!(out.moments[0].guest_time_ns, 1010 * s_ns as i64);
        assert_eq!(out.moments[4].guest_time_ns, 1025 * s_ns as i64);
        assert_eq!(out.events_origin_ns, Some(1000 * s_ns));
        assert!(out.decision_diff.is_empty());
        // A replay that saw another plan differs from its original.
        let replay = observe_typed(
            &req(Some(out.decisions.clone())),
            &events.replace(r#""at_secs":10"#, r#""at_secs":11"#),
        );
        assert_eq!(replay.decision_diff.len(), 1, "{:?}", replay.decision_diff);
        assert!(replay.decision_diff[0].starts_with("nemesis plan"));
    }

    #[test]
    fn configs_never_contain_a_single_quote() {
        // `tempo-dst start '<config>'` relies on it.
        for load in [Load::Transfers, Load::Trie, Load::Tip20, Load::Chain] {
            for decisions in [Decisions::Guest, Decisions::Explicit] {
                let mut req = recorded(crate::scenario::tests::sample());
                req.decisions = None;
                req.args = TempoArgs {
                    load,
                    decisions,
                    reference: true,
                    ..Default::default()
                };
                let out = config(&req).unwrap();
                assert!(!serde_json::to_string(&out.config).unwrap().contains('\''));
            }
        }
    }
}
