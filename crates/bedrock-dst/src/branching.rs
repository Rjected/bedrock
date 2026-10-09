// SPDX-License-Identifier: GPL-2.0

//! Branching from a moment of a recorded run (`bedrock-dst branch`), and
//! the moments worth branching from (`bedrock-dst moments`).
//!
//! A recorded seed (tape, scenario, manifest) re-executes deterministically
//! to the moment `T`; a checkpoint there forks `K` branches that continue
//! with other randomness ([`Vary::Rng`]), other harness decisions
//! ([`Vary::Decisions`]), both, or nothing ([`Vary::None`]: the original run
//! again). This module holds the pure parts: parsing a moment, deriving each
//! branch's seed and decisions, listing moments, and summarizing branches.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use bedrock_lab::{Cut, InputRecording, VirtTime};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scenario::{Generation, Kill, Scenario};
use crate::{splitmix64, NEMESIS_MAX_DOWN_SECS, NEMESIS_WARMUP_SECS};

/// Salt of a branch's suffix RNG seed.
const BRANCH_RNG_SALT: u64 = 0x6272_616e_6368_7231; // "branchr1"
/// Salt of a branch's re-drawn decisions.
const BRANCH_DECISION_SALT: u64 = 0x6272_616e_6368_6431; // "branchd1"

/// What a branch changes after the moment.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vary {
    /// Nothing: the rest of the tape (randomness and host actions) as
    /// recorded. Reproduces the original run byte for byte.
    None,
    /// Randomness: a fresh stream seeded per branch (thread-fuzz schedules,
    /// Tempo's and the kernel's own RNG, guest-drawn spec seeds).
    #[default]
    Rng,
    /// Harness decisions: the nemesis kills and load generations not yet
    /// applied at the moment are re-drawn from the branch seed (delivered
    /// with `tempo-dst redecide`); randomness continues the original stream.
    Decisions,
    /// Both.
    Both,
}

impl Vary {
    pub fn decisions(self) -> bool {
        matches!(self, Vary::Decisions | Vary::Both)
    }
}

/// A moment of a recorded run (`--at`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum At {
    /// Virtual seconds since the branch forked from the warm checkpoint
    /// (the run's own clock: `42.5` or `42.5s`).
    RunSecs(f64),
    /// Absolute virtual time (`vt:110.2`), as console.log stamps it.
    Vt(f64),
    /// Just before randomness input `n` of the tape (`input:1234` or `#1234`).
    Input(usize),
}

impl FromStr for At {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let secs = |v: &str| -> Result<f64, String> {
            let v = v.strip_suffix('s').unwrap_or(v);
            match v.parse::<f64>() {
                Ok(x) if x.is_finite() && x >= 0.0 => Ok(x),
                _ => Err(format!(
                    "bad moment {s:?}: want <run secs>, vt:<secs> or input:<n>"
                )),
            }
        };
        if let Some(n) = s.strip_prefix("input:").or_else(|| s.strip_prefix('#')) {
            return n
                .parse()
                .map(At::Input)
                .map_err(|_| format!("bad input index in {s:?}"));
        }
        if let Some(v) = s.strip_prefix("vt:") {
            return secs(v).map(At::Vt);
        }
        secs(s).map(At::RunSecs)
    }
}

impl fmt::Display for At {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            At::RunSecs(s) => write!(f, "{s}s"),
            At::Vt(s) => write!(f, "vt:{s}"),
            At::Input(n) => write!(f, "input:{n}"),
        }
    }
}

impl At {
    /// The virtual time of this moment in a run that forked at `start`.
    pub fn resolve(&self, rec: &InputRecording, start: VirtTime) -> Result<VirtTime, String> {
        let freq = start.frequency();
        Ok(match *self {
            At::RunSecs(s) => start + bedrock_lab::VirtDuration::from_secs_f64(s, freq),
            At::Vt(s) => VirtTime::from_secs_f64(s, freq),
            At::Input(n) => {
                rec.random_inputs()
                    .get(n)
                    .ok_or_else(|| {
                        format!(
                            "the tape holds {} randomness inputs; input:{n} is past its end",
                            rec.random_inputs().len()
                        )
                    })?
                    .at
            }
        })
    }

    /// For a default output directory name.
    pub fn slug(&self) -> String {
        self.to_string().replace(':', "-")
    }
}

/// A stretch of a run's randomness served by one [`bedrock_lab::SeededSource`]:
/// from randomness input `from_input` on, the stream of `seed` after `skip`
/// values. A campaign seed is one stretch; each `--vary rng` branch starts
/// another at its cut. `--vary decisions` continues the stretch in force at
/// its cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RngStream {
    pub from_input: u64,
    pub seed: u64,
    pub skip: u64,
}

/// A host command a branch issues at a virtual time mid-run (the
/// `tempo-dst redecide` of `--vary decisions`); `replay --tape` issues it
/// again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAction {
    pub at_instructions: u64,
    pub command: String,
}

/// `manifest.json` `branch`: where a branch came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchInfo {
    /// The recorded seed directory branched from.
    pub parent: String,
    pub parent_seed: u64,
    pub parent_tape_sha256: String,
    /// `--at` as given.
    pub at: String,
    /// Where the branches forked: the checkpoint's virtual time.
    pub at_instructions: u64,
    pub at_run_secs: f64,
    /// Inputs taken from the parent tape (the branch tape's prefix).
    pub cut: Cut,
    pub vary: Vary,
    /// The branch seed (`--seed-start`..): with the parent tape and the
    /// moment, all a branch is a function of.
    pub branch: u64,
    /// The suffix stream's seed (`--vary rng|both`).
    pub rng_seed: Option<u64>,
    /// How this run's randomness was served, stretch by stretch.
    pub rng_streams: Vec<RngStream>,
    /// Host commands issued mid-run.
    #[serde(default)]
    pub actions: Vec<HostAction>,
}

/// The stream a branch continues for `--vary decisions` at randomness input
/// `cut`: the stretch in force there, advanced past what the run drew.
pub fn continued_stream(
    streams: &[RngStream],
    rec: &InputRecording,
    cut: usize,
) -> Option<RngStream> {
    let s = streams
        .iter()
        .rev()
        .find(|s| s.from_input as usize <= cut)?;
    let drawn = &rec.random_inputs()[s.from_input as usize..cut];
    Some(RngStream {
        from_input: cut as u64,
        seed: s.seed,
        skip: s.skip + bedrock_lab::SeededSource::draws(drawn),
    })
}

/// The first 16 hex digits of a sha256, as a number.
fn sha_prefix(sha256: &str) -> u64 {
    u64::from_str_radix(sha256.get(..16).unwrap_or("0"), 16).unwrap_or(0)
}

/// Branch `branch`'s seed for its suffix randomness and re-drawn decisions:
/// a pure function of the parent tape, the moment and the branch seed.
pub fn branch_seed(parent_tape_sha256: &str, at: VirtTime, branch: u64) -> u64 {
    splitmix64(
        splitmix64(sha_prefix(parent_tape_sha256) ^ BRANCH_RNG_SALT ^ at.instructions()) ^ branch,
    )
}

/// A branch's decisions: the parent's up to the moment, re-drawn after it.
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
            .map_or(NEMESIS_WARMUP_SECS, |k| k.at_secs + k.down_secs + gap)
            .max(after);
        while plan.len() < n.max_kills as usize {
            let at = t + next() % (gap + 1);
            let down = next() % (NEMESIS_MAX_DOWN_SECS + 1);
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

/// One moment worth branching from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Moment {
    /// Seconds since the branch forked (what `--at <secs>` takes).
    pub run_secs: f64,
    pub vt_secs: f64,
    /// Randomness inputs of the tape before it (`--at input:<n>`).
    pub input: usize,
    pub what: String,
}

/// Moments of a recorded run, from its events (nemesis kills and restarts,
/// load generations, re-decisions, each Always signature's first failure and
/// its last pass before it) and its tape (the start action). Guest event
/// times are mapped to virtual time through the run's first event, which
/// `tempo-dst start` emits while the start action (on the tape) runs, so
/// they are good to a few hundred ms; `--at input:<n>` is exact.
pub fn moments(events: &str, rec: &InputRecording, start: VirtTime, run_secs: u64) -> Vec<Moment> {
    let freq = start.frequency();
    let Some(start_io) = rec.io_inputs().first() else {
        return Vec::new();
    };
    let evs: Vec<Value> = events
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let mut raw: Vec<(f64, String)> = vec![(
        start_io.at.as_secs_f64(),
        "`tempo-dst start` (branch from after it)".into(),
    )];
    if let Some(first_ns) = evs.first().and_then(|e| e["guest_time_ns"].as_u64()) {
        let vt = |ns: u64| start_io.at.as_secs_f64() + (ns as f64 - first_ns as f64) / 1e9;
        for e in &evs {
            let Some(ns) = e["guest_time_ns"].as_u64() else {
                continue;
            };
            let d = &e["detail"];
            let node = e["container"].as_str() == Some("tempo");
            match (e["source"].as_str(), e["kind"].as_str()) {
                (Some("nemesis"), Some("kill")) if node => {
                    raw.push((vt(ns), format!("nemesis kill #{}", d["index"])));
                }
                (Some("nemesis"), Some("restart")) if node => {
                    raw.push((vt(ns), format!("nemesis restart #{}", d["index"])));
                }
                (Some("nemesis"), Some("redecide")) => {
                    raw.push((vt(ns), "decisions re-drawn (branch)".into()));
                }
                (Some("load"), Some("generation")) if d["generation"].as_u64() != Some(0) => {
                    raw.push((vt(ns), format!("load generation {}", d["generation"])));
                }
                (Some("assertion"), Some("first-failure")) => {
                    let sig = d["signature"].as_str().unwrap_or("?");
                    raw.push((vt(ns), format!("first failure: {sig}")));
                    raw.push((vt(ns) - 5.0, format!("5 s before first failure: {sig}")));
                    if let Some(p) = d["last_pass_ns"].as_u64() {
                        raw.push((vt(p), format!("last pass before failure: {sig}")));
                    }
                }
                _ => {}
            }
        }
    }
    let lo = start_io.at.as_secs_f64();
    let hi = start.as_secs_f64() + run_secs as f64;
    let mut out: Vec<Moment> = raw
        .into_iter()
        .filter(|(t, _)| *t >= lo && *t < hi)
        .map(|(t, what)| Moment {
            run_secs: t - start.as_secs_f64(),
            vt_secs: t,
            input: rec.cut_at_time(VirtTime::from_secs_f64(t, freq)).random,
            what,
        })
        .collect();
    out.sort_by(|a, b| a.vt_secs.total_cmp(&b.vt_secs));
    out
}

/// One finished branch, for the summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BranchResult {
    pub name: String,
    pub pass: bool,
    pub failures: Vec<String>,
    /// sha256 of the branch's assertions and events: equal fingerprints are
    /// the same run.
    pub fingerprint: String,
    /// Artifacts and tape equal the parent's.
    pub identical_to_parent: bool,
}

/// Signatures × branches, like `regressions/hunt.sh`.
pub fn summarize(results: &[BranchResult]) -> String {
    let mut by_sig: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for r in results {
        for s in &r.failures {
            by_sig.entry(s).or_default().push(&r.name);
        }
    }
    let distinct = results
        .iter()
        .map(|r| &r.fingerprint)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let passed = results.iter().filter(|r| r.pass).count();
    let identical = results.iter().filter(|r| r.identical_to_parent).count();
    let mut out = format!(
        "{} branches ({distinct} distinct runs, {identical} identical to the parent): {passed} pass; {} failing signatures\n",
        results.len(),
        by_sig.len()
    );
    let mut sigs: Vec<_> = by_sig.into_iter().collect();
    sigs.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));
    for (sig, branches) in sigs {
        out += &format!(
            "{:4}  {sig}  e.g. {}\n",
            branches.len(),
            branches[..branches.len().min(3)].join(", ")
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::tests::sample;
    use bedrock_lab::{BashTarget, IoInput, RandomInput};
    use bedrock_vm::events::RandomSource;

    const F: u64 = bedrock_vm::DEFAULT_TSC_FREQUENCY;

    fn vt(s: f64) -> VirtTime {
        VirtTime::from_secs_f64(s, F)
    }

    #[test]
    fn moments_parse() {
        assert_eq!("42.5".parse::<At>(), Ok(At::RunSecs(42.5)));
        assert_eq!("42.5s".parse::<At>(), Ok(At::RunSecs(42.5)));
        assert_eq!("vt:110".parse::<At>(), Ok(At::Vt(110.0)));
        assert_eq!("input:12".parse::<At>(), Ok(At::Input(12)));
        assert_eq!("#12".parse::<At>(), Ok(At::Input(12)));
        for bad in ["", "-1", "vt:x", "input:-3", "nan", "12q"] {
            assert!(bad.parse::<At>().is_err(), "{bad}");
        }
        assert_eq!(At::Vt(1.5).slug(), "vt-1.5");
        let rec = InputRecording::from_parts(
            vec![RandomInput {
                at: vt(75.0),
                source: RandomSource::Rdrand,
                pid: 0,
                bytes: vec![0; 8],
            }],
            vec![],
        );
        let start = vt(70.0);
        assert_eq!(At::RunSecs(2.0).resolve(&rec, start).unwrap(), vt(72.0));
        assert_eq!(At::Vt(71.0).resolve(&rec, start).unwrap(), vt(71.0));
        assert_eq!(At::Input(0).resolve(&rec, start).unwrap(), vt(75.0));
        assert!(At::Input(1).resolve(&rec, start).is_err());
    }

    /// A guest-decided trie scenario: kills at 10+2 and 40+1, gap 20, three
    /// load generations.
    fn parent() -> Scenario {
        let mut s = sample();
        s.run_secs = 90;
        s.nemesis.enabled = true;
        s.nemesis.max_kills = 3;
        s.nemesis.min_gap_secs = 20;
        s.nemesis.plan = Some(vec![
            Kill {
                at_secs: 10,
                down_secs: 2,
            },
            Kill {
                at_secs: 40,
                down_secs: 1,
            },
        ]);
        s.load.generations = (0..3)
            .map(|g| Generation {
                txgen_seed: 99 + g,
                spec_seed: 3,
            })
            .collect();
        s
    }

    #[test]
    fn redraw_keeps_decisions_before_the_moment() {
        let p = parent();
        let plan0 = p.nemesis.plan.clone().unwrap();
        // 20 s in: kill 0 fired and restarted, so it and generations 0 and
        // 1 stay; everything after is re-drawn.
        let r = redraw_decisions(&p, 20.0, 1);
        assert_eq!((r.kept_kills, r.kept_generations), (1, 2));
        assert_eq!(r.nemesis_plan[0], plan0[0]);
        assert_eq!(r.generations[..2], p.load.generations[..2]);
        // One generation per restart plus the first.
        assert_eq!(r.generations.len(), r.nemesis_plan.len() + 1);
        // The re-drawn kills come after the moment and follow the planner's
        // constraints.
        for w in r.nemesis_plan.windows(2) {
            assert!(w[1].at_secs >= w[0].at_secs + w[0].down_secs + 20, "{r:?}");
        }
        for k in &r.nemesis_plan[1..] {
            assert!(k.at_secs > 20 && k.down_secs <= NEMESIS_MAX_DOWN_SECS);
            assert!(k.at_secs + k.down_secs + 20 <= p.run_secs);
        }
        assert!(r.nemesis_plan.len() <= 3);
        // A pure function of the seed; other seeds give other decisions.
        assert_eq!(redraw_decisions(&p, 20.0, 1), r);
        let others: std::collections::BTreeSet<_> = (0..16)
            .map(|s| format!("{:?}", redraw_decisions(&p, 20.0, s).generations))
            .collect();
        assert!(others.len() > 8);

        // 11 s in: kill 0 fired but its restart (at 12) has not: generation
        // 1 starts after the moment, so it is re-drawn.
        let mid = redraw_decisions(&p, 11.0, 1);
        assert_eq!((mid.kept_kills, mid.kept_generations), (1, 1));
        // Before any kill: the whole plan is re-drawn, after the moment.
        let early = redraw_decisions(&p, 3.0, 2);
        assert_eq!((early.kept_kills, early.kept_generations), (0, 1));
        assert!(early.nemesis_plan.iter().all(|k| k.at_secs > 3));
        // After the last kill and its restart: nothing left to re-draw but
        // whatever still fits.
        let late = redraw_decisions(&p, 80.0, 3);
        assert_eq!(late.nemesis_plan, plan0);
        assert_eq!(late.generations, p.load.generations);
        // No nemesis, no load: nothing to draw.
        let mut quiet = p.clone();
        quiet.nemesis.enabled = false;
        quiet.nemesis.plan = Some(vec![]);
        quiet.load.generations.clear();
        let q = redraw_decisions(&quiet, 20.0, 4);
        assert!(q.nemesis_plan.is_empty() && q.generations.is_empty());
    }

    #[test]
    fn branch_seeds_depend_on_tape_moment_and_branch() {
        let sha = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
        let s = branch_seed(sha, vt(80.0), 0);
        assert_eq!(s, branch_seed(sha, vt(80.0), 0));
        assert_ne!(s, branch_seed(sha, vt(80.0), 1));
        assert_ne!(s, branch_seed(sha, vt(81.0), 0));
        assert_ne!(s, branch_seed("ff", vt(80.0), 0));
    }

    fn rd(at: f64, len: Option<usize>) -> RandomInput {
        RandomInput {
            at: vt(at),
            source: if len.is_some() {
                RandomSource::GetRandom
            } else {
                RandomSource::Rdrand
            },
            pid: 1,
            bytes: vec![0; len.unwrap_or(8)],
        }
    }

    #[test]
    fn decisions_continue_the_stream_in_force() {
        let rec = InputRecording::from_parts(
            vec![
                rd(1.0, None),
                rd(2.0, Some(12)),
                rd(3.0, None),
                rd(4.0, Some(3)),
            ],
            vec![],
        );
        let seed = RngStream {
            from_input: 0,
            seed: 7,
            skip: 0,
        };
        // A campaign seed: one stretch from input 0.
        assert_eq!(
            continued_stream(&[seed], &rec, 3),
            Some(RngStream {
                from_input: 3,
                seed: 7,
                skip: 1 + 2 + 1
            })
        );
        // A branch of a branch: the stretch that began at the first cut.
        let branched = RngStream {
            from_input: 2,
            seed: 9,
            skip: 0,
        };
        assert_eq!(
            continued_stream(&[seed, branched], &rec, 4),
            Some(RngStream {
                from_input: 4,
                seed: 9,
                skip: 2
            })
        );
        assert_eq!(
            continued_stream(&[seed, branched], &rec, 1).unwrap().seed,
            7
        );
    }

    #[test]
    fn moments_come_from_events_and_tape() {
        let start = vt(70.0);
        let rec = InputRecording::from_parts(
            (0..100).map(|i| rd(70.0 + f64::from(i), None)).collect(),
            vec![IoInput {
                at: vt(70.5),
                target: BashTarget::Host,
                command: "tempo-dst start '{}'".into(),
                record_output: true,
            }],
        );
        let s = 1_000_000_000u64;
        // Guest clock 1000 s at the start action's first event.
        let events = [
            format!(r#"{{"source":"load","kind":"generation","container":"txgen","guest_time_ns":{},"detail":{{"generation":0}}}}"#, 1000 * s),
            format!(r#"{{"source":"nemesis","kind":"plan","container":"tempo","guest_time_ns":{},"detail":{{"kills":[]}}}}"#, 1000 * s),
            format!(r#"{{"source":"nemesis","kind":"kill","container":"tempo","guest_time_ns":{},"detail":{{"index":0}}}}"#, 1010 * s),
            format!(r#"{{"source":"nemesis","kind":"kill","container":"txgen","guest_time_ns":{},"detail":{{}}}}"#, 1012 * s),
            format!(r#"{{"source":"nemesis","kind":"restart","container":"tempo","guest_time_ns":{},"detail":{{"index":0}}}}"#, 1012 * s),
            format!(r#"{{"source":"load","kind":"generation","container":"txgen","guest_time_ns":{},"detail":{{"generation":1}}}}"#, 1013 * s),
            format!(r#"{{"source":"assertion","kind":"first-failure","guest_time_ns":{},"detail":{{"signature":"E5/storage-root-mismatch","last_pass_ns":{}}}}}"#, 1030 * s, 1029 * s),
            // After the run: dropped.
            format!(r#"{{"source":"nemesis","kind":"kill","container":"tempo","guest_time_ns":{},"detail":{{"index":1}}}}"#, 1100 * s),
        ]
        .join("\n");
        let m = moments(&events, &rec, start, 90);
        let what: Vec<_> = m.iter().map(|m| m.what.as_str()).collect();
        assert_eq!(
            what,
            [
                "`tempo-dst start` (branch from after it)",
                "nemesis kill #0",
                "nemesis restart #0",
                "load generation 1",
                "5 s before first failure: E5/storage-root-mismatch",
                "last pass before failure: E5/storage-root-mismatch",
                "first failure: E5/storage-root-mismatch",
            ]
        );
        let kill = &m[1];
        assert!((kill.vt_secs - 80.5).abs() < 1e-6, "{kill:?}");
        assert!((kill.run_secs - 10.5).abs() < 1e-6);
        // Inputs at 70..=80 s come before it.
        assert_eq!(kill.input, 11);
        assert!(moments("", &InputRecording::new(), start, 90).is_empty());
    }

    #[test]
    fn summary_counts_signatures_and_distinct_runs() {
        let r = |name: &str, fails: &[&str], fp: &str| BranchResult {
            name: name.into(),
            pass: fails.is_empty(),
            failures: fails.iter().map(|s| s.to_string()).collect(),
            fingerprint: fp.into(),
            identical_to_parent: false,
        };
        let out = summarize(&[
            r("branch-0", &[], "a"),
            r("branch-1", &["E1/panic"], "b"),
            r("branch-2", &["E1/panic", "E2/head-stalled"], "c"),
            r("branch-3", &[], "a"),
        ]);
        assert!(out.starts_with("4 branches (3 distinct runs, 0 identical to the parent): 2 pass; 2 failing signatures"), "{out}");
        assert!(
            out.contains("   2  E1/panic  e.g. branch-1, branch-2"),
            "{out}"
        );
        assert!(
            out.contains("   1  E2/head-stalled  e.g. branch-2"),
            "{out}"
        );
    }
}
