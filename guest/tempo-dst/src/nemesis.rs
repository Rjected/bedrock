// SPDX-License-Identifier: GPL-2.0

//! Crash nemesis: SIGKILLs the node container and restarts it at times drawn
//! from Bedrock-controlled randomness.

use std::fs;
use std::io;
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::common::{
    self, Config, DstEvent, LoadGeneration, NemesisConfig, PlannedKill, CONFIG_PATH, NODE_CONTAINER,
};

/// Guest seconds after nemesis start before the first kill may land.
pub(crate) const WARMUP_SECS: u64 = 5;
/// Longest the node stays down before restart.
pub(crate) const MAX_DOWN_SECS: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kill {
    /// Seconds after nemesis start.
    pub at_secs: u64,
    pub down_secs: u64,
}

/// Kill schedule for one run. Kills are at least `min_gap_secs` apart and all
/// fit within `run_secs`, leaving the last gap for recovery.
pub fn plan(
    cfg: &NemesisConfig,
    run_secs: u64,
    mut draw: impl FnMut(u64, u64) -> u64,
) -> Vec<Kill> {
    let mut kills = Vec::new();
    if !cfg.enabled {
        return kills;
    }
    let gap = u64::from(cfg.min_gap_secs.max(1));
    let mut t = WARMUP_SECS;
    for _ in 0..cfg.max_kills {
        let at = t + draw(0, gap);
        let down = draw(0, MAX_DOWN_SECS);
        // Leave a full gap after the restart so liveness can be judged.
        if at + down + gap > run_secs {
            break;
        }
        kills.push(Kill {
            at_secs: at,
            down_secs: down,
        });
        t = at + down + gap;
    }
    kills
}

fn podman(args: &[&str]) -> bool {
    match Command::new("podman").args(args).status() {
        Ok(s) => s.success(),
        Err(e) => {
            eprintln!("podman {args:?}: {e}");
            false
        }
    }
}

/// The run's kills: `nemesis.plan` from the config when given (driver-made
/// decisions, `replay --scenario`), else [`plan`]. Draws either way, so the
/// guest consumes the same Bedrock randomness with or without a plan.
pub fn kills_for(cfg: &Config, draw: impl FnMut(u64, u64) -> u64) -> Vec<Kill> {
    let drawn = plan(&cfg.nemesis, cfg.run_secs, draw);
    match &cfg.nemesis_plan {
        Some(explicit) => explicit
            .iter()
            .map(|k| Kill {
                at_secs: k.at_secs,
                down_secs: k.down_secs,
            })
            .collect(),
        None => drawn,
    }
}

/// `tempo-dst nemesis [--resume]`.
pub fn run(resume: bool) {
    let cfg = Config::load();
    if resume {
        return run_resumed(&cfg);
    }
    let kills = kills_for(&cfg, |lo, hi| rand::random_range(lo..=hi));
    emit_plan(&kills, false);
    let mut elapsed = 0;
    for (i, k) in kills.iter().enumerate() {
        std::thread::sleep(Duration::from_secs(k.at_secs - elapsed));
        let killed = kill(i);
        std::thread::sleep(Duration::from_secs(k.down_secs));
        restart(&cfg, i, killed);
        elapsed = k.at_secs + k.down_secs;
    }
}

fn emit_plan(kills: &[Kill], resumed: bool) {
    let mut detail = json!({"kills": kills.iter().map(|k| json!({"at_secs": k.at_secs, "down_secs": k.down_secs})).collect::<Vec<_>>()});
    if resumed {
        detail["resumed"] = json!(true);
    }
    common::emit_event("nemesis", "plan", Some(NODE_CONTAINER), detail);
}

/// SIGKILLs the node (kill `i` of the plan) and waits until it has stopped;
/// whether the kill reached it.
fn kill(i: usize) -> bool {
    // Record before killing so the monitor and oracles never see an
    // unexplained death.
    common::emit_event("nemesis", "kill", Some(NODE_CONTAINER), json!({"index": i}));
    let killed = podman(&["kill", "-s", "KILL", NODE_CONTAINER]);
    // podman start fails until the container has fully stopped.
    let _ = podman(&["wait", NODE_CONTAINER]);
    killed
}

/// Restarts the node after kill `i` and starts the next load generation.
fn restart(cfg: &Config, i: usize, killed: bool) {
    let started = podman(&["start", NODE_CONTAINER]);
    common::emit_event(
        "nemesis",
        "restart",
        Some(NODE_CONTAINER),
        json!({"index": i, "killed": killed, "started": started}),
    );
    if let Err(e) = crate::start::restart_load(cfg, i as u64 + 1) {
        eprintln!("nemesis: restarting load: {e}");
    }
}

/// Sleeps until guest time `ns` (returns at once if it has passed).
fn sleep_until(ns: u64) {
    let now = common::guest_time_ns();
    if ns > now {
        std::thread::sleep(Duration::from_nanos(ns - now));
    }
}

/// What the nemesis has done so far in this branch, from the event log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Guest time of the first `plan` event: what kill times count from.
    pub start_ns: Option<u64>,
    /// The latest plan.
    pub plan: Vec<PlannedKill>,
    pub kills: usize,
    pub restarts: usize,
    /// Load generations started, in order.
    pub generations: Vec<LoadGeneration>,
}

impl Progress {
    pub fn of(events: &[DstEvent]) -> Self {
        let mut p = Progress::default();
        for ev in events {
            let node = ev.container.as_deref() == Some(NODE_CONTAINER);
            match (ev.source.as_str(), ev.kind.as_str()) {
                ("nemesis", "plan") if node => {
                    p.start_ns.get_or_insert(ev.guest_time_ns);
                    p.plan = serde_json::from_value(ev.detail["kills"].clone()).unwrap_or_default();
                }
                ("nemesis", "kill") if node => p.kills += 1,
                ("nemesis", "restart") if node => p.restarts += 1,
                ("load", "generation") => {
                    if let Ok(g) = serde_json::from_value(ev.detail.clone()) {
                        p.generations.push(g);
                    }
                }
                _ => {}
            }
        }
        p
    }

    /// Between faults: the nemesis has started, and no kill waits for its
    /// restart nor (with a load) a restart for its load generation.
    pub fn quiescent(&self, load: bool) -> bool {
        self.start_ns.is_some()
            && self.kills == self.restarts
            && (!load || self.generations.len() > self.restarts)
    }
}

/// A re-decided future for a running branch (`tempo-dst redecide`): the
/// whole kill plan and load generations as the driver would have them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Redecision {
    pub nemesis_plan: Vec<PlannedKill>,
    #[serde(default)]
    pub generations: Vec<LoadGeneration>,
}

/// The plan and generations to continue with: what already happened stays
/// (the guest knows exactly which kills fired; the driver only estimates
/// it), the rest comes from `r`, minus kills that would land in the past
/// (`now_secs` after nemesis start) or before the previous restart.
pub fn merge(p: &Progress, r: &Redecision, now_secs: u64) -> Redecision {
    let mut plan: Vec<PlannedKill> = p.plan.iter().take(p.kills).copied().collect();
    let mut free = plan.last().map_or(0, |k| k.at_secs + k.down_secs);
    for k in r.nemesis_plan.iter().skip(plan.len()) {
        if k.at_secs >= free && k.at_secs >= now_secs {
            plan.push(*k);
            free = k.at_secs + k.down_secs;
        }
    }
    let mut generations = p.generations.clone();
    generations.extend(r.generations.iter().skip(generations.len()).copied());
    Redecision {
        nemesis_plan: plan,
        generations,
    }
}

/// `tempo-dst redecide '<json>'`: replaces the not-yet-applied part of the
/// kill plan and load generations of a running branch (`bedrock-dst branch
/// --vary decisions`). Waits until the nemesis is between faults, stops it,
/// writes the merged plan into the config and starts `tempo-dst nemesis
/// --resume`, which continues on the original's clock. Prints the merged
/// [`Redecision`].
pub fn redecide(json: &str) -> io::Result<()> {
    let r: Redecision =
        serde_json::from_str(json).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut cfg = Config::load();
    let load = cfg.load.count > 0;
    // Let an in-flight kill or reload finish (bounded by its downtime and
    // podman); one still in flight after this is finished by --resume.
    for _ in 0..120 {
        if Progress::of(&common::read_events()).quiescent(load) {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    stop_nemesis()?;
    let p = Progress::of(&common::read_events());
    let start = p.start_ns.unwrap_or_else(common::guest_time_ns);
    let now_secs = common::guest_time_ns().saturating_sub(start) / 1_000_000_000;
    let merged = merge(&p, &r, now_secs);
    cfg.nemesis_plan = Some(merged.nemesis_plan.clone());
    cfg.load.generations = merged.generations.clone();
    cfg.validate().map_err(io::Error::other)?;
    fs::write(
        CONFIG_PATH,
        serde_json::to_string(&cfg).map_err(io::Error::other)?,
    )?;
    common::emit_event(
        "nemesis",
        "redecide",
        Some(NODE_CONTAINER),
        json!({
            "kills_done": p.kills,
            "restarts_done": p.restarts,
            "at_secs": now_secs,
            "plan": merged.nemesis_plan,
            "generations": merged.generations,
        }),
    );
    crate::start::spawn_detached_with("nemesis", &["--resume"], true)?;
    println!(
        "{}",
        serde_json::to_string(&merged).map_err(io::Error::other)?
    );
    Ok(())
}

/// SIGKILLs every running `tempo-dst nemesis` and waits until they are gone.
fn stop_nemesis() -> io::Result<()> {
    let me = std::process::id();
    let pids: Vec<u32> = fs::read_dir("/proc")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != me)
        .filter(|pid| {
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            args.len() >= 2 && args[0].ends_with(b"tempo-dst") && args[1] == b"nemesis"
        })
        .collect();
    for pid in &pids {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    for pid in &pids {
        for _ in 0..200 {
            // Gone, or a zombie waiting for init.
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let zombie = stat
                .rsplit(')')
                .next()
                .is_some_and(|s| s.trim_start().starts_with('Z'));
            if stat.is_empty() || zombie {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

/// `tempo-dst nemesis --resume`: continues a nemesis stopped by
/// [`redecide`] with the config's (merged) plan, on the original's clock: a
/// kill whose restart did not happen is finished first, then the remaining
/// kills fire at their times after the original nemesis start.
fn run_resumed(cfg: &Config) {
    let p = Progress::of(&common::read_events());
    let start = p.start_ns.unwrap_or_else(common::guest_time_ns);
    let kills: Vec<Kill> = cfg
        .nemesis_plan
        .iter()
        .flatten()
        .map(|k| Kill {
            at_secs: k.at_secs,
            down_secs: k.down_secs,
        })
        .collect();
    emit_plan(&kills, true);
    let secs = |s: u64| start + s * 1_000_000_000;
    if p.kills > p.restarts {
        // Stopped between a kill and its restart (the kill may not have
        // reached podman yet).
        let i = p.kills - 1;
        let killed = podman(&["kill", "-s", "KILL", NODE_CONTAINER]);
        let _ = podman(&["wait", NODE_CONTAINER]);
        if let Some(k) = kills.get(i) {
            sleep_until(secs(k.at_secs + k.down_secs));
        }
        restart(cfg, i, killed);
    } else if cfg.load.count > 0 && p.restarts > 0 && p.generations.len() <= p.restarts {
        // Stopped while restarting the load after restart `p.restarts`.
        if let Err(e) = crate::start::restart_load(cfg, p.restarts as u64) {
            eprintln!("nemesis: restarting load: {e}");
        }
    }
    for (i, k) in kills.iter().enumerate().skip(p.kills) {
        sleep_until(secs(k.at_secs));
        let killed = kill(i);
        sleep_until(secs(k.at_secs + k.down_secs));
        restart(cfg, i, killed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn cfg(max_kills: u32, min_gap_secs: u32) -> NemesisConfig {
        NemesisConfig {
            enabled: true,
            max_kills,
            min_gap_secs,
        }
    }

    #[test]
    fn disabled_plans_nothing() {
        let c = NemesisConfig {
            enabled: false,
            ..cfg(3, 10)
        };
        assert!(plan(&c, 1000, |lo, _| lo).is_empty());
    }

    #[test]
    fn kills_respect_gap_and_fit_the_run() {
        let mut x = 0u64;
        let kills = plan(&cfg(10, 20), 200, |lo, hi| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            lo + x % (hi - lo + 1)
        });
        assert!(!kills.is_empty());
        assert!(kills[0].at_secs >= WARMUP_SECS);
        for w in kills.windows(2) {
            assert!(w[1].at_secs >= w[0].at_secs + w[0].down_secs + 20);
        }
        for k in &kills {
            assert!(k.down_secs <= MAX_DOWN_SECS);
            assert!(k.at_secs + k.down_secs + 20 <= 200);
        }
    }

    #[test]
    fn max_kills_caps_the_plan() {
        assert_eq!(plan(&cfg(2, 1), 10_000, |lo, _| lo).len(), 2);
    }

    fn ev(source: &str, kind: &str, container: Option<&str>, t: u64, detail: Value) -> DstEvent {
        DstEvent {
            source: source.into(),
            kind: kind.into(),
            container: container.map(Into::into),
            guest_time_ns: t,
            detail,
        }
    }

    fn pk(at_secs: u64, down_secs: u64) -> PlannedKill {
        PlannedKill { at_secs, down_secs }
    }

    fn lg(txgen_seed: u64, spec_seed: u64) -> LoadGeneration {
        LoadGeneration {
            txgen_seed,
            spec_seed,
        }
    }

    /// A branch 20 s in: plan [10+2, 40+1], the first kill and restart done.
    fn progress_at_20s() -> Progress {
        let node = Some(NODE_CONTAINER);
        let gen =
            |g: u64| json!({"generation": g, "txgen_seed": 99 + g, "spec_seed": 7, "steps": 5});
        Progress::of(&[
            ev("load", "generation", Some("txgen"), 900, gen(0)),
            ev(
                "nemesis",
                "plan",
                node,
                1_000,
                json!({"kills": [{"at_secs": 10, "down_secs": 2}, {"at_secs": 40, "down_secs": 1}]}),
            ),
            ev("nemesis", "kill", node, 11_000, json!({"index": 0})),
            // The load reload is logged as a txgen kill: not a node kill.
            ev("nemesis", "kill", Some("txgen"), 13_100, json!({})),
            ev("nemesis", "restart", node, 13_000, json!({"index": 0})),
            ev("load", "generation", Some("txgen"), 13_200, gen(1)),
            ev("assertion", "first-failure", None, 15_000, json!({})),
        ])
    }

    #[test]
    fn progress_counts_node_faults_and_generations() {
        let p = progress_at_20s();
        assert_eq!(p.start_ns, Some(1_000));
        assert_eq!(p.plan, [pk(10, 2), pk(40, 1)]);
        assert_eq!((p.kills, p.restarts), (1, 1));
        assert_eq!(p.generations, [lg(99, 7), lg(100, 7)]);
        assert!(p.quiescent(true));
        // A kill without its restart, or a restart before its generation,
        // is in flight.
        let mut mid = p.clone();
        mid.kills = 2;
        assert!(!mid.quiescent(false));
        let mut reload = p.clone();
        reload.generations.pop();
        assert!(!reload.quiescent(true) && reload.quiescent(false));
        assert!(!Progress::default().quiescent(false));
    }

    #[test]
    fn redecision_keeps_what_already_happened() {
        let p = progress_at_20s();
        // The driver proposes a new plan whose first kill differs from the
        // one that fired: the fired one stays, and a proposed kill in the
        // past is dropped.
        let r = Redecision {
            nemesis_plan: vec![pk(11, 3), pk(19, 0), pk(25, 2), pk(26, 0), pk(60, 1)],
            generations: vec![lg(1, 1), lg(2, 2), lg(3, 3), lg(4, 4)],
        };
        let m = merge(&p, &r, 20);
        // Index 0 fired (kept as fired); 19 is in the past; 26 overlaps 25+2.
        assert_eq!(m.nemesis_plan, [pk(10, 2), pk(25, 2), pk(60, 1)]);
        // Generations 0 and 1 started; the rest are the proposal's.
        assert_eq!(m.generations, [lg(99, 7), lg(100, 7), lg(3, 3), lg(4, 4)]);
        // A proposal shorter than what happened keeps what happened.
        let none = merge(&p, &Redecision::default(), 20);
        assert_eq!(none.nemesis_plan, [pk(10, 2)]);
        assert_eq!(none.generations.len(), 2);
    }

    #[test]
    fn plan_is_a_function_of_the_draws() {
        let a = plan(&cfg(5, 15), 300, |lo, hi| (lo + hi) / 2);
        let b = plan(&cfg(5, 15), 300, |lo, hi| (lo + hi) / 2);
        assert_eq!(a, b);
    }
}
