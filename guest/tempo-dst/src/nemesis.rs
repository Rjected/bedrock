// SPDX-License-Identifier: GPL-2.0

//! Crash nemesis: SIGKILLs the node container and restarts it at times drawn
//! from Bedrock-controlled randomness.

use std::process::Command;
use std::time::Duration;

use serde_json::json;

use crate::common::{self, Config, NemesisConfig, NODE_CONTAINER};

/// Guest seconds after nemesis start before the first kill may land.
const WARMUP_SECS: u64 = 5;
/// Longest the node stays down before restart.
const MAX_DOWN_SECS: u64 = 3;

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

pub fn run() {
    let cfg = Config::load();
    let kills = plan(&cfg.nemesis, cfg.run_secs, |lo, hi| {
        rand::random_range(lo..=hi)
    });
    common::emit_event(
        "nemesis",
        "plan",
        Some(NODE_CONTAINER),
        json!({"kills": kills.iter().map(|k| json!({"at_secs": k.at_secs, "down_secs": k.down_secs})).collect::<Vec<_>>()}),
    );
    let mut elapsed = 0;
    for (i, k) in kills.iter().enumerate() {
        std::thread::sleep(Duration::from_secs(k.at_secs - elapsed));
        // Record before killing so the monitor and oracles never see an
        // unexplained death.
        common::emit_event("nemesis", "kill", Some(NODE_CONTAINER), json!({"index": i}));
        let killed = podman(&["kill", "-s", "KILL", NODE_CONTAINER]);
        // podman start fails until the container has fully stopped.
        let _ = podman(&["wait", NODE_CONTAINER]);
        std::thread::sleep(Duration::from_secs(k.down_secs));
        let started = podman(&["start", NODE_CONTAINER]);
        common::emit_event(
            "nemesis",
            "restart",
            Some(NODE_CONTAINER),
            json!({"index": i, "killed": killed, "started": started}),
        );
        elapsed = k.at_secs + k.down_secs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn plan_is_a_function_of_the_draws() {
        let a = plan(&cfg(5, 15), 300, |lo, hi| (lo + hi) / 2);
        let b = plan(&cfg(5, 15), 300, |lo, hi| (lo + hi) / 2);
        assert_eq!(a, b);
    }
}
