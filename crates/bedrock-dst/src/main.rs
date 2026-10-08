// SPDX-License-Identifier: GPL-2.0

//! Seeded DST campaigns for the Tempo workload (workloads/tempo-dst).
//!
//! Boots the guest once, runs until the node has produced `--warm-blocks`
//! blocks, and checkpoints. Each seed then forks a branch whose controlled
//! randomness (RDRAND, getrandom: thread schedules and nemesis timing) is a
//! pure function of the seed, starts the in-guest oracles, nemesis, and load,
//! runs `--run-secs` of virtual time, finalizes, and collects artifacts:
//!
//! ```text
//! <out>/campaign.json            arguments, for replay
//! <out>/seed-<n>/config.json     /bedrock/in/config.json as delivered
//! <out>/seed-<n>/assertions.jsonl, events.jsonl, finalize.json, *.log
//! <out>/seed-<n>/console.log     guest serial output for the branch
//! <out>/seed-<n>/verdict.json
//! <out>/summary.json
//! ```
//!
//! `replay <out>/seed-<n>` reruns one seed from scratch and checks the
//! collected artifacts are byte-identical.

mod verdict;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bedrock_lab::{
    BashTarget, Branch, Checkpoint, Event, EventSink, LabOpts, RngMode, RunOutcome, VirtDuration,
    VirtTime,
};
use bedrock_vm::{load_kernel, LinuxBootConfig, VmBuilder, DEFAULT_TSC_FREQUENCY};
use clap::{Args as ClapArgs, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Guest files collected per run, relative to /bedrock.
const COLLECT: &[&str] = &[
    "events.jsonl",
    "out/finalize.json",
    "out/oracle.log",
    "out/nemesis.log",
];
/// Artifacts compared by `replay`.
const REPLAY_COMPARE: &[&str] = &[
    "assertions.jsonl",
    "events.jsonl",
    "finalize.json",
    "verdict.json",
];
/// Stays under bedrock-io's 256 KiB output buffer.
const CHUNK: usize = 200_000;
/// Guest memory snapshot, written to `guest-mem.txt` at the warm checkpoint and
/// after each seed: Bedrock commits all of `--memory-mb` on the host, so this is
/// what sizes it. tmpfs (root, container storage, journal) counts as Shmem.
const MEM_REPORT: &str = "free -m; df -m / /run /tmp /dev/shm; \
    grep -E '^(MemAvailable|Shmem|Committed_AS):' /proc/meminfo; \
    ps -eo rss=,comm= --sort=-rss | head -8";

#[derive(Parser)]
#[command(
    name = "bedrock-dst",
    about = "Seeded DST campaigns over a warm checkpoint"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run seeds `[seed_start, seed_start + seeds)`.
    Campaign(Box<CampaignArgs>),
    /// Rerun one seed of a finished campaign and compare its artifacts.
    Replay { run_dir: PathBuf },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Load {
    Transfers,
    Trie,
}

/// Trie load image (workloads/tempo-dst/trie).
const TRIE_IMAGE: &str = "bedrock/tempo-dst-trie:latest";
/// RawStorage deployed by trie/deploy.yaml during warmup: dev account 0's
/// first transaction on a fresh chain (CREATE address of nonce 0).
const TRIE_CONTRACT: &str = "0x5fbdb2315678afecb367f032d93f642f64180aa3";

#[derive(ClapArgs, Clone, Serialize, Deserialize)]
struct CampaignArgs {
    #[arg(long)]
    vmlinux: PathBuf,
    #[arg(long)]
    initrd: PathBuf,
    /// Compose file served to the guest as compose.yaml.
    #[arg(long)]
    compose: PathBuf,
    /// Image archive served to the guest as images.tar.
    #[arg(long)]
    images: PathBuf,
    #[arg(
        long,
        default_value = "console=hvc0 nopti nokaslr mitigations=off break audit=0 bedrock_ncpus=5"
    )]
    cmdline: String,
    #[arg(long, default_value_t = 16384)]
    memory_mb: usize,
    /// RDRAND seed for the shared boot and warmup prefix.
    #[arg(long, default_value_t = 0xbed0_7e3b)]
    boot_seed: u64,
    #[arg(long, default_value_t = 0)]
    seed_start: u64,
    /// txgen's transaction-generation seed, shared by every branch so seeds
    /// vary faults and schedules over a fixed workload.
    #[arg(long, default_value_t = 99)]
    workload_seed: u64,
    #[arg(long, default_value_t = 1)]
    seeds: u64,
    /// Virtual seconds per branch before finalize.
    #[arg(long, default_value_t = 180)]
    run_secs: u64,
    /// Head the node must reach before the warm checkpoint.
    #[arg(long, default_value_t = 10)]
    warm_blocks: u64,
    /// Virtual seconds allowed for boot plus warmup.
    #[arg(long, default_value_t = 900)]
    warm_timeout_secs: u64,
    #[arg(long, default_value_t = 3)]
    max_kills: u32,
    #[arg(long, default_value_t = 30)]
    min_gap_secs: u32,
    /// Disable the crash nemesis.
    #[arg(long)]
    no_nemesis: bool,
    #[arg(long, default_value_t = 60)]
    liveness_secs: u64,
    /// Transfers for the txgen load container (0 disables load).
    #[arg(long, default_value_t = 20_000)]
    txgen_count: u64,
    #[arg(long, default_value_t = 100)]
    txgen_tps: u64,
    /// Load: `transfers` (pathUSD transfers) or `trie` (RawStorage writes over
    /// slot shapes generated per seed, checked by E5/E6; uses --trie-tps).
    #[arg(long, value_enum, default_value_t = Load::Transfers)]
    load: Load,
    /// Trie load rate; ~5 tx/s puts about one step in each 200 ms block.
    #[arg(long, default_value_t = 5)]
    trie_tps: u64,
    #[arg(long, default_value = "dst-out")]
    out: PathBuf,
}

/// Writes guest serial lines to the current run's console.log.
#[derive(Default)]
struct ConsoleSink {
    file: Mutex<Option<File>>,
}

impl ConsoleSink {
    fn open(&self, path: &Path) -> Result<()> {
        *self.file.lock().unwrap() = Some(File::create(path)?);
        Ok(())
    }
}

impl EventSink for ConsoleSink {
    fn on_event(&self, event: Event<'_>) {
        if let Event::SerialLine { at, line, .. } = event {
            if let Some(f) = self.file.lock().unwrap().as_mut() {
                let _ = writeln!(
                    f,
                    "[{:>9.3}] {}",
                    at.as_secs_f64(),
                    String::from_utf8_lossy(line)
                );
            }
        }
    }
}

fn secs(s: u64) -> VirtDuration {
    VirtDuration::from_secs(s, DEFAULT_TSC_FREQUENCY)
}

fn host_bash(branch: &mut Branch, cmd: &str) -> Result<(i32, String)> {
    let out = branch.bash(BashTarget::host(), cmd, true)?;
    Ok((out.exit_code, out.output_lossy().into_owned()))
}

/// Reads a guest file from `offset` in chunks that fit the I/O output buffer.
fn fetch(branch: &mut Branch, path: &str, offset: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    loop {
        let cmd = format!(
            "tail -c +{} {path} 2>/dev/null | head -c {CHUNK}",
            offset + data.len() + 1
        );
        let out = branch.bash(BashTarget::host(), &cmd, true)?;
        let n = out.output.len();
        data.extend_from_slice(&out.output);
        if n < CHUNK {
            return Ok(data);
        }
    }
}

fn boot(args: &CampaignArgs, sink: Arc<ConsoleSink>) -> Result<Checkpoint> {
    let mut vm = VmBuilder::new().memory_mb(args.memory_mb).build()?;
    let kernel = fs::read(&args.vmlinux)?;
    let initrd = fs::read(&args.initrd)?;
    let (entry, end) = load_kernel(vm.memory_mut()?, &kernel)?;
    let boot = LinuxBootConfig::new(entry, end)
        .cmdline(&args.cmdline)
        .initramfs(&initrd);
    vm.setup_linux_boot(&boot)?;
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let deadline = VirtTime::from_secs(args.warm_timeout_secs, DEFAULT_TSC_FREQUENCY);
    let ready = Checkpoint::initial_when_ready_with(
        vm,
        deadline,
        LabOpts {
            sink,
            rng: RngMode::Seeded(args.boot_seed),
            files: vec![
                ("compose.yaml".into(), path(&args.compose)),
                ("images.tar".into(), path(&args.images)),
            ],
            ..Default::default()
        },
    )?;
    println!("ready at vt {:.1}s", ready.time().as_secs_f64());

    let mut warm = ready.branch()?;
    loop {
        if warm.current_time() >= deadline {
            return Err(format!(
                "node did not reach {} blocks before warm timeout",
                args.warm_blocks
            )
            .into());
        }
        warm.run_for(secs(5))?;
        let (code, out) = host_bash(&mut warm, "tempo-dst head")?;
        let head = out.trim().parse::<u64>().ok().filter(|_| code == 0);
        println!(
            "warmup vt {:.1}s head {head:?}",
            warm.current_time().as_secs_f64()
        );
        if head.is_some_and(|h| h >= args.warm_blocks) {
            break;
        }
    }
    if args.load == Load::Trie {
        // Deploy before the checkpoint so branches start writing at once.
        let (code, out) = host_bash(
            &mut warm,
            &format!("tempo-dst deploy {TRIE_IMAGE} {TRIE_CONTRACT}"),
        )?;
        if code != 0 {
            return Err(format!("trie contract deploy failed ({code}): {out}").into());
        }
        println!(
            "trie contract deployed at vt {:.1}s",
            warm.current_time().as_secs_f64()
        );
    }
    let (_, mem) = host_bash(&mut warm, MEM_REPORT)?;
    fs::write(args.out.join("guest-mem.txt"), mem)?;
    let cp = warm.checkpoint()?;
    println!("warm checkpoint at vt {:.1}s", cp.time().as_secs_f64());
    Ok(cp)
}

fn guest_config(args: &CampaignArgs, seed: u64) -> serde_json::Value {
    json!({
        "seed": seed,
        "workload_seed": args.workload_seed,
        "run_secs": args.run_secs,
        "liveness_secs": args.liveness_secs,
        "nemesis": {
            "enabled": !args.no_nemesis,
            "max_kills": args.max_kills,
            "min_gap_secs": args.min_gap_secs,
        },
        "load": match args.load {
            Load::Transfers => json!({
                "seed": args.workload_seed,
                "count": args.txgen_count,
                "tps": args.txgen_tps,
            }),
            Load::Trie => json!({
                "seed": args.workload_seed,
                // Enough steps to outlast the run.
                "count": (args.run_secs + 60) * args.trie_tps,
                "tps": args.trie_tps,
                "image": TRIE_IMAGE,
            }),
        },
        "trie": (args.load == Load::Trie).then(|| json!({
            "address": TRIE_CONTRACT,
        })),
    })
}

/// Coverage every seed must reach (see `verdict`): the load landed, and with
/// the trie load, its checks ran over every slot shape.
fn required(args: &CampaignArgs) -> Vec<&'static str> {
    match args.load {
        Load::Transfers if args.txgen_count == 0 => vec![],
        Load::Transfers => vec!["S/load-included"],
        Load::Trie => vec!["S/load-included", "S/trie-checked"],
    }
}

fn run_seed(
    args: &CampaignArgs,
    warm: &Checkpoint,
    sink: &ConsoleSink,
    seed: u64,
    dir: &Path,
) -> Result<verdict::Verdict> {
    fs::create_dir_all(dir)?;
    sink.open(&dir.join("console.log"))?;
    // Per-seed randomness: RDRAND and guest getrandom() come from the
    // hypervisor's in-VM PRNG, reseeded per branch.
    let mut b = warm.branch()?;
    b.reseed_rng(seed)?;
    println!(
        "seed {seed}: branch forked at vt {:.1}s",
        b.current_time().as_secs_f64()
    );
    let start = b.current_time();

    let config = serde_json::to_string(&guest_config(args, seed))?;
    fs::write(dir.join("config.json"), &config)?;
    // JSON from guest_config never contains a single quote.
    let (code, out) = host_bash(&mut b, &format!("tempo-dst start '{config}'"))?;
    println!(
        "seed {seed}: started at vt {:.1}s",
        b.current_time().as_secs_f64()
    );
    let offset: usize = out
        .trim()
        .parse()
        .map_err(|_| format!("tempo-dst start failed ({code}): {out}"))?;

    let end = start + secs(args.run_secs);
    let mut guest_exit = None;
    while b.current_time() < end {
        match b.run_until(end)?.1 {
            RunOutcome::ReachedTime => break,
            RunOutcome::Ready | RunOutcome::ActionResponse { .. } => {}
            RunOutcome::RngExhausted => return Err("seed source exhausted".into()),
            RunOutcome::Yielded { kind } => {
                guest_exit = Some(format!("{kind:?}"));
                break;
            }
        }
    }

    let mut assertions = Vec::new();
    if guest_exit.is_none() {
        host_bash(&mut b, "tempo-dst finalize")?;
        assertions = fetch(&mut b, "/bedrock/assertions.jsonl", offset)?;
        for f in COLLECT {
            let data = fetch(&mut b, &format!("/bedrock/{f}"), 0)?;
            fs::write(dir.join(Path::new(f).file_name().unwrap()), data)?;
        }
        let (_, mem) = host_bash(&mut b, MEM_REPORT)?;
        fs::write(dir.join("guest-mem.txt"), mem)?;
    }
    if let Some(kind) = &guest_exit {
        // The guest stopped under us (e.g. kernel panic or shutdown): a failure
        // in its own right; the guest can no longer report anything.
        let rec = json!({"Always": {"condition": {"Bool": false}, "result": false,
            "message": format!("D/guest-exited: {kind}"),
            "location": {"file": "bedrock-dst", "line": 0, "column": 0}}});
        assertions.extend_from_slice(format!("{rec}\n").as_bytes());
    }
    fs::write(dir.join("assertions.jsonl"), &assertions)?;
    let v = verdict::aggregate(&String::from_utf8_lossy(&assertions), &required(args));
    fs::write(dir.join("verdict.json"), serde_json::to_vec_pretty(&v)?)?;
    Ok(v)
}

fn campaign(args: CampaignArgs) -> Result<()> {
    fs::create_dir_all(&args.out)?;
    fs::write(
        args.out.join("campaign.json"),
        serde_json::to_vec_pretty(&args)?,
    )?;
    let sink = Arc::new(ConsoleSink::default());
    sink.open(&args.out.join("boot-console.log"))?;
    let wall = Instant::now();
    let warm = boot(&args, sink.clone())?;
    let mut summary = BTreeMap::new();
    for seed in args.seed_start..args.seed_start + args.seeds {
        let t = Instant::now();
        let v = run_seed(
            &args,
            &warm,
            &sink,
            seed,
            &args.out.join(format!("seed-{seed}")),
        )?;
        println!(
            "seed {seed}: {} ({:.0}s wall) {:?}",
            if v.pass { "PASS" } else { "FAIL" },
            t.elapsed().as_secs_f64(),
            v.failures.keys().collect::<Vec<_>>()
        );
        summary.insert(
            seed,
            json!({"pass": v.pass, "failures": v.failures.keys().collect::<Vec<_>>()}),
        );
    }
    let hours = wall.elapsed().as_secs_f64() / 3600.0;
    let report = json!({
        "seeds": summary,
        "failed": summary.values().filter(|v| v["pass"] == false).count(),
        "wall_secs": wall.elapsed().as_secs(),
        "seeds_per_hour": args.seeds as f64 / hours.max(1e-9),
    });
    fs::write(
        args.out.join("summary.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn replay(run_dir: &Path) -> Result<()> {
    let out = run_dir.parent().ok_or("run dir has no parent")?;
    let mut args: CampaignArgs = serde_json::from_slice(&fs::read(out.join("campaign.json"))?)?;
    let seed: u64 = run_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("seed-"))
        .and_then(|n| n.parse().ok())
        .ok_or("run dir must be named seed-<n>")?;
    let replay_out = run_dir.join("replay");
    args.out = replay_out.clone();
    args.seed_start = seed;
    args.seeds = 1;
    campaign(args)?;
    let mut diverged = Vec::new();
    for f in REPLAY_COMPARE {
        let a = fs::read(run_dir.join(f)).unwrap_or_default();
        let b = fs::read(replay_out.join(format!("seed-{seed}")).join(f)).unwrap_or_default();
        if a != b {
            diverged.push(*f);
        }
    }
    if diverged.is_empty() {
        println!("replay of seed {seed}: identical");
        Ok(())
    } else {
        Err(format!("replay of seed {seed} diverged in {diverged:?}").into())
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Campaign(args) => campaign(*args),
        Cmd::Replay { run_dir } => replay(&run_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_config_round_trips_into_shell_quoting() {
        let Cmd::Campaign(args) = Cli::parse_from([
            "x",
            "campaign",
            "--vmlinux",
            "k",
            "--initrd",
            "i",
            "--compose",
            "c",
            "--images",
            "t",
            "--no-nemesis",
        ])
        .cmd
        else {
            unreachable!()
        };
        let c = guest_config(&args, 7);
        assert_eq!(c["seed"], 7);
        assert_eq!(c["workload_seed"], 99);
        assert_eq!(c["nemesis"]["enabled"], false);
        assert!(!c.to_string().contains('\''));
        assert_eq!(c["trie"], serde_json::Value::Null);
    }

    #[test]
    fn trie_load_configures_image_and_oracle() {
        let Cmd::Campaign(args) = Cli::parse_from([
            "x",
            "campaign",
            "--vmlinux",
            "k",
            "--initrd",
            "i",
            "--compose",
            "c",
            "--images",
            "t",
            "--load",
            "trie",
        ])
        .cmd
        else {
            unreachable!()
        };
        let c = guest_config(&args, 3);
        assert_eq!(c["load"]["image"], "bedrock/tempo-dst-trie:latest");
        assert_eq!(c["load"]["tps"], 5);
        // Slots and the write stream are generated in the guest from the seed.
        assert!(c["trie"].get("slots").is_none());
        assert!(!c.to_string().contains('\''));
    }
}
