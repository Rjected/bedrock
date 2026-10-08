// SPDX-License-Identifier: GPL-2.0

//! Seeded DST campaigns for the Tempo workload (workloads/tempo-dst).
//!
//! Boots the guest once, runs until the node has produced `--warm-blocks`
//! blocks, and checkpoints. Each seed then forks a branch whose controlled
//! randomness (RDRAND, getrandom: thread schedules and nemesis timing) and
//! swarm choices ([`swarm_for`], e.g. forced preemption) are pure functions of
//! the seed, starts the in-guest oracles, nemesis, and load,
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
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bedrock_lab::{
    BashTarget, Branch, Checkpoint, Event, EventSink, LabError, LabOpts, RngMode, RunOutcome,
    VirtDuration, VirtTime,
};
use bedrock_vm::{
    load_kernel, LinuxBootConfig, PreemptConfig, VmBuilder, VmError, DEFAULT_TSC_FREQUENCY,
};
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
    "config.json",
    "assertions.jsonl",
    "events.jsonl",
    "finalize.json",
    "verdict.json",
];
/// Forced-preemption periods (guest instructions) a seed draws from; 0 = off.
///
/// The emulated TSC counts retired guest instructions at 2.9952 GHz, so one
/// virtual ms is ~3M instructions, about one HZ=1000 tick. A period P gives
/// gaps in `[P, 2P)` (mean 1.5P): 200k is ~10 extra preemptions per tick
/// (dense: lands inside short critical sections and spin loops), 2M about one
/// per tick (doubles the preemption rate), 20M about one per 10 ticks (sparse
/// perturbation of long runs). Preemptions land on the next deterministic
/// exit past the deadline, so the effective rate is also bounded by the exit
/// rate.
const PREEMPT_PERIODS: &[u64] = &[0, 200_000, 2_000_000, 20_000_000];
/// Salt separating the preemption draw from the seed's other uses.
const PREEMPT_SALT: u64 = 0x7072_6565_6d70_7431; // "preempt1"

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Per-seed forced preemption `(period, seed)`. `(0, 0)` when disabled by
/// `--no-preempt`. Part of [`swarm_for`].
fn preempt_for(args: &CampaignArgs, seed: u64) -> (u64, u64) {
    if args.no_preempt {
        return (0, 0);
    }
    let r = splitmix64(seed ^ PREEMPT_SALT);
    let period = PREEMPT_PERIODS[(r % PREEMPT_PERIODS.len() as u64) as usize];
    if period == 0 {
        return (0, 0);
    }
    (period, splitmix64(r))
}

/// One seed's swarm record: every feature that shapes the run, so verdicts
/// can be grouped by feature. Recorded as `"swarm"` in the per-seed
/// `config.json` and `verdict.json` (both compared by `replay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Swarm {
    /// Drawn per seed: forced-preemption period (guest instructions; 0 =
    /// off) and its jitter seed, applied by the driver (`Branch::set_preempt`).
    preempt: Preempt,
    /// Campaign-wide, recorded so a verdict names its whole configuration.
    load: Load,
    reference: bool,
    nemesis: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Preempt {
    period: u64,
    seed: u64,
}

/// The swarm choices for `seed`: a pure function of the campaign arguments
/// and the seed, drawn with a host-side PRNG (never guest randomness), so
/// `replay` re-derives them. In-guest load generators (trie, TIP-20, chain
/// specs) key off the same seed via the delivered config.
fn swarm_for(args: &CampaignArgs, seed: u64) -> Swarm {
    let (period, preempt_seed) = preempt_for(args, seed);
    Swarm {
        preempt: Preempt {
            period,
            seed: preempt_seed,
        },
        load: args.load,
        reference: args.reference,
        nemesis: !args.no_nemesis,
    }
}

/// `ENOTTY` (asm-generic errno): the loaded bedrock.ko has no such ioctl.
const ENOTTY: i32 = 25;

/// Turns a missing SET_PREEMPT_CONFIG ioctl into an actionable error.
fn preempt_error(e: &io::Error) -> Box<dyn Error> {
    if e.raw_os_error() == Some(ENOTTY) {
        "bedrock.ko lacks SET_PREEMPT_CONFIG; reload the module built from this tree \
         or pass --no-preempt"
            .into()
    } else {
        format!("SET_PREEMPT_CONFIG failed: {e}").into()
    }
}

/// Applies a seed's forced preemption to its branch. A zero period never
/// calls the ioctl: a branch inherits the warm checkpoint's (disabled)
/// setting, and `--no-preempt` must work on a module without the ioctl.
fn apply_preempt(b: &mut Branch, p: Preempt) -> Result<()> {
    if p.period == 0 {
        return Ok(());
    }
    b.set_preempt(p.period, p.seed).map_err(|e| match &e {
        LabError::Vm(VmError::Ioctl { source, .. }) => preempt_error(source),
        _ => e.into(),
    })
}

/// Whether any seed of the campaign enables forced preemption.
fn any_preempt(args: &CampaignArgs) -> bool {
    (args.seed_start..args.seed_start.saturating_add(args.seeds))
        .any(|s| swarm_for(args, s).preempt.period != 0)
}

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
    Tip20,
    Chain,
}

/// Trie load image (workloads/tempo-dst/trie).
const TRIE_IMAGE: &str = "bedrock/tempo-dst-trie:latest";
/// RawStorage deployed by trie/deploy.yaml during warmup: dev account 0's
/// first transaction on a fresh chain (CREATE address of nonce 0).
const TRIE_CONTRACT: &str = "0x5fbdb2315678afecb367f032d93f642f64180aa3";
/// TIP-20 load image (workloads/tempo-dst/tip20); its token is created and
/// minted during warmup.
const TIP20_IMAGE: &str = "bedrock/tempo-dst-tip20:latest";
/// Chain-of-blocks load image (workloads/tempo-dst/chain).
const CHAIN_IMAGE: &str = "bedrock/tempo-dst-chain:latest";
/// ChainOfBlocks deployed by chain/deploy.yaml during warmup: dev account 9's
/// first transaction.
const CHAIN_CONTRACT: &str = "0x700b6a60ce7eaaea56f065753d8dcb9653dbad35";
/// Nemesis warmup before the first kill (guest `nemesis::WARMUP_SECS`) plus
/// its longest downtime (`MAX_DOWN_SECS`).
const NEMESIS_SLACK_SECS: u64 = 5 + 3;

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
    /// Disable per-seed forced preemption (see `PREEMPT_PERIODS`).
    #[arg(long)]
    #[serde(default)]
    no_preempt: bool,
    #[arg(long, default_value_t = 60)]
    liveness_secs: u64,
    /// Transfers for the txgen load container (0 disables load).
    #[arg(long, default_value_t = 20_000)]
    txgen_count: u64,
    #[arg(long, default_value_t = 100)]
    txgen_tps: u64,
    /// Load: `transfers` (pathUSD transfers), `trie` (RawStorage writes over
    /// slot shapes generated per seed, checked by E5/E6; uses --trie-tps),
    /// `tip20` (transfers among a closed set of holders of a fresh TIP-20,
    /// generated per seed, checked by E8; uses --tip20-tps), or `chain`
    /// (ChainOfBlocks appends, checked by E9; uses --chain-tps).
    #[arg(long, value_enum, default_value_t = Load::Transfers)]
    load: Load,
    /// Trie load rate; ~5 tx/s puts about one step in each 200 ms block.
    #[arg(long, default_value_t = 5)]
    trie_tps: u64,
    /// TIP-20 load rate; ~50 tx/s puts about ten transfers in each block.
    #[arg(long, default_value_t = 50)]
    tip20_tps: u64,
    /// Chain load rate; ~10 tx/s puts about two appends in each 200 ms block.
    #[arg(long, default_value_t = 10)]
    chain_tps: u64,
    /// Run the E7 reference node (compose service `tempo-ref`; run.sh keeps
    /// it in the compose file only with this flag).
    #[arg(long)]
    #[serde(default)]
    reference: bool,
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
    if any_preempt(args) {
        // Probe for the ioctl before the long boot. Disabled is a fresh VM's
        // state, so this leaves the guest untouched.
        vm.set_preempt_config(&PreemptConfig::disabled())
            .map_err(|e| preempt_error(&e))?;
    }
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
    let deploy = match args.load {
        // The TIP-20 token is created and minted below, not by address.
        Load::Transfers | Load::Tip20 => None,
        Load::Trie => Some((TRIE_IMAGE, TRIE_CONTRACT, "/workload/trie/deploy.yaml")),
        Load::Chain => Some((CHAIN_IMAGE, CHAIN_CONTRACT, "/workload/chain/deploy.yaml")),
    };
    if let Some((image, contract, spec)) = deploy {
        // Deploy before the checkpoint so branches start writing at once.
        let (code, out) = host_bash(
            &mut warm,
            &format!("tempo-dst deploy {image} {contract} {spec}"),
        )?;
        if code != 0 {
            return Err(format!("{contract} deploy failed ({code}): {out}").into());
        }
        println!(
            "{contract} deployed at vt {:.1}s",
            warm.current_time().as_secs_f64()
        );
    }
    if args.load == Load::Tip20 {
        let (code, out) = host_bash(&mut warm, &format!("tempo-dst deploy-tip20 {TIP20_IMAGE}"))?;
        if code != 0 {
            return Err(format!("TIP-20 token deploy failed ({code}): {out}").into());
        }
        println!(
            "TIP-20 token minted at vt {:.1}s",
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
            Load::Tip20 => json!({
                "seed": args.workload_seed,
                "count": (args.run_secs + 60) * args.tip20_tps,
                "tps": args.tip20_tps,
                "image": TIP20_IMAGE,
            }),
            Load::Chain => json!({
                "seed": args.workload_seed,
                "count": (args.run_secs + 60) * args.chain_tps,
                "tps": args.chain_tps,
                "image": CHAIN_IMAGE,
            }),
        },
        "trie": (args.load == Load::Trie).then(|| json!({
            "address": TRIE_CONTRACT,
        })),
        "tip20": args.load == Load::Tip20,
        "cob": (args.load == Load::Chain).then(|| json!({
            "address": CHAIN_CONTRACT,
        })),
        "reference": args.reference,
        // Driver-side choices (e.g. preemption, applied by
        // `Branch::set_preempt`), recorded so the run is self-describing and
        // `replay` compares them. The guest ignores this key.
        "swarm": swarm_for(args, seed),
    })
}

/// Whether every seed's nemesis plan has at least one kill and its restart:
/// the first kill lands by warmup + one gap, and needs its downtime plus a
/// full gap after it for recovery (see `tempo-dst`'s `nemesis::plan`).
fn kills_planned(args: &CampaignArgs) -> bool {
    !args.no_nemesis
        && args.max_kills > 0
        && args.run_secs >= 2 * u64::from(args.min_gap_secs.max(1)) + NEMESIS_SLACK_SECS
}

/// Coverage every seed must reach (see `verdict`): the load landed, and its
/// oracle checked it. With the trie load, its checks ran. With the TIP-20
/// load: blocks carried several transfers, and a snapshot pinned while in
/// memory read the same after it was persisted and, when the plan guarantees
/// a kill, after a restart. With the chain load, an append made during the
/// run was checked against state and, when the run kills the node, the chain
/// grew past its pre-kill length after a restart. With `--reference`, the
/// reference node caught up and was compared.
fn required(args: &CampaignArgs) -> Vec<&'static str> {
    let mut required = match args.load {
        Load::Transfers if args.txgen_count == 0 => vec![],
        Load::Transfers => vec!["S/load-included"],
        Load::Trie => vec!["S/load-included", "S/trie-checked"],
        Load::Tip20 => {
            let mut r = vec![
                "S/load-included",
                "S/tip20-checked",
                "S/tip20-transfers-in-block",
                "S/tip20-pinned-read-survived-persistence",
            ];
            if kills_planned(args) {
                r.push("S/tip20-pinned-read-survived-restart");
            }
            r
        }
        Load::Chain if kills_planned(args) => vec![
            "S/load-included",
            "S/chain-appended",
            "S/chain-survived-restart",
        ],
        Load::Chain => vec!["S/load-included", "S/chain-appended"],
    };
    if args.reference {
        required.extend(["S/reference-compared", "S/reference-synced"]);
    }
    required
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
    // Per-seed forced preemption, scoped to this branch; it is driver-side,
    // so it applies to every load and with --reference.
    let swarm = swarm_for(args, seed);
    apply_preempt(&mut b, swarm.preempt)?;
    println!(
        "seed {seed}: branch forked at vt {:.1}s (preempt period {})",
        b.current_time().as_secs_f64(),
        swarm.preempt.period
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
    let mut v = verdict::aggregate(&String::from_utf8_lossy(&assertions), &required(args));
    v.swarm = serde_json::to_value(swarm)?;
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
            json!({
                "pass": v.pass,
                "failures": v.failures.keys().collect::<Vec<_>>(),
                "swarm": v.swarm,
            }),
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
        assert_eq!(c["reference"], false);
        assert!(!required(&args).iter().any(|s| s.contains("reference")));
    }

    #[test]
    fn reference_flag_reaches_guest_and_coverage() {
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
            "--reference",
        ])
        .cmd
        else {
            unreachable!()
        };
        assert_eq!(guest_config(&args, 1)["reference"], true);
        assert_eq!(
            required(&args),
            [
                "S/load-included",
                "S/reference-compared",
                "S/reference-synced"
            ]
        );
    }

    fn campaign_args(extra: &[&str]) -> CampaignArgs {
        let base = [
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
        ];
        let Cmd::Campaign(args) = Cli::parse_from(base.iter().chain(extra)).cmd else {
            unreachable!()
        };
        *args
    }

    #[test]
    fn preempt_is_a_stable_per_seed_swarm_feature() {
        let args = campaign_args(&[]);
        let draws: Vec<_> = (0..64).map(|s| preempt_for(&args, s)).collect();
        // Pure function of the seed.
        assert_eq!(
            draws,
            (0..64).map(|s| preempt_for(&args, s)).collect::<Vec<_>>()
        );
        // Pinned so a change to the draw (which would silently change what
        // old seeds mean) is caught.
        let periods: Vec<u64> = draws[..8].iter().map(|d| d.0).collect();
        assert_eq!(
            periods,
            [200_000, 0, 0, 0, 0, 2_000_000, 2_000_000, 20_000_000]
        );
        for &(p, sd) in &draws {
            assert!(PREEMPT_PERIODS.contains(&p));
            assert_eq!(p == 0, sd == 0, "seed is set exactly when enabled");
        }
        // Every magnitude, including off, appears across 64 seeds.
        for p in PREEMPT_PERIODS {
            assert!(draws.iter().any(|d| d.0 == *p), "period {p} never drawn");
        }

        // Recorded in the delivered config's swarm record.
        let c = guest_config(&args, 5);
        assert_eq!(c["swarm"]["preempt"]["period"], draws[5].0);
        assert_eq!(c["swarm"]["preempt"]["seed"], draws[5].1);

        // --no-preempt turns it off for every seed.
        let off = campaign_args(&["--no-preempt"]);
        assert!((0..64).all(|s| preempt_for(&off, s) == (0, 0)));
        assert_eq!(guest_config(&off, 5)["swarm"]["preempt"]["period"], 0);
        assert!(!any_preempt(&off));
        assert!(any_preempt(&campaign_args(&["--seeds", "8"])));
    }

    #[test]
    fn swarm_record_covers_every_load_and_reference() {
        for load in ["transfers", "trie", "tip20", "chain"] {
            for reference in [false, true] {
                let mut extra = vec!["--load", load];
                if reference {
                    extra.push("--reference");
                }
                let args = campaign_args(&extra);
                for seed in 0..16 {
                    let s = swarm_for(&args, seed);
                    // Preemption is driver-side: the same draw for every load
                    // and with --reference.
                    let (period, pseed) = preempt_for(&campaign_args(&[]), seed);
                    assert_eq!((s.preempt.period, s.preempt.seed), (period, pseed));
                    assert_eq!(s.reference, reference);
                    let c = guest_config(&args, seed);
                    assert_eq!(c["swarm"], serde_json::to_value(s).unwrap());
                    assert_eq!(c["swarm"]["load"], load);
                }
            }
        }
        let c = guest_config(&campaign_args(&["--no-nemesis"]), 0);
        assert_eq!(c["swarm"]["nemesis"], false);
    }

    #[test]
    fn missing_preempt_ioctl_is_explained() {
        let msg = preempt_error(&io::Error::from_raw_os_error(ENOTTY)).to_string();
        assert!(msg.contains("lacks SET_PREEMPT_CONFIG"), "{msg}");
        assert!(msg.contains("--no-preempt"), "{msg}");
        let other = preempt_error(&io::Error::from_raw_os_error(22)).to_string();
        assert!(!other.contains("lacks"), "{other}");
    }

    #[test]
    fn campaign_json_without_no_preempt_still_replays() {
        // campaign.json from before --no-preempt existed must still parse.
        let mut v = serde_json::to_value(campaign_args(&[])).unwrap();
        v.as_object_mut().unwrap().remove("no_preempt");
        let args: CampaignArgs = serde_json::from_value(v).unwrap();
        assert!(!args.no_preempt);
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

    #[test]
    fn tip20_load_configures_image_oracle_and_coverage() {
        let args = campaign_args(&["--load", "tip20"]);
        let c = guest_config(&args, 3);
        assert_eq!(c["load"]["image"], "bedrock/tempo-dst-tip20:latest");
        assert_eq!(c["load"]["tps"], 50);
        assert_eq!(c["tip20"], true);
        assert_eq!(c["trie"], serde_json::Value::Null);
        assert_eq!(c["cob"], serde_json::Value::Null);
        assert!(required(&args).contains(&"S/tip20-pinned-read-survived-restart"));
        let quiet = campaign_args(&["--load", "tip20", "--no-nemesis"]);
        assert!(!required(&quiet).contains(&"S/tip20-pinned-read-survived-restart"));
        assert!(required(&quiet).contains(&"S/tip20-transfers-in-block"));
    }

    #[test]
    fn chain_load_configures_image_oracle_and_coverage() {
        let args = campaign_args(&["--load", "chain"]);
        let c = guest_config(&args, 3);
        assert_eq!(c["load"]["image"], "bedrock/tempo-dst-chain:latest");
        assert_eq!(c["load"]["tps"], 10);
        assert_eq!(c["cob"]["address"], CHAIN_CONTRACT);
        assert_eq!(c["trie"], serde_json::Value::Null);
        assert_eq!(c["tip20"], false);
        assert!(required(&args).contains(&"S/chain-survived-restart"));
        // Without a guaranteed kill there is no restart to survive.
        for extra in [&["--no-nemesis"][..], &["--run-secs", "60"]] {
            let args = campaign_args(&[&["--load", "chain"][..], extra].concat());
            assert_eq!(required(&args), ["S/load-included", "S/chain-appended"]);
        }
    }
}
