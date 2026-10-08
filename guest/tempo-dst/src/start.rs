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
use crate::trie_gen;

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
/// (`trie_gen::spec`) over the run's slots.
///
/// Load only: failures (e.g. while the node is down) are not oracles. The
/// oracle's S/load-included shows the load actually landed.
pub fn start_load(config: &Config, generation: u64) -> io::Result<()> {
    let load = &config.load;
    if load.count == 0 {
        return Ok(());
    }
    let mut spec = load.spec.clone();
    let mut mount = Vec::new();
    if let Some(trie) = config
        .trie
        .as_ref()
        .filter(|t| t.generated())
        .and(config.trie())
    {
        let path = Path::new(OUT_DIR).join(format!("trie-load-{generation}.yaml"));
        let steps = usize::try_from(load.count).unwrap_or(usize::MAX);
        fs::write(
            &path,
            trie_gen::spec(config.seed, generation, &trie.slots, steps),
        )?;
        spec = "/workload/trie/generated.yaml".into();
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

/// Deploys the trie load's RawStorage contract (`deploy.yaml`, from the trie
/// load image) and waits until `address` has code. Run before the warm
/// checkpoint, so every branch starts with the contract in place.
pub fn deploy(image: &str, address: &str) -> io::Result<()> {
    // run.sh's own pass check expects a pure-load run; the code check is ours.
    Command::new("podman")
        .args(["run", "--rm", "--network", "host", "-e", "BEDROCK=0"])
        .args(["-e", "TXGEN_SPEC=/workload/trie/deploy.yaml"])
        .args(["-e", "TXGEN_COUNT=1", "-e", "TXGEN_TPS=1"])
        .args(["--entrypoint", "/bin/bash", image, "/workload/run.sh"])
        .stdout(Stdio::null())
        .status()?;
    for _ in 0..60 {
        if common::has_code(address).map_err(io::Error::other)? {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Err(io::Error::other(format!("no code at {address}")))
}
