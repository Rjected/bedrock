// SPDX-License-Identifier: GPL-2.0

//! `seed-N/scenario.json`: every harness decision of one run, by value.
//!
//! A seed is a compressed scenario: the swarm draw, the preemption jitter
//! seed, the nemesis kill plan and each load generation's seeds are all
//! derived from it. The scenario stores the derived values, so
//! `bedrock-dst replay --scenario` reruns the same decisions even after the
//! seed-to-decision derivation changes (it never calls `swarm_for`, the
//! nemesis planner or the generators' seed derivations).
//!
//! It does not cover randomness inside the guest that is not a harness
//! decision (thread-fuzz schedules, Tempo's and the kernel's own RNG use):
//! that is the run's Bedrock RNG stream, reproduced by `rng_seed` (same
//! binaries, same stream) or exactly by the input tape.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Load, Swarm};

pub const SCENARIO_VERSION: u32 = 1;

/// Who makes the harness decisions that are not swarm features (the
/// nemesis kill plan, each load generation's seeds).
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decisions {
    /// The guest derives them (the nemesis draws its plan from Bedrock's
    /// getrandom stream); the driver reads them back from the run's events.
    /// `replay --scenario` then has to deliver them, so the delivered config
    /// differs from the original and only the decisions (not the thread
    /// schedule) are guaranteed to replay. The default, and what campaigns
    /// from before `--decisions` used.
    #[default]
    Guest,
    /// The driver derives them from the seed before the run and delivers
    /// them in the config (`nemesis_plan`, `load.generations`), so
    /// `replay --scenario` delivers byte-identical config and reproduces the
    /// run exactly. Changes which kill plan a seed gets.
    Explicit,
}

/// One nemesis kill: SIGKILL the node `at_secs` after nemesis start, restart
/// it `down_secs` later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kill {
    pub at_secs: u64,
    pub down_secs: u64,
}

/// Inputs of one load generation (0 at branch start, one more per restart).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generation {
    /// txgen's `TXGEN_SEED`.
    pub txgen_seed: u64,
    /// Seed of the generated trie/TIP-20/chain spec.
    pub spec_seed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NemesisScenario {
    pub enabled: bool,
    pub max_kills: u32,
    pub min_gap_secs: u32,
    /// The kill plan; `None` until known (guest-made, before the run).
    pub plan: Option<Vec<Kill>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadScenario {
    pub kind: Load,
    pub count: u64,
    pub tps: u64,
    /// Load image; `None` is the plain txgen image.
    pub image: Option<String>,
    /// `--workload-seed` (generation `g`'s derived txgen seed is this + g).
    pub workload_seed: u64,
    /// Spec seeds drawn in the guest from getrandom (`--spec-seeds-from-getrandom`).
    #[serde(default)]
    pub draw_spec_seeds: bool,
    /// Per-generation inputs, delivered by `replay --scenario`.
    pub generations: Vec<Generation>,
}

/// One load generation as the guest reported it (`load`/`generation` event).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedGeneration {
    pub generation: u64,
    pub txgen_seed: u64,
    pub spec_seed: u64,
    pub steps: u64,
    /// `trie`, `tip20`, `chain`, or `None` for an image-defined spec.
    pub kind: Option<String>,
    /// keccak256 of the generated spec: the full load content, which is a
    /// function of the generator code and the inputs above.
    pub spec_keccak: Option<String>,
    /// Trie slots (derived in the guest from the run seed).
    pub slots: Option<Vec<u64>>,
    /// The guest process that started the generation (consumer of its
    /// getrandom draws on the tape). Not a decision; ignored by comparisons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// What the run reported about its decisions (from `events.jsonl`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {
    /// The nemesis `plan` event.
    pub nemesis_plan: Option<Vec<Kill>>,
    pub generations: Vec<ObservedGeneration>,
}

impl Observed {
    /// Parses the decision events out of an `events.jsonl`.
    pub fn from_events(events: &str) -> Self {
        let mut out = Observed::default();
        for line in events.lines() {
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match (ev["source"].as_str(), ev["kind"].as_str()) {
                (Some("nemesis"), Some("plan")) => {
                    out.nemesis_plan = serde_json::from_value(ev["detail"]["kills"].clone()).ok();
                }
                (Some("load"), Some("generation")) => {
                    if let Ok(g) = serde_json::from_value(ev["detail"].clone()) {
                        out.generations.push(g);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Without the non-decision fields, for comparisons.
    pub fn decisions(&self) -> Self {
        let mut d = self.clone();
        for g in &mut d.generations {
            g.pid = None;
        }
        d
    }
}

/// `seed-N/scenario.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    pub version: u32,
    pub decisions: Decisions,
    /// The run seed: the config's `seed` (guest-side derivations key off it).
    pub seed: u64,
    /// `Branch::reseed_rng` seed: Bedrock's RDRAND/getrandom stream.
    pub rng_seed: u64,
    pub swarm: Swarm,
    pub run_secs: u64,
    pub liveness_secs: u64,
    pub nemesis: NemesisScenario,
    pub load: LoadScenario,
    /// Filled in after the run.
    #[serde(default)]
    pub observed: Observed,
    /// What each field means (documentation only; ignored on read).
    #[serde(default)]
    pub meaning: BTreeMap<String, String>,
}

impl Scenario {
    /// Records what the run reported: guest-made decisions become explicit
    /// for replay; driver-made ones are kept as delivered.
    pub fn record_observed(&mut self, observed: Observed) {
        if self.nemesis.plan.is_none() {
            self.nemesis.plan = observed.nemesis_plan.clone();
        }
        if self.load.generations.is_empty() {
            self.load.generations = observed
                .generations
                .iter()
                .map(|g| Generation {
                    txgen_seed: g.txgen_seed,
                    spec_seed: g.spec_seed,
                })
                .collect();
        }
        self.observed = observed;
    }

    /// Whether `replay --scenario` delivers exactly the original config, and
    /// so must reproduce the run byte for byte.
    pub fn replays_exactly(&self) -> bool {
        self.decisions == Decisions::Explicit && !self.load.draw_spec_seeds
    }

    /// Where `self` (the original) and `replayed` made different decisions.
    pub fn decision_diff(&self, replayed: &Scenario) -> Vec<String> {
        let mut out = Vec::new();
        if self.swarm != replayed.swarm {
            out.push(format!("swarm: {:?} vs {:?}", self.swarm, replayed.swarm));
        }
        let (a, b) = (self.observed.decisions(), replayed.observed.decisions());
        if a.nemesis_plan != b.nemesis_plan {
            out.push(format!(
                "nemesis plan: {:?} vs {:?}",
                a.nemesis_plan, b.nemesis_plan
            ));
        }
        if a.generations != b.generations {
            out.push(format!(
                "load generations: {:?} vs {:?}",
                a.generations, b.generations
            ));
        }
        out
    }
}

/// Field documentation written into every scenario.
pub fn meaning() -> BTreeMap<String, String> {
    [
        ("decisions", "guest: nemesis plan and load seeds were drawn in the guest and read back from events; explicit: the driver derived them and delivered them in config.json (replay --scenario is then byte-identical)"),
        ("seed", "run seed: config.json `seed`; guest-side derivations (trie slots) key off it"),
        ("rng_seed", "Branch::reseed_rng seed: Bedrock's RDRAND/getrandom stream (thread-fuzz schedules, Tempo's own randomness); only the input tape pins it independently of binaries"),
        ("swarm.preempt", "forced preemption: period in guest instructions (0 = off) and xorshift jitter seed, applied with Branch::set_preempt"),
        ("swarm.load", "load kind (transfers, trie, tip20, chain)"),
        ("swarm.reference", "E7 reference node runs"),
        ("swarm.nemesis", "crash nemesis enabled"),
        ("run_secs", "virtual seconds before finalize"),
        ("liveness_secs", "E2 head-stall budget"),
        ("nemesis.plan", "kills: SIGKILL the node at_secs after nemesis start, restart down_secs later; each restart starts the next load generation"),
        ("load.generations", "per load generation (0 at start, +1 per restart): txgen_seed (TXGEN_SEED) and spec_seed (seed of the generated trie/TIP-20/chain spec)"),
        ("load.count", "transactions per generation (spec steps)"),
        ("observed", "what the guest reported: its plan event and per-generation load events, incl. the generated spec's keccak256 and the trie slots"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Scenario {
        Scenario {
            version: SCENARIO_VERSION,
            decisions: Decisions::Explicit,
            seed: 3,
            rng_seed: 3,
            swarm: crate::swarm_for(&crate::tests::campaign_args(&["--load", "trie"]), 3),
            run_secs: 180,
            liveness_secs: 60,
            nemesis: NemesisScenario {
                enabled: true,
                max_kills: 3,
                min_gap_secs: 30,
                plan: Some(vec![
                    Kill {
                        at_secs: 12,
                        down_secs: 1,
                    },
                    Kill {
                        at_secs: 70,
                        down_secs: 3,
                    },
                ]),
            },
            load: LoadScenario {
                kind: Load::Trie,
                count: 1200,
                tps: 5,
                image: Some("bedrock/tempo-dst-trie:latest".into()),
                workload_seed: 99,
                draw_spec_seeds: false,
                generations: vec![
                    Generation {
                        txgen_seed: 99,
                        spec_seed: 3,
                    },
                    Generation {
                        txgen_seed: 100,
                        spec_seed: 3,
                    },
                ],
            },
            observed: Observed::default(),
            meaning: meaning(),
        }
    }

    #[test]
    fn scenario_round_trips_through_json() {
        let s = sample();
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Scenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // `meaning` is documentation: a scenario without it still loads.
        let mut v: Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("meaning");
        v.as_object_mut().unwrap().remove("observed");
        let bare: Scenario = serde_json::from_value(v).unwrap();
        assert_eq!(bare.nemesis, s.nemesis);
        assert!(bare.meaning.is_empty());
    }

    #[test]
    fn observed_decisions_come_from_events() {
        let events = concat!(
            r#"{"source":"nemesis","kind":"plan","container":"tempo","guest_time_ns":1,"detail":{"kills":[{"at_secs":9,"down_secs":2}]}}"#,
            "\n",
            r#"{"source":"load","kind":"generation","container":"txgen","guest_time_ns":2,"detail":{"generation":0,"txgen_seed":99,"spec_seed":3,"steps":10,"kind":"tip20","spec_keccak":"0xab","slots":null,"pid":77}}"#,
            "\n",
            r#"{"source":"nemesis","kind":"kill","container":"tempo","guest_time_ns":3,"detail":{"index":0}}"#,
            "\nnot json\n",
        );
        let o = Observed::from_events(events);
        assert_eq!(
            o.nemesis_plan,
            Some(vec![Kill {
                at_secs: 9,
                down_secs: 2
            }])
        );
        assert_eq!(o.generations.len(), 1);
        assert_eq!(o.generations[0].spec_keccak.as_deref(), Some("0xab"));
        assert_eq!(o.generations[0].pid, Some(77));
        assert_eq!(o.decisions().generations[0].pid, None);

        // A guest-made scenario takes its plan and seeds from what it saw...
        let mut guest = sample();
        guest.decisions = Decisions::Guest;
        guest.nemesis.plan = None;
        guest.load.generations.clear();
        guest.record_observed(o.clone());
        assert_eq!(guest.nemesis.plan, o.nemesis_plan);
        assert_eq!(
            guest.load.generations,
            [Generation {
                txgen_seed: 99,
                spec_seed: 3
            }]
        );
        assert!(!guest.replays_exactly());
        // ...an explicit one keeps what it delivered.
        let mut explicit = sample();
        explicit.record_observed(o);
        assert_eq!(explicit.nemesis.plan, sample().nemesis.plan);
        assert!(explicit.replays_exactly());
    }
}
