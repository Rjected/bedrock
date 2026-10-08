// SPDX-License-Identifier: GPL-2.0

//! Branch setup, invoked once by the host driver right after forking: installs
//! the run config, resets the event log, starts the oracle, nemesis, and load,
//! and prints the assertion-log offset the run's records start at.

use std::fs::{self, File};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

use crate::common::{self, Config, ASSERTIONS_PATH, CONFIG_PATH, EVENTS_PATH, OUT_DIR};
use crate::{cob_gen, tip20, trie_gen};

/// Starts `tempo-dst <sub>` in its own session so it outlives the I/O-channel
/// command that launched it.
fn spawn_detached(sub: &str) -> io::Result<()> {
    let log = File::create(Path::new(OUT_DIR).join(format!("{sub}.log")))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg(sub)
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
    fs::create_dir_all(Path::new(CONFIG_PATH).parent().unwrap())?;
    fs::create_dir_all(OUT_DIR)?;
    fs::write(CONFIG_PATH, config_json)?;
    fs::write(EVENTS_PATH, "")?;
    let offset = fs::metadata(ASSERTIONS_PATH).map_or(0, |m| m.len());

    spawn_detached("oracle")?;
    spawn_detached("nemesis")?;
    start_load(&config, 0)?;
    println!("{offset}");
    Ok(())
}

/// Starts the txgen load as container `txgen`. Generation `n` (0 at branch
/// start, then one per node restart) draws its transactions from workload seed
/// `load.seed + n`; a generated trie load also gets a fresh write stream
/// (`trie_gen::spec`) over the run's slots, the TIP-20 load a fresh transfer
/// stream (`tip20::spec`), and a chain-of-blocks load fresh payloads
/// (`cob_gen::spec`).
///
/// Load only: failures (e.g. while the node is down) are not oracles. The
/// oracle's S/load-included shows the load actually landed.
pub fn start_load(config: &Config, generation: u64) -> io::Result<()> {
    let load = &config.load;
    if load.count == 0 {
        return Ok(());
    }
    let steps = usize::try_from(load.count).unwrap_or(usize::MAX);
    let generated = if let Some(trie) = config
        .trie
        .as_ref()
        .filter(|t| t.generated())
        .and(config.trie())
    {
        Some((
            "trie",
            trie_gen::spec(config.seed, generation, &trie.slots, steps),
        ))
    } else if config.tip20 {
        Some(("tip20", tip20::spec(config.seed, generation, steps)))
    } else if config.cob.is_some() {
        Some(("chain", cob_gen::spec(config.seed, generation, steps)))
    } else {
        None
    };
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
        .args(["-e", &format!("TXGEN_SEED={}", load.seed + generation)])
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
