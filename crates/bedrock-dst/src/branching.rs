// SPDX-License-Identifier: GPL-2.0

//! Branching from a moment of a recorded run (`bedrock-dst branch`), and
//! the moments worth branching from (`bedrock-dst moments`).
//!
//! A recorded seed (tape, scenario, manifest) re-executes deterministically
//! to the moment `T`; a checkpoint there forks `K` branches that continue
//! with other randomness ([`Vary::Rng`]), other harness decisions
//! ([`Vary::Decisions`]), both, or nothing ([`Vary::None`]: the original run
//! again). This module holds the pure parts: parsing a moment, deriving each
//! branch's seed, listing moments, and summarizing branches. Re-drawing a
//! branch's decisions is the workload planner's (`Redraw` in the contract).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use bedrock_dst_contract::Moment as WorkloadMoment;
use bedrock_lab::{Cut, InputRecording, VirtTime};
use serde::{Deserialize, Serialize};

use crate::splitmix64;

/// Salt of a branch's suffix RNG seed.
const BRANCH_RNG_SALT: u64 = 0x6272_616e_6368_7231; // "branchr1"

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
    /// The workload's decisions not yet applied at the moment are re-drawn
    /// by its planner from the branch seed (delivered with the planner's
    /// mid-run action); randomness continues the original stream.
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

/// A host command a branch issues at a virtual time mid-run (the planner's
/// action of `--vary decisions`); `replay --tape` issues it again.
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

/// Moments of a recorded run: the start action (from the tape) and the
/// moments the workload found in its events (`observe`). Guest event times
/// are mapped to virtual time through the run's first event (`origin_ns`),
/// which the start hook emits while the start action (on the tape) runs, so
/// they are good to a few hundred ms; `--at input:<n>` is exact.
pub fn moments(
    workload: &[WorkloadMoment],
    origin_ns: Option<u64>,
    start_label: &str,
    rec: &InputRecording,
    start: VirtTime,
    run_secs: u64,
) -> Vec<Moment> {
    let freq = start.frequency();
    let Some(start_io) = rec.io_inputs().first() else {
        return Vec::new();
    };
    let mut raw: Vec<(f64, String)> = vec![(start_io.at.as_secs_f64(), start_label.into())];
    if let Some(first_ns) = origin_ns {
        let vt = |ns: i64| start_io.at.as_secs_f64() + (ns as f64 - first_ns as f64) / 1e9;
        raw.extend(
            workload
                .iter()
                .map(|m| (vt(m.guest_time_ns), m.what.clone())),
        );
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
    fn moments_come_from_the_workload_and_tape() {
        let start = vt(70.0);
        let rec = InputRecording::from_parts(
            (0..100).map(|i| rd(70.0 + f64::from(i), None)).collect(),
            vec![IoInput {
                at: vt(70.5),
                target: BashTarget::Host,
                command: "wl start '{}'".into(),
                record_output: true,
            }],
        );
        let s = 1_000_000_000i64;
        let wm = |t: i64, what: &str| WorkloadMoment {
            guest_time_ns: t * s,
            what: what.into(),
        };
        // Guest clock 1000 s at the start action's first event.
        let workload = [
            wm(1030, "first failure: E5"),
            wm(1010, "kill #0"),
            wm(1025, "5 s before first failure: E5"),
            // Before the start action, and after the run: dropped.
            wm(999, "early"),
            wm(1100, "kill #1"),
        ];
        let m = moments(
            &workload,
            Some(1000 * s as u64),
            "`wl start`",
            &rec,
            start,
            90,
        );
        let what: Vec<_> = m.iter().map(|m| m.what.as_str()).collect();
        assert_eq!(
            what,
            [
                "`wl start`",
                "kill #0",
                "5 s before first failure: E5",
                "first failure: E5",
            ]
        );
        let kill = &m[1];
        assert!((kill.vt_secs - 80.5).abs() < 1e-6, "{kill:?}");
        assert!((kill.run_secs - 10.5).abs() < 1e-6);
        // Inputs at 70..=80 s come before it.
        assert_eq!(kill.input, 11);
        // Without events only the start action is known.
        assert_eq!(moments(&workload, None, "s", &rec, start, 90).len(), 1);
        assert!(moments(&[], None, "s", &InputRecording::new(), start, 90).is_empty());
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
