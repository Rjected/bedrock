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
//! <out>/seed-<n>/scenario.json   every harness decision, by value (scenario.rs)
//! <out>/seed-<n>/tape.bin        every input the branch consumed (bedrock_lab::Tape)
//! <out>/seed-<n>/manifest.json   what the tape is tied to (manifest.rs)
//! <out>/summary.json
//! ```
//!
//! `replay <out>/seed-<n>` reruns one seed from scratch (re-deriving it from
//! the seed) and checks the collected artifacts are byte-identical.
//! `replay --scenario` reruns the recorded decisions instead of the seed's;
//! `replay --tape` serves the recorded inputs (randomness and host actions at
//! their recorded virtual times) and refuses to run against other binaries.
//!
//! `branch <out>/seed-<n> --at T` re-executes a recorded seed's tape to the
//! moment `T`, checkpoints there, and runs `--seeds` branches from it that
//! vary the randomness and/or decisions after `T` (`branching.rs`), each a
//! full seed run with its own tape; `moments` lists moments worth branching
//! from.

mod branching;
mod manifest;
mod scenario;
mod verdict;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bedrock_lab::{
    BashOutput, BashTarget, Branch, Checkpoint, Cut, Event, EventSink, InputRecording, InputSource,
    IoInput, LabError, LabOpts, PrefixSource, RecordedInputSource, RngMode, RunOutcome,
    SeededSource, Tape, VirtDuration, VirtTime,
};
use bedrock_vm::{
    load_kernel, LinuxBootConfig, PreemptConfig, VmBuilder, VmError, DEFAULT_TSC_FREQUENCY,
};
use branching::{At, BranchInfo, BranchResult, HostAction, RngStream, Vary};
use clap::{Args as ClapArgs, Parser, Subcommand};
use manifest::{Environment, Manifest, TapeSummary};
use scenario::{Decisions, Generation, Kill, LoadScenario, NemesisScenario, Observed, Scenario};
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
/// Salt of the driver-side nemesis plan (`--decisions explicit`).
const NEMESIS_SALT: u64 = 0x6e65_6d65_7369_7331; // "nemesis1"
/// Per-seed files next to the collected artifacts.
const SCENARIO_FILE: &str = "scenario.json";
const TAPE_FILE: &str = "tape.bin";
const MANIFEST_FILE: &str = "manifest.json";

/// How harness decisions are derived from a seed. Everything that derives a
/// decision takes one, so a test can change the derivation and check that a
/// recorded scenario does not care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Derivation {
    preempt_salt: u64,
    nemesis_salt: u64,
}

impl Derivation {
    const CURRENT: Self = Self {
        preempt_salt: PREEMPT_SALT,
        nemesis_salt: NEMESIS_SALT,
    };

    /// Test hook: `BEDROCK_DST_DERIVATION_SALT=<u64>` perturbs every salt,
    /// simulating a change to how decisions are derived from a seed (seeds
    /// then mean other runs; `replay --scenario` must be unaffected).
    fn from_env() -> Self {
        match std::env::var("BEDROCK_DST_DERIVATION_SALT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(x) => Self {
                preempt_salt: PREEMPT_SALT ^ x,
                nemesis_salt: NEMESIS_SALT ^ x,
            },
            None => Self::CURRENT,
        }
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Per-seed forced preemption `(period, seed)`. `(0, 0)` when disabled by
/// `--no-preempt`. Part of [`swarm_for`].
#[cfg(test)]
fn preempt_for(args: &CampaignArgs, seed: u64) -> (u64, u64) {
    preempt_for_with(args, seed, &Derivation::CURRENT)
}

fn preempt_for_with(args: &CampaignArgs, seed: u64, d: &Derivation) -> (u64, u64) {
    if args.no_preempt {
        return (0, 0);
    }
    let r = splitmix64(seed ^ d.preempt_salt);
    let period = PREEMPT_PERIODS[(r % PREEMPT_PERIODS.len() as u64) as usize];
    if period == 0 {
        return (0, 0);
    }
    (period, splitmix64(r))
}

/// One seed's swarm record: every feature that shapes the run, so verdicts
/// can be grouped by feature. Recorded as `"swarm"` in the per-seed
/// `config.json` and `verdict.json` (both compared by `replay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Swarm {
    /// Drawn per seed: forced-preemption period (guest instructions; 0 =
    /// off) and its jitter seed, applied by the driver (`Branch::set_preempt`).
    preempt: Preempt,
    /// Campaign-wide, recorded so a verdict names its whole configuration.
    load: Load,
    reference: bool,
    nemesis: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Preempt {
    period: u64,
    seed: u64,
}

/// The swarm choices for `seed`: a pure function of the campaign arguments
/// and the seed, drawn with a host-side PRNG (never guest randomness), so
/// `replay` re-derives them. In-guest load generators (trie, TIP-20, chain
/// specs) key off the same seed via the delivered config.
fn swarm_for(args: &CampaignArgs, seed: u64) -> Swarm {
    swarm_for_with(args, seed, &Derivation::CURRENT)
}

fn swarm_for_with(args: &CampaignArgs, seed: u64, d: &Derivation) -> Swarm {
    let (period, preempt_seed) = preempt_for_with(args, seed, d);
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
    Replay(ReplayArgs),
    /// Re-execute a recorded seed to a moment, checkpoint there, and fan out
    /// branches that continue with other randomness and/or decisions.
    Branch(BranchArgs),
    /// List moments of a recorded seed worth branching from.
    Moments(MomentsArgs),
}

#[derive(ClapArgs, Debug)]
struct BranchArgs {
    /// A recorded seed directory (tape.bin, scenario.json, manifest.json):
    /// a campaign's `seed-<n>`, or a branch.
    run_dir: PathBuf,
    /// The moment: virtual seconds since the run forked from the warm
    /// checkpoint (`42.5`), absolute virtual time (`vt:112.3`), or just
    /// before a randomness input of the tape (`input:1234`). See `moments`.
    #[arg(long)]
    at: At,
    /// Branches to run.
    #[arg(long, default_value_t = 4)]
    seeds: u64,
    /// First branch seed.
    #[arg(long, default_value_t = 0)]
    seed_start: u64,
    /// Output directory (default `<run_dir>/branches-<at>`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// What branches change after the moment: `none` (the rest of the tape:
    /// the original run again), `rng` (fresh randomness per branch),
    /// `decisions` (re-draw the nemesis kills and load generations not yet
    /// applied), or `both`.
    #[arg(long, value_enum, default_value_t = Vary::Rng)]
    vary: Vary,
    /// Branch even if manifest.json does not match the current binaries.
    #[arg(long)]
    force: bool,
}

#[derive(ClapArgs, Debug)]
struct MomentsArgs {
    /// A recorded seed directory.
    run_dir: PathBuf,
}

#[derive(ClapArgs)]
struct ReplayArgs {
    /// A campaign's `seed-<n>` directory.
    run_dir: PathBuf,
    /// Serve the recorded input tape (randomness and host actions at their
    /// recorded virtual times) instead of the seed's RNG stream. Requires the
    /// binaries in manifest.json.
    #[arg(long, conflicts_with = "scenario")]
    tape: bool,
    /// Rerun the recorded decisions (scenario.json) instead of re-deriving
    /// them from the seed.
    #[arg(long)]
    scenario: bool,
    /// With --tape: replay even if manifest.json does not match the current
    /// binaries or the warm checkpoint time.
    #[arg(long, requires = "tape")]
    force: bool,
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
/// Guest `nemesis::WARMUP_SECS`: no kill before this.
const NEMESIS_WARMUP_SECS: u64 = 5;
/// Guest `nemesis::MAX_DOWN_SECS`: longest downtime.
const NEMESIS_MAX_DOWN_SECS: u64 = 3;
/// Nemesis warmup before the first kill plus its longest downtime.
const NEMESIS_SLACK_SECS: u64 = NEMESIS_WARMUP_SECS + NEMESIS_MAX_DOWN_SECS;

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
    /// Who makes the nemesis plan and per-generation load seeds: `guest`
    /// (drawn in the guest, read back into scenario.json) or `explicit`
    /// (derived by the driver from the seed and delivered in config.json, so
    /// `replay --scenario` is byte-identical). `explicit` gives seeds other
    /// kill plans than `guest`.
    #[arg(long, value_enum, default_value_t = Decisions::Guest)]
    #[serde(default)]
    decisions: Decisions,
    /// Draw each generated load's spec seed from getrandom in the guest
    /// (Bedrock-controlled, on the tape) instead of using the run seed.
    /// Changes every seed's load; `replay --scenario` then delivers the drawn
    /// seeds, so it replays the load but not byte-for-byte.
    #[arg(long)]
    #[serde(default)]
    spec_seeds_from_getrandom: bool,
    /// Serve the seed's RNG stream in-kernel (`Branch::reseed_rng`, as
    /// before trace replay) instead of from the driver (`SeededSource`: the
    /// same values, but each draw exits to userspace like a tape replay
    /// does). In-kernel serving is not exactly replayable from a tape: an
    /// interrupt pending at a randomness exit lands on the other side of the
    /// draw. Campaigns from before trace replay used it.
    #[arg(long)]
    #[serde(default = "yes")]
    kernel_rng: bool,
    /// Do not record seed-<n>/tape.bin (input capture costs one event per
    /// RDRAND/getrandom served).
    #[arg(long)]
    #[serde(default)]
    no_tape: bool,
    /// `KEY=VALUE` recorded in every manifest.json (e.g. `tempo=<rev>`).
    #[arg(long = "build-info")]
    #[serde(default)]
    build_info: Vec<String>,
    #[arg(long, default_value = "dst-out")]
    out: PathBuf,
}

fn yes() -> bool {
    true
}

impl CampaignArgs {
    /// These arguments with the run parameters `s` recorded, so a scenario
    /// replay judges coverage like its original.
    fn with_scenario(&self, s: &Scenario) -> CampaignArgs {
        let mut a = self.clone();
        a.run_secs = s.run_secs;
        a.liveness_secs = s.liveness_secs;
        a.load = s.load.kind;
        a.reference = s.swarm.reference;
        a.no_nemesis = !s.nemesis.enabled;
        a.max_kills = s.nemesis.max_kills;
        a.min_gap_secs = s.nemesis.min_gap_secs;
        if s.load.kind == Load::Transfers {
            a.txgen_count = s.load.count;
        }
        a
    }
}

/// Writes guest serial lines to the current run's console.log.
#[derive(Default)]
struct ConsoleSink {
    file: Mutex<Option<File>>,
}

impl ConsoleSink {
    fn open(&self, path: &Path) -> Result<()> {
        self.open_with(path, &[])
    }

    /// Starts `path` with `prefix` (a branch's console before its moment).
    fn open_with(&self, path: &Path, prefix: &[u8]) -> Result<()> {
        let mut f = File::create(path)?;
        f.write_all(prefix)?;
        *self.file.lock().unwrap() = Some(f);
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

/// Runs host commands in the guest: a live branch, or a tape replay.
trait Host {
    fn bash(&mut self, cmd: &str) -> Result<BashOutput>;
}

impl Host for Branch {
    fn bash(&mut self, cmd: &str) -> Result<BashOutput> {
        Ok(Branch::bash(self, BashTarget::host(), cmd, true)?)
    }
}

fn host_bash(host: &mut impl Host, cmd: &str) -> Result<(i32, String)> {
    let out = host.bash(cmd)?;
    Ok((out.exit_code, out.output_lossy().into_owned()))
}

/// Reads a guest file from `offset` in chunks that fit the I/O output buffer.
fn fetch(host: &mut impl Host, path: &str, offset: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    loop {
        let cmd = format!(
            "tail -c +{} {path} 2>/dev/null | head -c {CHUNK}",
            offset + data.len() + 1
        );
        let out = host.bash(&cmd)?;
        let n = out.output.len();
        data.extend_from_slice(&out.output);
        if n < CHUNK {
            return Ok(data);
        }
    }
}

/// Blocks the reference may trail the primary at the warm checkpoint.
const REFERENCE_SYNC_SLACK: u64 = 2;

/// Boots and warms the shared prefix. `probe_preempt`: some branch will use
/// forced preemption, so check the ioctl exists before the long boot.
fn boot(args: &CampaignArgs, sink: Arc<ConsoleSink>, probe_preempt: bool) -> Result<Checkpoint> {
    let mut vm = VmBuilder::new().memory_mb(args.memory_mb).build()?;
    if probe_preempt {
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
    if args.reference {
        // The reference backfills the primary's chain over p2p. Wait for it
        // before the checkpoint so every branch starts with it synced (seen:
        // a reference that never backfilled stalled at head 0 all run).
        loop {
            if warm.current_time() >= deadline {
                return Err("reference node did not sync before warm timeout".into());
            }
            let head = |b: &mut Branch, cmd| {
                host_bash(b, cmd)
                    .map(|(code, out)| out.trim().parse::<u64>().ok().filter(|_| code == 0))
            };
            let primary = head(&mut warm, "tempo-dst head")?;
            let reference = head(&mut warm, "tempo-dst reference-head")?;
            println!(
                "reference sync vt {:.1}s: primary {primary:?} reference {reference:?}",
                warm.current_time().as_secs_f64()
            );
            if let (Some(p), Some(r)) = (primary, reference) {
                if r + REFERENCE_SYNC_SLACK >= p {
                    break;
                }
            }
            warm.run_for(secs(5))?;
        }
    }
    let (_, mem) = host_bash(&mut warm, MEM_REPORT)?;
    fs::write(args.out.join("guest-mem.txt"), mem)?;
    let cp = warm.checkpoint()?;
    println!("warm checkpoint at vt {:.1}s", cp.time().as_secs_f64());
    Ok(cp)
}

/// The guest nemesis planner (`tempo-dst`'s `nemesis::plan`), run by the
/// driver for `--decisions explicit`: kills at least `min_gap_secs` apart
/// after a warmup, each leaving a full gap for recovery before `run_secs`,
/// drawn from a host PRNG of the seed instead of guest randomness.
fn nemesis_plan_for(args: &CampaignArgs, seed: u64, d: &Derivation) -> Vec<Kill> {
    let mut kills = Vec::new();
    if args.no_nemesis {
        return kills;
    }
    let mut state = splitmix64(seed ^ d.nemesis_salt);
    let mut draw = |lo: u64, hi: u64| {
        state = splitmix64(state);
        lo + state % (hi - lo + 1)
    };
    let gap = u64::from(args.min_gap_secs.max(1));
    let mut t = NEMESIS_WARMUP_SECS;
    for _ in 0..args.max_kills {
        let at = t + draw(0, gap);
        let down = draw(0, NEMESIS_MAX_DOWN_SECS);
        if at + down + gap > args.run_secs {
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

/// `(count, tps, image)` of the campaign's load.
fn load_params(args: &CampaignArgs) -> (u64, u64, Option<&'static str>) {
    // Generated loads get enough steps to outlast the run.
    let steps = |tps| (args.run_secs + 60) * tps;
    match args.load {
        Load::Transfers => (args.txgen_count, args.txgen_tps, None),
        Load::Trie => (steps(args.trie_tps), args.trie_tps, Some(TRIE_IMAGE)),
        Load::Tip20 => (steps(args.tip20_tps), args.tip20_tps, Some(TIP20_IMAGE)),
        Load::Chain => (steps(args.chain_tps), args.chain_tps, Some(CHAIN_IMAGE)),
    }
}

/// Every decision of `seed`'s run, derived from the campaign arguments and
/// the seed. With `--decisions guest` the nemesis plan and load seeds are
/// left to the guest (filled in from its events after the run).
fn scenario_for(args: &CampaignArgs, seed: u64, d: &Derivation) -> Scenario {
    let (count, tps, image) = load_params(args);
    let explicit = args.decisions == Decisions::Explicit;
    let plan = explicit.then(|| nemesis_plan_for(args, seed, d));
    // One generation at start plus one per restart, each as the guest would
    // derive it (so explicit decisions do not change the load).
    let generations = match &plan {
        Some(plan) if !args.spec_seeds_from_getrandom => (0..=plan.len() as u64)
            .map(|g| Generation {
                txgen_seed: args.workload_seed + g,
                spec_seed: seed,
            })
            .collect(),
        _ => Vec::new(),
    };
    Scenario {
        version: scenario::SCENARIO_VERSION,
        decisions: args.decisions,
        seed,
        rng_seed: seed,
        swarm: swarm_for_with(args, seed, d),
        run_secs: args.run_secs,
        liveness_secs: args.liveness_secs,
        nemesis: NemesisScenario {
            enabled: !args.no_nemesis,
            max_kills: args.max_kills,
            min_gap_secs: args.min_gap_secs,
            plan,
        },
        load: LoadScenario {
            kind: args.load,
            count,
            tps,
            image: image.map(Into::into),
            workload_seed: args.workload_seed,
            draw_spec_seeds: args.spec_seeds_from_getrandom,
            generations,
        },
        observed: Observed::default(),
        meaning: scenario::meaning(),
    }
}

/// The guest's `/bedrock/in/config.json` for scenario `s`. `deliver`: pass
/// the nemesis plan and per-generation load inputs explicitly (driver-made
/// decisions, scenario replays); otherwise the guest derives them.
fn guest_config(s: &Scenario, deliver: bool) -> serde_json::Value {
    let mut load = json!({
        "seed": s.load.workload_seed,
        "count": s.load.count,
        "tps": s.load.tps,
    });
    if let Some(image) = &s.load.image {
        load["image"] = json!(image);
    }
    if s.load.draw_spec_seeds {
        load["draw_spec_seeds"] = json!(true);
    }
    if deliver && !s.load.generations.is_empty() {
        load["generations"] = json!(s.load.generations);
    }
    let kind = s.load.kind;
    let mut config = json!({
        "seed": s.seed,
        "workload_seed": s.load.workload_seed,
        "run_secs": s.run_secs,
        "liveness_secs": s.liveness_secs,
        "nemesis": {
            "enabled": s.nemesis.enabled,
            "max_kills": s.nemesis.max_kills,
            "min_gap_secs": s.nemesis.min_gap_secs,
        },
        "load": load,
        "trie": (kind == Load::Trie).then(|| json!({
            "address": TRIE_CONTRACT,
        })),
        "tip20": kind == Load::Tip20,
        "cob": (kind == Load::Chain).then(|| json!({
            "address": CHAIN_CONTRACT,
        })),
        "reference": s.swarm.reference,
        // Driver-side choices (e.g. preemption, applied by
        // `Branch::set_preempt`), recorded so the run is self-describing and
        // `replay` compares them. The guest ignores this key.
        "swarm": s.swarm,
    });
    if deliver {
        if let Some(plan) = &s.nemesis.plan {
            config["nemesis_plan"] = json!(plan);
        }
    }
    config
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

/// A branch being driven: live (inputs from the seeded RNG and the driver's
/// own host commands) or a tape replay (inputs from a recorded tape, whose
/// host actions the lab queues at their recorded virtual times; the driver's
/// commands must match them one for one).
struct Driver {
    b: Branch,
    tape: Option<TapeCursor>,
}

struct TapeCursor {
    io: Vec<IoInput>,
    pos: usize,
    /// Responses must arrive before this (the tape's end plus slack).
    deadline: VirtTime,
    recording: InputRecording,
}

impl Driver {
    fn exhausted(&self, at: VirtTime) -> Box<dyn Error> {
        let why = self
            .b
            .input_exhaustion()
            .unwrap_or_else(|| "the input source returned no randomness".into());
        format!("tape replay failed at vt {:.6}s: {why}", at.as_secs_f64()).into()
    }

    /// Runs to `end`; `Some(kind)` if the guest stopped under us.
    fn run_until(&mut self, end: VirtTime) -> Result<Option<String>> {
        while self.b.current_time() < end {
            let (at, outcome) = self.b.run_until(end)?;
            match outcome {
                RunOutcome::ReachedTime => break,
                RunOutcome::Ready => {}
                RunOutcome::ActionResponse { .. } if self.tape.is_some() => {
                    return Err(format!(
                        "tape replay: unexpected host-action response at vt {:.6}s",
                        at.as_secs_f64()
                    )
                    .into())
                }
                RunOutcome::ActionResponse { .. } => {}
                RunOutcome::RngExhausted if self.tape.is_some() => return Err(self.exhausted(at)),
                RunOutcome::RngExhausted => return Err("seed source exhausted".into()),
                RunOutcome::Yielded { kind } => return Ok(Some(format!("{kind:?}"))),
            }
        }
        Ok(None)
    }

    /// After a tape replay: the inputs consumed must be exactly the tape's.
    fn check_tape_consumed(&self) -> Result<()> {
        let Some(t) = &self.tape else {
            return Ok(());
        };
        if t.pos != t.io.len() {
            return Err(format!(
                "tape replay: the driver issued {} of the tape's {} host actions",
                t.pos,
                t.io.len()
            )
            .into());
        }
        let (want, got) = (
            t.recording.random_inputs(),
            self.b.input_recording().random_inputs(),
        );
        if let Some(i) = (0..want.len().min(got.len())).find(|&i| want[i] != got[i]) {
            return Err(format!(
                "tape replay diverged at randomness input #{i}: tape {:?}, replay {:?}",
                want[i], got[i]
            )
            .into());
        }
        if want.len() != got.len() {
            return Err(format!(
                "tape replay consumed {} randomness inputs; the tape holds {}",
                got.len(),
                want.len()
            )
            .into());
        }
        Ok(())
    }
}

impl Host for Driver {
    fn bash(&mut self, cmd: &str) -> Result<BashOutput> {
        let Some(t) = self.tape.as_mut() else {
            return Host::bash(&mut self.b, cmd);
        };
        let Some(expected) = t.io.get(t.pos) else {
            return Err(format!(
                "tape replay: the driver issued {cmd:?} after the tape's last host action"
            )
            .into());
        };
        if expected.command != cmd || expected.target != BashTarget::Host || !expected.record_output
        {
            return Err(format!(
                "tape replay diverged at host action #{}: the driver issued {cmd:?}, the tape \
                 recorded {:?}",
                t.pos, expected.command
            )
            .into());
        }
        t.pos += 1;
        let deadline = t.deadline;
        // The lab queues the action itself once the branch reaches its
        // recorded time; run until its response.
        loop {
            let (at, outcome) = self.b.run_until(deadline)?;
            match outcome {
                RunOutcome::ActionResponse { output } => return Ok(output),
                RunOutcome::Ready => {}
                RunOutcome::ReachedTime => {
                    return Err(format!(
                        "tape replay: no response to {cmd:?} by vt {:.6}s",
                        at.as_secs_f64()
                    )
                    .into())
                }
                RunOutcome::RngExhausted => return Err(self.exhausted(at)),
                RunOutcome::Yielded { kind } => {
                    return Err(
                        format!("tape replay: guest stopped ({kind:?}) during {cmd:?}").into(),
                    )
                }
            }
        }
    }
}

/// How a seed's branch gets its inputs.
enum Inputs {
    /// Bedrock's in-VM PRNG reseeded with the scenario's `rng_seed`;
    /// `record`: capture a tape.
    Seeded { record: bool },
    /// A recorded tape.
    Tape(Tape),
}

/// One finished seed.
struct SeedRun {
    verdict: verdict::Verdict,
    scenario: Scenario,
    start: VirtTime,
    end: VirtTime,
    recording: Option<InputRecording>,
}

/// Runs scenario `s` on a branch of `warm` and collects its artifacts into
/// `dir`. `config`: the exact config to deliver (a tape replay passes the
/// recorded one); `None` builds it from `s` (delivering decisions when
/// `deliver`). `actions`: host commands to issue mid-run (a replayed
/// branch's).
#[allow(clippy::too_many_arguments)]
fn run_seed(
    args: &CampaignArgs,
    warm: &Checkpoint,
    sink: &ConsoleSink,
    s: &Scenario,
    deliver: bool,
    config: Option<String>,
    inputs: Inputs,
    actions: &[HostAction],
    dir: &Path,
) -> Result<SeedRun> {
    fs::create_dir_all(dir)?;
    sink.open(&dir.join("console.log"))?;
    let mut record_tape = false;
    let (b, tape) = match inputs {
        // Per-seed randomness: RDRAND and guest getrandom() come from the
        // hypervisor's xorshift stream seeded per branch, served in-kernel
        // or (the same values) from the driver.
        Inputs::Seeded { record } if args.kernel_rng => {
            let mut b = warm.branch()?;
            b.reseed_rng(s.rng_seed)?;
            if record {
                b.set_record_inputs(true)?;
                record_tape = true;
            }
            (b, None)
        }
        Inputs::Seeded { record } => {
            // A source branch records its inputs anyway.
            record_tape = record;
            let b = warm.branch_with_input_source(SeededSource::new(s.rng_seed))?;
            (b, None)
        }
        Inputs::Tape(tape) => {
            if tape.start != warm.time() {
                return Err(format!(
                    "tape starts at {} instructions but the warm checkpoint is at {}",
                    tape.start.instructions(),
                    warm.time().instructions()
                )
                .into());
            }
            let cursor = TapeCursor {
                io: tape.recording.io_inputs().to_vec(),
                pos: 0,
                deadline: tape.end + secs(60),
                recording: tape.recording.clone(),
            };
            let b = warm.branch_with_input_source(RecordedInputSource::new(tape.recording))?;
            (b, Some(cursor))
        }
    };
    let config = match config {
        Some(c) => c,
        None => serde_json::to_string(&guest_config(s, deliver))?,
    };
    fs::write(dir.join("config.json"), &config)?;
    let started = begin(s, b, tape, &config)?;
    finish(args, s, started, actions, record_tape, dir)
}

/// A seed's branch after `tempo-dst start`.
struct Started {
    d: Driver,
    /// Where the branch forked (the warm checkpoint).
    start: VirtTime,
    /// The assertion-log offset `tempo-dst start` printed.
    offset: usize,
}

/// Applies the scenario's forced preemption to `b` and delivers `config`
/// with `tempo-dst start`.
fn begin(s: &Scenario, b: Branch, tape: Option<TapeCursor>, config: &str) -> Result<Started> {
    let seed = s.seed;
    let mut d = Driver { b, tape };
    // Per-seed forced preemption, scoped to this branch (and inherited by
    // checkpoints of it); it is driver-side, so it applies to every load and
    // with --reference.
    apply_preempt(&mut d.b, s.swarm.preempt)?;
    println!(
        "seed {seed}: branch forked at vt {:.1}s (preempt period {})",
        d.b.current_time().as_secs_f64(),
        s.swarm.preempt.period
    );
    let start = d.b.current_time();
    // JSON from guest_config never contains a single quote.
    let (code, out) = host_bash(&mut d, &format!("tempo-dst start '{config}'"))?;
    println!(
        "seed {seed}: started at vt {:.1}s",
        d.b.current_time().as_secs_f64()
    );
    let offset: usize = out
        .trim()
        .parse()
        .map_err(|_| format!("tempo-dst start failed ({code}): {out}"))?;
    Ok(Started { d, start, offset })
}

/// Runs a started branch to `s.run_secs` after its fork (issuing `actions`
/// at their times), finalizes, and collects artifacts, verdict and scenario
/// into `dir`.
fn finish(
    args: &CampaignArgs,
    s: &Scenario,
    started: Started,
    actions: &[HostAction],
    record_tape: bool,
    dir: &Path,
) -> Result<SeedRun> {
    let Started {
        mut d,
        start,
        offset,
    } = started;
    let end = start + secs(s.run_secs);
    let mut guest_exit = None;
    for a in actions {
        let at = VirtTime::from_instructions(a.at_instructions, DEFAULT_TSC_FREQUENCY);
        if at >= end {
            break;
        }
        guest_exit = d.run_until(at)?;
        if guest_exit.is_some() {
            break;
        }
        let (code, out) = host_bash(&mut d, &a.command)?;
        println!(
            "seed {}: host action at vt {:.3}s exited {code}: {}",
            s.seed,
            at.as_secs_f64(),
            out.trim()
        );
        if code != 0 {
            return Err(format!("host action {:?} failed ({code}): {out}", a.command).into());
        }
    }
    if guest_exit.is_none() {
        guest_exit = d.run_until(end)?;
    }

    let mut assertions = Vec::new();
    if guest_exit.is_none() {
        host_bash(&mut d, "tempo-dst finalize")?;
        assertions = fetch(&mut d, "/bedrock/assertions.jsonl", offset)?;
        for f in COLLECT {
            let data = fetch(&mut d, &format!("/bedrock/{f}"), 0)?;
            fs::write(dir.join(Path::new(f).file_name().unwrap()), data)?;
        }
        let (_, mem) = host_bash(&mut d, MEM_REPORT)?;
        fs::write(dir.join("guest-mem.txt"), mem)?;
    }
    d.check_tape_consumed()?;
    if let Some(kind) = &guest_exit {
        // The guest stopped under us (e.g. kernel panic or shutdown): a failure
        // in its own right; the guest can no longer report anything.
        let rec = json!({"Always": {"condition": {"Bool": false}, "result": false,
            "message": format!("D/guest-exited: {kind}"),
            "location": {"file": "bedrock-dst", "line": 0, "column": 0}}});
        assertions.extend_from_slice(format!("{rec}\n").as_bytes());
    }
    fs::write(dir.join("assertions.jsonl"), &assertions)?;
    let required = required(&args.with_scenario(s));
    let mut v = verdict::aggregate(&String::from_utf8_lossy(&assertions), &required);
    v.swarm = serde_json::to_value(s.swarm)?;
    fs::write(dir.join("verdict.json"), serde_json::to_vec_pretty(&v)?)?;

    let mut scenario = s.clone();
    let events = fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
    scenario.record_observed(Observed::from_events(&events));
    fs::write(
        dir.join(SCENARIO_FILE),
        serde_json::to_vec_pretty(&scenario)?,
    )?;
    let recording = record_tape.then(|| d.b.input_recording().clone());
    Ok(SeedRun {
        verdict: v,
        scenario,
        start,
        end: d.b.current_time(),
        recording,
    })
}

/// Writes a live seed's (or branch's) tape and manifest.
fn write_trace(
    args: &CampaignArgs,
    env: &Environment,
    run: &SeedRun,
    dir: &Path,
    branch: Option<BranchInfo>,
) -> Result<()> {
    let tape = run.recording.as_ref().map(|rec| {
        let bytes = Tape {
            tsc_frequency: DEFAULT_TSC_FREQUENCY,
            start: run.start,
            end: run.end,
            recording: rec.clone(),
        }
        .to_bytes();
        (bytes, rec)
    });
    let summary = match &tape {
        Some((bytes, rec)) => {
            fs::write(dir.join(TAPE_FILE), bytes)?;
            Some(TapeSummary {
                file: TAPE_FILE.into(),
                sha256: manifest::sha256_bytes(bytes),
                random_inputs: rec.random_inputs().len() as u64,
                random_bytes: rec
                    .random_inputs()
                    .iter()
                    .map(|r| r.bytes.len() as u64)
                    .sum(),
                io_inputs: rec.io_inputs().len() as u64,
                consumers: manifest::consumers(rec),
            })
        }
        None => None,
    };
    let m = Manifest {
        version: manifest::MANIFEST_VERSION,
        seed: run.scenario.seed,
        bedrock_dst: manifest::bedrock_dst_identity(),
        environment: env.clone(),
        campaign: serde_json::to_value(args)?,
        warm_checkpoint_instructions: run.start.instructions(),
        branch_end_instructions: run.end.instructions(),
        rng_seed: run.scenario.rng_seed,
        preempt: serde_json::to_value(run.scenario.swarm.preempt)?,
        tape: summary,
        branch,
    };
    fs::write(dir.join(MANIFEST_FILE), serde_json::to_vec_pretty(&m)?)?;
    Ok(())
}

fn probe_environment(args: &CampaignArgs) -> Result<Environment> {
    Ok(Environment::probe(
        &args.vmlinux,
        &args.initrd,
        &args.images,
        &args.compose,
        &args.build_info,
        DEFAULT_TSC_FREQUENCY,
        args.boot_seed,
    )?)
}

fn campaign(args: CampaignArgs) -> Result<()> {
    fs::create_dir_all(&args.out)?;
    fs::write(
        args.out.join("campaign.json"),
        serde_json::to_vec_pretty(&args)?,
    )?;
    let env = probe_environment(&args)?;
    let derivation = Derivation::from_env();
    let sink = Arc::new(ConsoleSink::default());
    sink.open(&args.out.join("boot-console.log"))?;
    let wall = Instant::now();
    let warm = boot(&args, sink.clone(), any_preempt(&args))?;
    let mut summary = BTreeMap::new();
    for seed in args.seed_start..args.seed_start + args.seeds {
        let t = Instant::now();
        let dir = args.out.join(format!("seed-{seed}"));
        let s = scenario_for(&args, seed, &derivation);
        let run = run_seed(
            &args,
            &warm,
            &sink,
            &s,
            args.decisions == Decisions::Explicit,
            None,
            Inputs::Seeded {
                record: !args.no_tape,
            },
            &[],
            &dir,
        )?;
        write_trace(&args, &env, &run, &dir, None)?;
        let v = run.verdict;
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

/// `<dir>`'s seed, from its `seed-<n>` name.
fn seed_of(run_dir: &Path) -> Result<u64> {
    Ok(run_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("seed-"))
        .and_then(|n| n.parse().ok())
        .ok_or("run dir must be named seed-<n>")?)
}

/// `REPLAY_COMPARE` artifacts that differ between two seed directories.
fn diverged_artifacts(a: &Path, b: &Path) -> Vec<&'static str> {
    REPLAY_COMPARE
        .iter()
        .copied()
        .filter(|f| {
            fs::read(a.join(f)).unwrap_or_default() != fs::read(b.join(f)).unwrap_or_default()
        })
        .collect()
}

/// A recorded seed's tape with the campaign arguments and environment it
/// was recorded against (checked against the current binaries).
struct Recorded {
    args: CampaignArgs,
    manifest: Manifest,
    tape: Tape,
    env: Environment,
}

/// Loads `run_dir`'s manifest and tape and checks that the current binaries
/// are the ones it was recorded with (a mismatch is an error unless
/// `force`).
fn load_recorded(run_dir: &Path, force: bool) -> Result<Recorded> {
    let m: Manifest = serde_json::from_slice(&fs::read(run_dir.join(MANIFEST_FILE))?)
        .map_err(|e| format!("{}: {e}", run_dir.join(MANIFEST_FILE).display()))?;
    // The binaries are the ones the tape was recorded with, wherever the
    // manifest says they were.
    let args: CampaignArgs = serde_json::from_value(m.campaign.clone())?;
    let now = probe_environment(&args)?;
    let diff = m.environment.diff(&now);
    if !diff.is_empty() {
        let msg = format!(
            "the tape was recorded against other binaries:\n  {}",
            diff.join("\n  ")
        );
        if !force {
            return Err(format!("{msg}\n(--force runs anyway)").into());
        }
        eprintln!("warning: {msg}");
    }
    let summary = m
        .tape
        .as_ref()
        .ok_or("the run recorded no tape (--no-tape)")?;
    let bytes = fs::read(run_dir.join(&summary.file))?;
    if manifest::sha256_bytes(&bytes) != summary.sha256 {
        return Err(format!("{} does not match its manifest sha256", summary.file).into());
    }
    let tape = Tape::from_bytes(&bytes)?;
    if tape.tsc_frequency != DEFAULT_TSC_FREQUENCY {
        return Err(format!(
            "tape TSC frequency {} != {DEFAULT_TSC_FREQUENCY}",
            tape.tsc_frequency
        )
        .into());
    }
    Ok(Recorded {
        args,
        manifest: m,
        tape,
        env: now,
    })
}

/// Boots the recorded campaign's warm checkpoint and checks it lands where
/// the manifest says.
fn boot_recorded(
    rec: &Recorded,
    args: &CampaignArgs,
    sink: Arc<ConsoleSink>,
    preempt: bool,
    force: bool,
) -> Result<Checkpoint> {
    let warm = boot(args, sink, preempt)?;
    let w = rec.manifest.warm_checkpoint_instructions;
    if warm.time().instructions() != w {
        let msg = format!(
            "warm checkpoint at {} instructions, recorded at {w}: the boot prefix differs",
            warm.time().instructions()
        );
        if !force {
            return Err(format!("{msg} (--force runs anyway)").into());
        }
        eprintln!("warning: {msg}");
    }
    Ok(warm)
}

fn read_scenario(run_dir: &Path) -> Result<Scenario> {
    Ok(
        serde_json::from_slice(&fs::read(run_dir.join(SCENARIO_FILE))?)
            .map_err(|e| format!("{}: {e}", run_dir.join(SCENARIO_FILE).display()))?,
    )
}

fn replay(r: &ReplayArgs) -> Result<()> {
    let run_dir = r.run_dir.as_path();
    let out = run_dir.parent().ok_or("run dir has no parent")?;
    let name = run_dir
        .file_name()
        .ok_or("run dir has no name")?
        .to_string_lossy()
        .into_owned();
    let branch = fs::read(run_dir.join(MANIFEST_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
        .and_then(|m| m.branch);
    if branch.is_some() && !r.tape {
        return Err("a branch replays with --tape (its seed alone does not describe it)".into());
    }
    if !r.tape && !r.scenario {
        // Re-derive the seed from the campaign arguments.
        let mut args: CampaignArgs = serde_json::from_slice(&fs::read(out.join("campaign.json"))?)?;
        let seed = seed_of(run_dir)?;
        let replay_out = run_dir.join("replay");
        args.out = replay_out.clone();
        args.seed_start = seed;
        args.seeds = 1;
        campaign(args)?;
        let diverged = diverged_artifacts(run_dir, &replay_out.join(format!("seed-{seed}")));
        return if diverged.is_empty() {
            println!("replay of seed {seed}: identical");
            Ok(())
        } else {
            Err(format!("replay of seed {seed} diverged in {diverged:?}").into())
        };
    }

    let recorded = read_scenario(run_dir)?;
    let seed = recorded.seed;
    let mode = if r.tape { "tape" } else { "scenario" };
    let replay_out = run_dir.join(format!("replay-{mode}"));
    fs::create_dir_all(&replay_out)?;
    let sink = Arc::new(ConsoleSink::default());
    let preempt = recorded.swarm.preempt.period != 0;
    let dir = replay_out.join(&name);
    let run = if r.tape {
        let rec = load_recorded(run_dir, r.force)?;
        let mut args = rec.args.clone();
        args.out = replay_out.clone();
        fs::write(
            replay_out.join("campaign.json"),
            serde_json::to_vec_pretty(&args)?,
        )?;
        sink.open(&replay_out.join("boot-console.log"))?;
        let warm = boot_recorded(&rec, &args, sink.clone(), preempt, r.force)?;
        // The exact config the original delivered (it is also in the
        // tape's first host action, which the driver must match).
        let config = fs::read_to_string(run_dir.join("config.json"))?;
        let actions = branch.map(|b| b.actions).unwrap_or_default();
        run_seed(
            &args,
            &warm,
            &sink,
            &recorded,
            true,
            Some(config),
            Inputs::Tape(rec.tape),
            &actions,
            &dir,
        )?
    } else {
        let mut args: CampaignArgs = serde_json::from_slice(&fs::read(out.join("campaign.json"))?)?;
        args.out = replay_out.clone();
        fs::write(
            replay_out.join("campaign.json"),
            serde_json::to_vec_pretty(&args)?,
        )?;
        sink.open(&replay_out.join("boot-console.log"))?;
        let warm = boot(&args, sink.clone(), preempt)?;
        let run = run_seed(
            &args,
            &warm,
            &sink,
            &recorded,
            true,
            None,
            Inputs::Seeded { record: true },
            &[],
            &dir,
        )?;
        let env = probe_environment(&args)?;
        write_trace(&args, &env, &run, &dir, None)?;
        run
    };

    let decisions = recorded.decision_diff(&run.scenario);
    let diverged = diverged_artifacts(run_dir, &dir);
    let exact = r.tape || recorded.replays_exactly();
    if !decisions.is_empty() {
        return Err(format!(
            "{mode} replay of seed {seed} made other decisions:\n  {}",
            decisions.join("\n  ")
        )
        .into());
    }
    match (diverged.is_empty(), exact) {
        (true, _) => {
            println!("{mode} replay of {name}: identical");
            Ok(())
        }
        (false, true) => Err(format!("{mode} replay of {name} diverged in {diverged:?}").into()),
        (false, false) => {
            println!(
                "{mode} replay of seed {seed}: same decisions; artifacts differ in {diverged:?} \
                 (decisions were made in the guest, so the replay delivered a different config; \
                 use --decisions explicit campaigns or replay --tape for exact replays)"
            );
            Ok(())
        }
    }
}

/// How a run's randomness was served: a branch's manifest says; a campaign
/// seed is one stretch of its `rng_seed` stream.
fn rng_streams(m: &Manifest) -> Vec<RngStream> {
    match &m.branch {
        Some(b) => b.rng_streams.clone(),
        None => vec![RngStream {
            from_input: 0,
            seed: m.rng_seed,
            skip: 0,
        }],
    }
}

/// sha256 over a run's assertions and events: equal for the same run.
fn fingerprint(dir: &Path) -> String {
    let mut data = fs::read(dir.join("assertions.jsonl")).unwrap_or_default();
    data.extend(fs::read(dir.join("events.jsonl")).unwrap_or_default());
    manifest::sha256_bytes(&data)
}

/// `bedrock-dst branch`: re-execute a recorded seed to a moment, checkpoint
/// there, and run `--seeds` branches from the checkpoint.
fn branch_cmd(a: &BranchArgs) -> Result<()> {
    let run_dir = a.run_dir.as_path();
    let rec = load_recorded(run_dir, a.force)?;
    let parent = read_scenario(run_dir)?;
    let config = fs::read_to_string(run_dir.join("config.json"))?;
    let out = a
        .out
        .clone()
        .unwrap_or_else(|| run_dir.join(format!("branches-{}", a.at.slug())));
    fs::create_dir_all(&out)?;
    let mut args = rec.args.clone();
    args.out = out.clone();
    fs::write(out.join("campaign.json"), serde_json::to_vec_pretty(&args)?)?;
    let tape = &rec.tape;
    let tape_rec = &tape.recording;
    let parent_sha = rec
        .manifest
        .tape
        .as_ref()
        .map(|t| t.sha256.clone())
        .unwrap_or_default();

    let sink = Arc::new(ConsoleSink::default());
    sink.open(&out.join("boot-console.log"))?;
    let warm = boot_recorded(
        &rec,
        &args,
        sink.clone(),
        parent.swarm.preempt.period != 0,
        a.force,
    )?;
    let fork = warm.time();
    let target = a.at.resolve(tape_rec, fork)?;
    let end = fork + secs(parent.run_secs);
    let start_io = tape_rec
        .io_inputs()
        .first()
        .ok_or("the tape has no start action")?
        .at;
    if target <= start_io || target >= end {
        return Err(format!(
            "moment vt {:.3}s is outside the run: pick one after its start action (vt {:.3}s) \
             and before its end (vt {:.3}s); see `bedrock-dst moments`",
            target.as_secs_f64(),
            start_io.as_secs_f64(),
            end.as_secs_f64()
        )
        .into());
    }

    // The prefix: the tape up to the moment, served strictly, then (never
    // reached before the checkpoint) the rest of the tape.
    sink.open(&out.join("prefix-console.log"))?;
    let cut0 = tape_rec.cut_at_time(target);
    let source = PrefixSource::new(
        tape_rec,
        cut0,
        RecordedInputSource::new(tape_rec.suffix(cut0)),
    );
    let b = warm.branch_with_input_source(source)?;
    let prefix_io = tape_rec.prefix(cut0);
    let cursor = TapeCursor {
        io: prefix_io.io_inputs().to_vec(),
        pos: 0,
        deadline: target + secs(60),
        recording: prefix_io,
    };
    let mut started = begin(&parent, b, Some(cursor), &config)?;
    if started.d.b.current_time() > target {
        return Err(format!(
            "moment vt {:.3}s falls inside the start action (it responded at vt {:.3}s)",
            target.as_secs_f64(),
            started.d.b.current_time().as_secs_f64()
        )
        .into());
    }
    if let Some(kind) = started.d.run_until(target)? {
        return Err(format!("the guest stopped ({kind}) before the moment").into());
    }
    let at = started.d.b.current_time();
    let got = started.d.b.input_recording().clone();
    if !tape_rec.starts_with(&got) {
        return Err("re-executing the tape prefix consumed other inputs than the tape".into());
    }
    let cut = Cut {
        random: got.random_inputs().len(),
        io: got.io_inputs().len(),
    };
    let at_run = (at - fork).as_secs_f64();
    println!(
        "moment {}: vt {:.6}s (run {at_run:.3}s), after {} of the tape's {} randomness inputs \
         and {} of its {} host actions",
        a.at,
        at.as_secs_f64(),
        cut.random,
        tape_rec.random_inputs().len(),
        cut.io,
        tape_rec.io_inputs().len()
    );
    let Started {
        d, start, offset, ..
    } = started;
    let checkpoint = d.b.checkpoint()?;
    let prefix_console = fs::read(out.join("prefix-console.log")).unwrap_or_default();
    let streams = rng_streams(&rec.manifest);
    // Kill times count from the nemesis start, right after the start action.
    let t_nemesis = (at - start_io).as_secs_f64();

    let mut results = Vec::new();
    for i in a.seed_start..a.seed_start + a.seeds {
        let name = format!("branch-{i}");
        let dir = out.join(&name);
        fs::create_dir_all(&dir)?;
        sink.open_with(&dir.join("console.log"), &prefix_console)?;
        let seed = branching::branch_seed(&parent_sha, at, i);
        let mut s = parent.clone();
        let mut actions = Vec::new();
        let kept: Vec<RngStream> = streams
            .iter()
            .filter(|st| (st.from_input as usize) < cut.random)
            .copied()
            .collect();
        let (fallback, branch_streams, rng_seed): (Box<dyn InputSource>, _, _) = match a.vary {
            Vary::None => (
                Box::new(RecordedInputSource::new(tape_rec.suffix(cut))),
                streams.clone(),
                None,
            ),
            Vary::Rng | Vary::Both => {
                let mut st = kept;
                st.push(RngStream {
                    from_input: cut.random as u64,
                    seed,
                    skip: 0,
                });
                (Box::new(SeededSource::new(seed)), st, Some(seed))
            }
            Vary::Decisions => {
                let c = branching::continued_stream(&streams, tape_rec, cut.random)
                    .ok_or("the parent's randomness streams do not cover the moment")?;
                let mut st = kept;
                st.push(c);
                (Box::new(SeededSource::new(c.seed).skip(c.skip)), st, None)
            }
        };
        if let Some(seed) = rng_seed {
            s.rng_seed = seed;
        }
        if a.vary.decisions() {
            let r = branching::redraw_decisions(&parent, t_nemesis, seed);
            println!(
                "{name}: decisions after the moment re-drawn: kills {:?} (kept {}), generations {:?} \
                 (kept {})",
                r.nemesis_plan, r.kept_kills, r.generations, r.kept_generations
            );
            if Some(&r.nemesis_plan) == parent.nemesis.plan.as_ref()
                && r.generations == parent.load.generations
            {
                println!(
                    "{name}: nothing is left to re-draw after this moment (the next kill would \
                     not fit the run); only the randomness varies"
                );
            }
            actions.push(HostAction {
                at_instructions: at.instructions(),
                command: format!("tempo-dst redecide '{}'", serde_json::to_string(&r)?),
            });
            // What the guest applied, read back from its events.
            s.nemesis.plan = None;
            s.load.generations.clear();
        }
        let source = PrefixSource::new(tape_rec, cut, fallback).after_prefix();
        let b = checkpoint.branch_with_input_source(source)?;
        let cursor = (a.vary == Vary::None).then(|| TapeCursor {
            io: tape_rec.suffix(cut).io_inputs().to_vec(),
            pos: 0,
            deadline: tape.end + secs(60),
            recording: tape_rec.clone(),
        });
        fs::write(dir.join("config.json"), &config)?;
        println!(
            "{name}: branch seed {i} ({seed:#x}) from vt {:.3}s",
            at.as_secs_f64()
        );
        let run = finish(
            &args,
            &s,
            Started {
                d: Driver { b, tape: cursor },
                start,
                offset,
            },
            &actions,
            true,
            &dir,
        )?;
        let info = BranchInfo {
            parent: run_dir.to_string_lossy().into_owned(),
            parent_seed: parent.seed,
            parent_tape_sha256: parent_sha.clone(),
            at: a.at.to_string(),
            at_instructions: at.instructions(),
            at_run_secs: at_run,
            cut,
            vary: a.vary,
            branch: i,
            rng_seed,
            rng_streams: branch_streams,
            actions,
        };
        write_trace(&args, &rec.env, &run, &dir, Some(info))?;
        let same_tape =
            fs::read(dir.join(TAPE_FILE)).ok() == fs::read(run_dir.join(TAPE_FILE)).ok();
        let diverged = diverged_artifacts(run_dir, &dir);
        let v = &run.verdict;
        let result = BranchResult {
            name: name.clone(),
            pass: v.pass,
            failures: v.failures.keys().cloned().collect(),
            fingerprint: fingerprint(&dir),
            identical_to_parent: same_tape && diverged.is_empty(),
        };
        println!(
            "{name}: {}{} {:?}",
            if v.pass { "PASS" } else { "FAIL" },
            if result.identical_to_parent {
                " (identical to the parent)"
            } else {
                ""
            },
            result.failures
        );
        results.push(result);
    }
    let text = branching::summarize(&results);
    print!("{text}");
    fs::write(
        out.join("summary.json"),
        serde_json::to_vec_pretty(&json!({
            "parent": run_dir.to_string_lossy(),
            "at": a.at.to_string(),
            "at_instructions": at.instructions(),
            "at_run_secs": at_run,
            "cut": cut,
            "vary": a.vary,
            "branches": results,
        }))?,
    )?;
    if a.vary == Vary::None {
        if let Some(r) = results.iter().find(|r| !r.identical_to_parent) {
            return Err(format!(
                "{} varied nothing but differs from the parent {}",
                r.name,
                run_dir.display()
            )
            .into());
        }
        println!("--vary none: every branch is identical to the parent");
    }
    Ok(())
}

/// `bedrock-dst moments`: print the moments of a recorded seed worth
/// branching from, with the `--at` values that pick them.
fn moments_cmd(a: &MomentsArgs) -> Result<()> {
    let run_dir = a.run_dir.as_path();
    let m: Manifest = serde_json::from_slice(&fs::read(run_dir.join(MANIFEST_FILE))?)?;
    let s = read_scenario(run_dir)?;
    let summary = m.tape.as_ref().ok_or("the run recorded no tape")?;
    let tape = Tape::from_bytes(&fs::read(run_dir.join(&summary.file))?)?;
    let events = fs::read_to_string(run_dir.join("events.jsonl")).unwrap_or_default();
    let start = VirtTime::from_instructions(m.warm_checkpoint_instructions, tape.tsc_frequency);
    let list = branching::moments(&events, &tape.recording, start, s.run_secs);
    println!(
        "{}: forked at vt {:.3}s, {} s run, {} randomness inputs on the tape",
        run_dir.display(),
        start.as_secs_f64(),
        s.run_secs,
        tape.recording.random_inputs().len()
    );
    println!("{:>10} {:>12} {:>14}  moment", "--at", "vt", "input");
    for m in &list {
        println!(
            "{:>10.3} {:>12.3} {:>14}  {}",
            m.run_secs,
            m.vt_secs,
            format!("input:{}", m.input),
            m.what
        );
    }
    let verdict = fs::read_to_string(run_dir.join("verdict.json")).unwrap_or_default();
    if verdict.contains("\"pass\": false") && !events.contains("\"first-failure\"") {
        println!("(no first-failure events: the guest predates them, so failures are not timed)");
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Campaign(args) => campaign(*args),
        Cmd::Replay(r) => replay(&r),
        Cmd::Branch(b) => branch_cmd(&b),
        Cmd::Moments(m) => moments_cmd(&m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config a campaign delivers for `seed`.
    fn cfg(args: &CampaignArgs, seed: u64) -> serde_json::Value {
        guest_config(
            &scenario_for(args, seed, &Derivation::CURRENT),
            args.decisions == Decisions::Explicit,
        )
    }

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
        let c = cfg(&args, 7);
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
        assert_eq!(cfg(&args, 1)["reference"], true);
        assert_eq!(
            required(&args),
            [
                "S/load-included",
                "S/reference-compared",
                "S/reference-synced"
            ]
        );
    }

    pub(crate) fn campaign_args(extra: &[&str]) -> CampaignArgs {
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
        let c = cfg(&args, 5);
        assert_eq!(c["swarm"]["preempt"]["period"], draws[5].0);
        assert_eq!(c["swarm"]["preempt"]["seed"], draws[5].1);

        // --no-preempt turns it off for every seed.
        let off = campaign_args(&["--no-preempt"]);
        assert!((0..64).all(|s| preempt_for(&off, s) == (0, 0)));
        assert_eq!(cfg(&off, 5)["swarm"]["preempt"]["period"], 0);
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
                    let c = cfg(&args, seed);
                    assert_eq!(c["swarm"], serde_json::to_value(s).unwrap());
                    assert_eq!(c["swarm"]["load"], load);
                }
            }
        }
        let c = cfg(&campaign_args(&["--no-nemesis"]), 0);
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
        let c = cfg(&args, 3);
        assert_eq!(c["load"]["image"], "bedrock/tempo-dst-trie:latest");
        assert_eq!(c["load"]["tps"], 5);
        // Slots and the write stream are generated in the guest from the seed.
        assert!(c["trie"].get("slots").is_none());
        assert!(!c.to_string().contains('\''));
    }

    #[test]
    fn tip20_load_configures_image_oracle_and_coverage() {
        let args = campaign_args(&["--load", "tip20"]);
        let c = cfg(&args, 3);
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
        let c = cfg(&args, 3);
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

    #[test]
    fn guest_decisions_keep_the_config_derivable_in_the_guest() {
        // The default leaves the plan and load seeds to the guest, as before
        // --decisions existed.
        let args = campaign_args(&["--load", "tip20"]);
        assert_eq!(args.decisions, Decisions::Guest);
        let c = cfg(&args, 4);
        assert!(c.get("nemesis_plan").is_none());
        assert!(c["load"].get("generations").is_none());
        assert!(c["load"].get("draw_spec_seeds").is_none());
        let s = scenario_for(&args, 4, &Derivation::CURRENT);
        assert_eq!(s.nemesis.plan, None);
        assert!(s.load.generations.is_empty());
        assert!(!s.replays_exactly());
        // A campaign.json from before trace replay still loads, as guest.
        let mut v = serde_json::to_value(campaign_args(&[])).unwrap();
        for k in [
            "decisions",
            "spec_seeds_from_getrandom",
            "no_tape",
            "build_info",
            "kernel_rng",
        ] {
            v.as_object_mut().unwrap().remove(k);
        }
        let old: CampaignArgs = serde_json::from_value(v).unwrap();
        assert_eq!(old.decisions, Decisions::Guest);
        assert!(!old.no_tape);
        // ...with the RNG served in-kernel, as it was; new campaigns serve it
        // from the driver so their tapes replay exactly.
        assert!(old.kernel_rng);
        assert!(!campaign_args(&[]).kernel_rng);
    }

    #[test]
    fn explicit_decisions_are_delivered_and_follow_the_guest_planner() {
        let args = campaign_args(&["--decisions", "explicit", "--load", "trie"]);
        for seed in 0..32 {
            let s = scenario_for(&args, seed, &Derivation::CURRENT);
            let plan = s.nemesis.plan.clone().unwrap();
            // tempo-dst's nemesis::plan constraints.
            assert!(!plan.is_empty() && plan.len() <= 3, "{plan:?}");
            let gap = u64::from(args.min_gap_secs);
            assert!(plan[0].at_secs >= NEMESIS_WARMUP_SECS);
            for w in plan.windows(2) {
                assert!(w[1].at_secs >= w[0].at_secs + w[0].down_secs + gap);
            }
            for k in &plan {
                assert!(k.down_secs <= NEMESIS_MAX_DOWN_SECS);
                assert!(k.at_secs + k.down_secs + gap <= args.run_secs);
            }
            // One load generation per restart plus the first, each as the
            // guest derives it.
            assert_eq!(s.load.generations.len(), plan.len() + 1);
            assert_eq!(s.load.generations[1].txgen_seed, args.workload_seed + 1);
            assert!(s.load.generations.iter().all(|g| g.spec_seed == seed));
            let c = cfg(&args, seed);
            assert_eq!(c["nemesis_plan"], serde_json::to_value(&plan).unwrap());
            assert_eq!(
                c["load"]["generations"],
                serde_json::to_value(&s.load.generations).unwrap()
            );
            assert!(!c.to_string().contains('\''));
            assert!(s.replays_exactly());
        }
        // Pinned, so a change to the derivation is noticed.
        let p = nemesis_plan_for(&args, 0, &Derivation::CURRENT);
        assert_eq!(
            p.iter()
                .map(|k| (k.at_secs, k.down_secs))
                .collect::<Vec<_>>(),
            [(28, 3), (86, 0), (146, 3)]
        );
        assert!(nemesis_plan_for(
            &campaign_args(&["--decisions", "explicit", "--no-nemesis"]),
            0,
            &Derivation::CURRENT
        )
        .is_empty());
        // Drawing spec seeds in the guest leaves them out of the config.
        let drawn = campaign_args(&["--decisions", "explicit", "--spec-seeds-from-getrandom"]);
        let s = scenario_for(&drawn, 1, &Derivation::CURRENT);
        assert!(s.load.generations.is_empty());
        assert!(!s.replays_exactly());
        assert_eq!(cfg(&drawn, 1)["load"]["draw_spec_seeds"], true);
    }

    #[test]
    fn scenario_replay_ignores_a_changed_derivation() {
        let args = campaign_args(&["--decisions", "explicit", "--load", "chain"]);
        let seed = 6;
        // The original run: decisions derived, delivered, written out.
        let original = scenario_for(&args, seed, &Derivation::CURRENT);
        let delivered = serde_json::to_string(&guest_config(&original, true)).unwrap();
        let file = serde_json::to_vec_pretty(&original).unwrap();

        // Later, the seed-to-decision derivation changes: re-deriving the
        // seed gives another run...
        let changed = Derivation {
            preempt_salt: PREEMPT_SALT ^ 0xdead,
            nemesis_salt: NEMESIS_SALT ^ 0xbeef,
        };
        let rederived = scenario_for(&args, seed, &changed);
        assert_ne!(rederived.nemesis.plan, original.nemesis.plan);
        assert_ne!(
            serde_json::to_string(&guest_config(&rederived, true)).unwrap(),
            delivered
        );
        // ...but replay --scenario reads the recorded decisions and delivers
        // exactly the original config, without deriving anything.
        let recorded: Scenario = serde_json::from_slice(&file).unwrap();
        assert_eq!(
            serde_json::to_string(&guest_config(&recorded, true)).unwrap(),
            delivered
        );
        assert_eq!(recorded.swarm.preempt, original.swarm.preempt);

        // A guest-made scenario replays its observed plan explicitly.
        let guest_args = campaign_args(&["--load", "chain"]);
        let mut guest = scenario_for(&guest_args, seed, &Derivation::CURRENT);
        let events = r#"{"source":"nemesis","kind":"plan","container":"tempo","guest_time_ns":1,"detail":{"kills":[{"at_secs":20,"down_secs":1}]}}"#;
        guest.record_observed(Observed::from_events(events));
        let c = guest_config(&guest, true);
        assert_eq!(c["nemesis_plan"][0]["at_secs"], 20);
        // ...while the original run was delivered without it.
        assert!(cfg(&guest_args, seed).get("nemesis_plan").is_none());
    }

    #[test]
    fn branch_and_moments_parse() {
        let Cmd::Branch(b) = Cli::parse_from([
            "x",
            "branch",
            "out/seed-3",
            "--at",
            "42.5",
            "--seeds",
            "50",
            "--vary",
            "both",
        ])
        .cmd
        else {
            unreachable!()
        };
        assert_eq!(b.run_dir, Path::new("out/seed-3"));
        assert_eq!(b.at, At::RunSecs(42.5));
        assert_eq!((b.seeds, b.seed_start), (50, 0));
        assert_eq!(b.vary, Vary::Both);
        assert!(b.out.is_none() && !b.force);
        // Defaults: four branches with fresh randomness.
        let Cmd::Branch(b) =
            Cli::parse_from(["x", "branch", "d", "--at", "input:900", "--out", "o"]).cmd
        else {
            unreachable!()
        };
        assert_eq!((b.at, b.seeds, b.vary), (At::Input(900), 4, Vary::Rng));
        assert_eq!(b.out.as_deref(), Some(Path::new("o")));
        for vary in ["none", "rng", "decisions", "both"] {
            assert!(Cli::try_parse_from(["x", "branch", "d", "--at", "1", "--vary", vary]).is_ok());
        }
        // --at is required and checked.
        assert!(Cli::try_parse_from(["x", "branch", "d"]).is_err());
        assert!(Cli::try_parse_from(["x", "branch", "d", "--at", "soon"]).is_err());
        assert!(Cli::try_parse_from(["x", "branch", "d", "--at", "1", "--vary", "all"]).is_err());
        let Cmd::Moments(m) = Cli::parse_from(["x", "moments", "out/seed-3"]).cmd else {
            unreachable!()
        };
        assert_eq!(m.run_dir, Path::new("out/seed-3"));
    }

    #[test]
    fn manifests_without_branch_info_still_load() {
        let env = Environment {
            vmlinux: manifest::InputFile {
                path: "k".into(),
                sha256: "0".into(),
                bytes: 0,
            },
            initrd: manifest::InputFile {
                path: "i".into(),
                sha256: "0".into(),
                bytes: 0,
            },
            images: manifest::InputFile {
                path: "t".into(),
                sha256: "0".into(),
                bytes: 0,
            },
            compose: manifest::InputFile {
                path: "c".into(),
                sha256: "0".into(),
                bytes: 0,
            },
            image_metadata: vec![],
            bedrock_ko: Default::default(),
            build_info: Default::default(),
            tsc_frequency: DEFAULT_TSC_FREQUENCY,
            boot_seed: 1,
        };
        let m = Manifest {
            version: manifest::MANIFEST_VERSION,
            seed: 3,
            bedrock_dst: json!({}),
            environment: env,
            campaign: json!({}),
            warm_checkpoint_instructions: 10,
            branch_end_instructions: 20,
            rng_seed: 3,
            preempt: json!({}),
            tape: None,
            branch: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("branch").is_none());
        let back: Manifest = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
        // A campaign seed is one stretch of its rng_seed stream.
        assert_eq!(
            rng_streams(&m),
            [RngStream {
                from_input: 0,
                seed: 3,
                skip: 0
            }]
        );
        let mut b = m.clone();
        b.branch = Some(BranchInfo {
            parent: "p/seed-3".into(),
            parent_seed: 3,
            parent_tape_sha256: "ab".into(),
            at: "12s".into(),
            at_instructions: 15,
            at_run_secs: 12.0,
            cut: Cut { random: 4, io: 1 },
            vary: Vary::Both,
            branch: 2,
            rng_seed: Some(9),
            rng_streams: vec![RngStream {
                from_input: 4,
                seed: 9,
                skip: 0,
            }],
            actions: vec![HostAction {
                at_instructions: 15,
                command: "tempo-dst redecide '{}'".into(),
            }],
        });
        let back: Manifest = serde_json::from_value(serde_json::to_value(&b).unwrap()).unwrap();
        assert_eq!(back, b);
        assert_eq!(rng_streams(&b)[0].seed, 9);
    }

    #[test]
    fn scenario_args_override_the_campaign_for_coverage() {
        let args = campaign_args(&["--load", "chain"]);
        let mut s = scenario_for(&args, 1, &Derivation::CURRENT);
        s.nemesis.enabled = false;
        s.run_secs = 60;
        let a = args.with_scenario(&s);
        assert!(a.no_nemesis);
        assert_eq!(required(&a), ["S/load-included", "S/chain-appended"]);
    }
}
