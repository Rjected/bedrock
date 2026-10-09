// SPDX-License-Identifier: GPL-2.0

//! Branch setup, invoked once by the host driver right after forking: installs
//! the run config, resets the event log, starts the oracle, nemesis, and load,
//! and snapshots assertion file offsets for this run.

use std::fs::{self, File};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

use crate::common::{self, Config, CONFIG_PATH, EVENTS_PATH, OUT_DIR};
use crate::{cob_gen, tip20, trie_gen};

/// Starts `tempo-dst <sub>` in its own session so it outlives the I/O-channel
/// command that launched it.
fn spawn_detached(sub: &str) -> io::Result<()> {
    spawn_detached_with(sub, &[], false)
}

/// [`spawn_detached`] with extra arguments; `append` keeps the existing log
/// (a resumed nemesis continues the original's `nemesis.log`).
pub(crate) fn spawn_detached_with(sub: &str, args: &[&str], append: bool) -> io::Result<()> {
    let path = Path::new(OUT_DIR).join(format!("{sub}.log"));
    let log = if append {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
    } else {
        File::create(path)?
    };
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg(sub)
        .args(args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| nix::unistd::setsid().map(drop).map_err(io::Error::from));
    }
    cmd.spawn().map(drop)
}

pub fn run(config_json: &str) -> io::Result<()> {
    let config: Config = serde_json::from_str(config_json)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    config
        .validate()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    fs::create_dir_all(Path::new(CONFIG_PATH).parent().unwrap())?;
    fs::create_dir_all(OUT_DIR)?;
    fs::write(CONFIG_PATH, config_json)?;
    fs::write(EVENTS_PATH, "")?;
    crate::assertions::snapshot()?;

    spawn_detached("oracle")?;
    spawn_detached("nemesis")?;
    start_load(&config, 0)?;
    Ok(())
}

/// The generated load spec of `generation` with spec seed `seed`, as
/// `(kind, text)`; `None` for a load that runs `load.spec` as is.
pub fn generated_spec(
    config: &Config,
    generation: u64,
    seed: u64,
) -> Option<(&'static str, String)> {
    let steps = usize::try_from(config.load.count).unwrap_or(usize::MAX);
    if let Some(trie) = config
        .trie
        .as_ref()
        .filter(|t| t.generated())
        .and(config.trie())
    {
        Some(("trie", trie_gen::spec(seed, generation, &trie.slots, steps)))
    } else if config.tip20 {
        Some(("tip20", tip20::spec(seed, generation, steps)))
    } else if config.cob.is_some() {
        Some(("chain", cob_gen::spec(seed, generation, steps)))
    } else {
        None
    }
}

/// Starts the txgen load as container `txgen`. Generation `n` (0 at branch
/// start, then one per node restart) takes its inputs from
/// [`Config::load_generation`]: by default it draws its transactions from
/// workload seed `load.seed + n`; a generated trie load also gets a fresh
/// write stream (`trie_gen::spec`) over the run's slots, the TIP-20 load a
/// fresh transfer stream (`tip20::spec`), and a chain-of-blocks load fresh
/// payloads (`cob_gen::spec`). Each generation is logged as a `load`
/// `generation` event with its inputs and the spec's keccak, which the
/// driver copies into the run's scenario.
///
/// Load only: failures (e.g. while the node is down) are not oracles. The
/// oracle's S/load-included shows the load actually landed.
pub fn start_load(config: &Config, generation: u64) -> io::Result<()> {
    let load = &config.load;
    if load.count == 0 {
        return Ok(());
    }
    let inputs = config.draw_load_generation(generation);
    let generated = generated_spec(config, generation, inputs.spec_seed);
    common::emit_event(
        "load",
        "generation",
        Some("txgen"),
        json!({
            "generation": generation,
            "txgen_seed": inputs.txgen_seed,
            "spec_seed": inputs.spec_seed,
            "steps": load.count,
            "kind": generated.as_ref().map(|g| g.0),
            "spec_keccak": generated
                .as_ref()
                .map(|g| alloy_primitives::keccak256(g.1.as_bytes()).to_string()),
            // Trie slots are derived in the guest; recorded so a scenario can
            // pin them.
            "slots": generated
                .as_ref()
                .filter(|g| g.0 == "trie")
                .and_then(|_| config.trie())
                .map(|t| t.slots),
            // The consumer of any getrandom draw above, for the input tape.
            "pid": std::process::id(),
        }),
    );
    let mut spec = load.spec.clone();
    let mut mount = Vec::new();
    if let Some((kind, text)) = generated {
        // Next to the image's artifacts, which the spec names relatively.
        let path = Path::new(OUT_DIR).join(format!("{kind}-load-{generation}.yaml"));
        fs::write(&path, text)?;
        spec = format!("/workload/{kind}/generated.yaml");
        mount = vec!["-v".to_string(), format!("{}:{spec}:ro", path.display())];
    }
    let image = if load.image.is_empty() {
        "bedrock/tempo-txgen:latest"
    } else {
        &load.image
    };
    let status = Command::new("podman")
        .args(["run", "-d", "--name", "txgen", "--network", "host"])
        .args(["-e", "BEDROCK=0"])
        .args(["-e", &format!("TXGEN_SEED={}", inputs.txgen_seed)])
        .args(["-e", &format!("TXGEN_COUNT={}", load.count)])
        .args(["-e", &format!("TXGEN_TPS={}", load.tps)])
        .args(["-e", &format!("TXGEN_SPEC={spec}")])
        .args(&mount)
        .args(["--entrypoint", "/bin/bash", image])
        .args(["-c", "bash /workload/run.sh || true"])
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!("podman run txgen: {status}")));
    }
    Ok(())
}

/// Replaces the load after node restart `generation`. txgen pre-signs its
/// transactions with consecutive nonces, and a SIGKILL drops the node's
/// transaction pool, so the old load's remaining transactions sit behind a
/// nonce gap forever. The new generation re-reads nonces from the node.
pub fn restart_load(config: &Config, generation: u64) -> io::Result<()> {
    if config.load.count == 0 {
        return Ok(());
    }
    // Logged as a nemesis kill so workload-monitor excuses the SIGKILL.
    common::emit_event(
        "nemesis",
        "kill",
        Some("txgen"),
        json!({"reason": "reload", "generation": generation}),
    );
    for args in [
        &["kill", "-s", "KILL", "txgen"][..],
        &["wait", "txgen"],
        &["rm", "-f", "txgen"],
    ] {
        Command::new("podman")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
    }
    start_load(config, generation)
}

/// The trie load's deploy spec inside its image.
pub const TRIE_DEPLOY_SPEC: &str = "/workload/trie/deploy.yaml";

/// Runs a load image's `spec` (its setup steps) once and waits until `done`
/// holds. Run before the warm checkpoint, so every branch starts with the
/// load's contracts in place.
fn deploy_with(
    image: &str,
    spec: &str,
    what: &str,
    mut done: impl FnMut() -> Result<bool, String>,
) -> io::Result<()> {
    // run.sh's own pass check expects a pure-load run; `done` is ours.
    Command::new("podman")
        .args(["run", "--rm", "--network", "host", "-e", "BEDROCK=0"])
        .args(["-e", &format!("TXGEN_SPEC={spec}")])
        .args(["-e", "TXGEN_COUNT=1", "-e", "TXGEN_TPS=1"])
        .args(["--entrypoint", "/bin/bash", image, "/workload/run.sh"])
        .stdout(Stdio::null())
        .status()?;
    for _ in 0..60 {
        if done().map_err(io::Error::other)? {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Err(io::Error::other(format!("deploy incomplete: {what}")))
}

/// Deploys a load's contract (deploy spec `spec` inside `image`, e.g. the
/// trie load's RawStorage) and waits until `address` has code.
pub fn deploy(image: &str, address: &str, spec: &str) -> io::Result<()> {
    deploy_with(image, spec, &format!("no code at {address}"), || {
        common::has_code(address)
    })
}

/// Creates the TIP-20 load's token and mints its supply to the holders
/// (`tip20/deploy.yaml`), and waits until the latest block holds exactly
/// `tip20::Ledger::initial()`.
pub fn deploy_tip20(image: &str) -> io::Result<()> {
    use crate::tip20::{Ledger, Tip20Chain};
    let mut chain = crate::oracle::RpcChain;
    deploy_with(
        image,
        "/workload/tip20/deploy.yaml",
        "TIP-20 holders do not hold the minted supply",
        || {
            let head = common::head_number()?;
            let Some(block) = chain.block(head)? else {
                return Ok(false);
            };
            // The token does not exist until its create lands.
            Ok(chain.ledger(block.hash).ok() == Some(Ledger::initial()))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{LoadGeneration, PlannedKill};
    use crate::nemesis;

    fn config(json: &str) -> Config {
        let c: Config = serde_json::from_str(json).unwrap();
        c.validate().unwrap();
        c
    }

    #[test]
    fn explicit_nemesis_plan_wins_but_still_draws() {
        let c = config(
            r#"{"seed": 3, "run_secs": 300,
                "nemesis": {"enabled": true, "max_kills": 3, "min_gap_secs": 20},
                "nemesis_plan": [{"at_secs": 11, "down_secs": 2}, {"at_secs": 40, "down_secs": 0}]}"#,
        );
        let mut draws = 0;
        let kills = nemesis::kills_for(&c, |lo, _| {
            draws += 1;
            lo
        });
        assert_eq!(
            kills
                .iter()
                .map(|k| (k.at_secs, k.down_secs))
                .collect::<Vec<_>>(),
            [(11, 2), (40, 0)]
        );
        // The same randomness is consumed as without a plan.
        let derived = Config {
            nemesis_plan: None,
            ..c.clone()
        };
        let mut plain_draws = 0;
        let drawn = nemesis::kills_for(&derived, |lo, _| {
            plain_draws += 1;
            lo
        });
        assert_eq!(draws, plain_draws);
        assert_ne!(drawn, kills);
        // An empty explicit plan means no kills.
        let none = Config {
            nemesis_plan: Some(vec![]),
            ..c
        };
        assert!(nemesis::kills_for(&none, |lo, _| lo).is_empty());
    }

    #[test]
    fn overlapping_explicit_plans_are_rejected() {
        let kill = |at_secs, down_secs| PlannedKill { at_secs, down_secs };
        let mut c = Config {
            nemesis_plan: Some(vec![kill(30, 3), kill(32, 1)]),
            ..Config::default()
        };
        assert!(c.validate().unwrap_err().contains("nemesis_plan[1]"));
        c.nemesis_plan.as_mut().unwrap()[1].at_secs = 33;
        assert!(c.validate().is_ok());
        assert!(run(
            r#"{"nemesis_plan": [{"at_secs": 9, "down_secs": 3}, {"at_secs": 10, "down_secs": 0}]}"#
        )
        .is_err());
    }

    #[test]
    fn explicit_load_generations_drive_seeds_and_specs() {
        let c = config(
            r#"{"seed": 3, "tip20": true,
                "load": {"seed": 99, "count": 20, "tps": 5,
                         "generations": [{"txgen_seed": 7, "spec_seed": 1234}]}}"#,
        );
        assert_eq!(
            c.load_generation(0),
            LoadGeneration {
                txgen_seed: 7,
                spec_seed: 1234
            }
        );
        // Past the explicit ones: derived as before.
        assert_eq!(
            c.load_generation(2),
            LoadGeneration {
                txgen_seed: 101,
                spec_seed: 3
            }
        );
        let (kind, text) = generated_spec(&c, 0, c.load_generation(0).spec_seed).unwrap();
        assert_eq!(kind, "tip20");
        assert_eq!(text, tip20::spec(1234, 0, 20));
        assert_eq!(
            generated_spec(&c, 2, c.load_generation(2).spec_seed)
                .unwrap()
                .1,
            tip20::spec(3, 2, 20)
        );

        let chain = config(
            r#"{"seed": 3, "cob": {"address": "0x0"},
                "load": {"count": 4, "generations": [{"txgen_seed": 1, "spec_seed": 9}]}}"#,
        );
        assert_eq!(
            generated_spec(&chain, 0, 9).unwrap().1,
            cob_gen::spec(9, 0, 4)
        );
    }

    #[test]
    fn explicit_trie_slots_still_generate_the_spec() {
        // A scenario's recorded slots with `generated`: the spec is
        // trie_gen's over exactly those slots, not the seed's.
        let c = config(
            r#"{"seed": 3, "trie": {"address": "0x0", "slots": [5, 6, 70000], "generated": true},
                "load": {"count": 10, "generations": [{"txgen_seed": 1, "spec_seed": 3}]}}"#,
        );
        assert_eq!(c.trie().unwrap().slots, [5, 6, 70000]);
        let (kind, text) = generated_spec(&c, 0, c.load_generation(0).spec_seed).unwrap();
        assert_eq!(kind, "trie");
        assert_eq!(text, trie_gen::spec(3, 0, &[5, 6, 70000], 10));
        // Without slots they come from the seed, as before.
        let derived = config(r#"{"seed": 3, "trie": {"address": "0x0"}, "load": {"count": 10}}"#);
        assert_eq!(derived.trie().unwrap().slots, trie_gen::slots(3));
        // Hand-written slots without `generated` keep the image's spec.
        let manual = config(
            r#"{"seed": 3, "trie": {"address": "0x0", "slots": [1]}, "load": {"count": 10}}"#,
        );
        assert!(generated_spec(&manual, 0, 3).is_none());
    }

    #[test]
    fn derived_configs_serialize_without_explicit_fields() {
        let c = config(r#"{"seed": 3, "trie": {"address": "0x0"}, "load": {"count": 10}}"#);
        let s = serde_json::to_string(&c).unwrap();
        for key in ["nemesis_plan", "generations", "generated"] {
            assert!(!s.contains(key), "{key} in {s}");
        }
    }
}
