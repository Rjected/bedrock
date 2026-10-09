// SPDX-License-Identifier: GPL-2.0

//! Seeded DST campaigns over a warm checkpoint, for any workload that
//! implements the workload contract (`workload.rs`; the Tempo workload in
//! workloads/tempo-dst is the reference implementation).
//!
//! Boots the guest once, runs the workload's `warmup` hook until it reports
//! ready, and checkpoints. Each seed then forks a branch whose controlled
//! randomness (RDRAND, getrandom) and forced preemption ([`preempt_for`])
//! are pure functions of the seed, asks the workload's host-side planner
//! for the seed's guest config, decisions and required coverage, starts the
//! workload (`start` hook), runs `--run-secs` of virtual time, finalizes
//! (`finalize` hook), and collects artifacts:
//!
//! ```text
//! <out>/campaign.json            arguments (incl. workload_args), for replay
//! <out>/seed-<n>/config.json     the workload config as delivered
//! <out>/seed-<n>/assertions.jsonl, events.jsonl, finalize.json, *.log
//! <out>/seed-<n>/console.log     guest serial output for the branch
//! <out>/seed-<n>/verdict.json
//! <out>/seed-<n>/scenario.json   every decision, by value (the planner's)
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
mod verdict;
mod workload;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bedrock_dst_contract::{
    ArgsRequest, CampaignInfo, ObserveRequest, PlanRequest, Preempt, Redraw, RunParams, VirtSecs,
    WarmupInfo, WarmupRequest,
};
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
use serde::{Deserialize, Serialize};
use workload::{Opaque, Workload, WorkloadPlan, WorkloadPlanRequest};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Guest files collected per run, relative to /bedrock.
const COLLECT: &[&str] = &[
    "events.jsonl",
    "out/finalize.json",
    "out/oracle.log",
    "out/nemesis.log",
];
/// Where the `finalize` hook leaves the run's assertions (every writer's
/// records since the branch's `start`, merged in timestamp order).
const ASSERTIONS_OUT: &str = "/bedrock/out/assertions.jsonl";
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
/// Per-seed files next to the collected artifacts.
const SCENARIO_FILE: &str = "scenario.json";
const TAPE_FILE: &str = "tape.bin";
const MANIFEST_FILE: &str = "manifest.json";
/// Virtual seconds between warmup hook calls.
const WARMUP_STEP_SECS: u64 = 5;

/// How decisions are derived from a seed: the driver's preemption salt and
/// the salt handed to the workload's planner, so a test can change the
/// derivation and check that a recorded scenario does not care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Derivation {
    preempt_salt: u64,
    /// Passed to the planner as `derivation_salt`.
    salt: Option<u64>,
}

impl Derivation {
    const CURRENT: Self = Self {
        preempt_salt: PREEMPT_SALT,
        salt: None,
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
                salt: Some(x),
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

/// Per-seed forced preemption: a pure function of the campaign arguments
/// and the seed, drawn with a host-side PRNG (never guest randomness), so
/// `replay` re-derives it. Off for every seed with `--no-preempt`.
fn preempt_for(args: &CampaignArgs, seed: u64) -> Preempt {
    preempt_for_with(args, seed, &Derivation::CURRENT)
}

fn preempt_for_with(args: &CampaignArgs, seed: u64, d: &Derivation) -> Preempt {
    if args.no_preempt {
        return Preempt::default();
    }
    let r = splitmix64(seed ^ d.preempt_salt);
    let period = PREEMPT_PERIODS[(r % PREEMPT_PERIODS.len() as u64) as usize];
    if period == 0 {
        return Preempt::default();
    }
    Preempt {
        period,
        seed: splitmix64(r),
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
        .any(|s| preempt_for(args, s).period != 0)
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
    /// `decisions` (the workload's planner re-draws the decisions not yet
    /// applied), or `both`.
    #[arg(long, value_enum, default_value_t = Vary::Rng)]
    vary: Vary,
    /// Branch even if manifest.json does not match the current binaries.
    #[arg(long)]
    force: bool,
    /// Planner to use instead of the recorded campaign's.
    #[arg(long)]
    workload_planner: Option<PathBuf>,
}

#[derive(ClapArgs, Debug)]
struct MomentsArgs {
    /// A recorded seed directory.
    run_dir: PathBuf,
    /// Planner to use instead of the recorded campaign's.
    #[arg(long)]
    workload_planner: Option<PathBuf>,
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
    /// Planner to use instead of the recorded campaign's.
    #[arg(long)]
    workload_planner: Option<PathBuf>,
}

fn default_workload_cmd() -> String {
    "tempo-dst".into()
}

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
    #[arg(long, default_value_t = 1)]
    seeds: u64,
    /// Virtual seconds per branch before finalize.
    #[arg(long, default_value_t = 180)]
    run_secs: u64,
    /// Passed to the workload's warmup hook (e.g. the head a node must
    /// reach before the warm checkpoint).
    #[arg(long, default_value_t = 10)]
    warm_blocks: u64,
    /// Virtual seconds allowed for boot plus warmup.
    #[arg(long, default_value_t = 900)]
    warm_timeout_secs: u64,
    /// Disable per-seed forced preemption (see `PREEMPT_PERIODS`).
    #[arg(long)]
    #[serde(default)]
    no_preempt: bool,
    /// The workload's command in the guest: the driver runs its `warmup`,
    /// `start` and `finalize` hooks (see workload.rs).
    #[arg(long, default_value = "tempo-dst")]
    #[serde(default = "default_workload_cmd")]
    workload_cmd: String,
    /// The workload's host-side planner (its `parse-args`, `config` and
    /// `observe` hooks); default: `--workload-cmd` from PATH.
    #[arg(long)]
    #[serde(default)]
    workload_planner: Option<PathBuf>,
    /// `KEY=VALUE` workload argument (repeatable), applied over
    /// `--workload-config`; the planner's `parse-args` hook types and
    /// checks them. Recorded as `workload_args`.
    #[arg(long = "workload-arg", value_name = "KEY=VALUE")]
    #[serde(skip)]
    workload_arg: Vec<String>,
    /// A JSON file of workload arguments.
    #[arg(long)]
    #[serde(skip)]
    workload_config: Option<PathBuf>,
    /// The workload's arguments as its planner parsed them (opaque here).
    #[arg(skip)]
    #[serde(default)]
    workload_args: Option<Opaque>,
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
    /// `KEY=VALUE` recorded in every manifest.json (e.g. `node=<rev>`).
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
    /// Has the planner parse `--workload-config` and `--workload-arg` into
    /// the workload's arguments (once, for a new campaign).
    fn resolve_workload_args(&mut self) -> Result<()> {
        if self.workload_args.is_none() {
            let req = ArgsRequest {
                config_file: self
                    .workload_config
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                args: self.workload_arg.clone(),
            };
            self.workload_args = Some(self.workload(None).parse_args(&req)?);
        }
        Ok(())
    }

    /// A campaign.json (or manifest `campaign`). One without
    /// `workload_args` predates the workload contract and is refused rather
    /// than run with the workload's defaults.
    fn from_json(text: &str) -> Result<Self> {
        let args: CampaignArgs = serde_json::from_str(text)?;
        if args.workload_args.is_none() {
            return Err(
                "no workload_args: the campaign predates the workload contract \
                        and cannot be replayed by this bedrock-dst"
                    .into(),
            );
        }
        Ok(args)
    }

    fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::from_json(&text).map_err(|e| format!("{}: {e}", path.display()).into())
    }

    /// The workload, with its planner overridden by `planner`.
    fn workload(&self, planner: Option<&Path>) -> Workload {
        Workload {
            cmd: self.workload_cmd.clone(),
            planner: planner
                .map(Path::to_path_buf)
                .or_else(|| self.workload_planner.clone())
                .unwrap_or_else(|| PathBuf::from(&self.workload_cmd)),
        }
    }

    /// The generic campaign fields handed to the workload's hooks.
    fn campaign_info(&self) -> CampaignInfo {
        CampaignInfo {
            run_secs: self.run_secs,
            warm_blocks: self.warm_blocks,
            warm_timeout_secs: self.warm_timeout_secs,
            seed_start: self.seed_start,
            seeds: self.seeds,
        }
    }

    /// The workload's arguments (`{}` until resolved: the workload's
    /// defaults).
    fn args(&self) -> Opaque {
        self.workload_args
            .clone()
            .unwrap_or_else(|| Opaque::parse("{}").expect("valid JSON"))
    }

    /// The planner request for `seed`: derive its plan.
    fn plan_request(&self, seed: u64, d: &Derivation) -> WorkloadPlanRequest {
        PlanRequest {
            campaign: self.campaign_info(),
            args: self.args(),
            seed,
            preempt: preempt_for_with(self, seed, d),
            derivation_salt: d.salt,
            decisions: None,
            redraw: None,
            rng_seed: None,
        }
    }

    /// The planner request for recorded decisions `s` of seed `seed`.
    fn recorded_request(&self, seed: u64, s: &Opaque) -> WorkloadPlanRequest {
        PlanRequest {
            decisions: Some(s.clone()),
            ..self.plan_request(seed, &Derivation::CURRENT)
        }
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

/// The guest command of the `warmup` hook. Nothing seed-specific: every
/// seed and every one-seed replay must rebuild the same warm prefix.
fn warmup_cmd(args: &CampaignArgs) -> Result<String> {
    let req = WarmupRequest {
        warmup: WarmupInfo {
            run_secs: args.run_secs,
            warm_blocks: args.warm_blocks,
            warm_timeout_secs: args.warm_timeout_secs,
        },
        args: args.args(),
    };
    Ok(args
        .workload(None)
        .guest_cmd("warmup", Some(&serde_json::to_string(&req)?)))
}

/// Boots and warms the shared prefix: runs the workload's `warmup` hook
/// every [`WARMUP_STEP_SECS`] until it reports `"ready": true`.
/// `probe_preempt`: some branch will use forced preemption, so check the
/// ioctl exists before the long boot.
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

    let wl = args.workload(None);
    let cmd = warmup_cmd(args)?;
    let mut warm = ready.branch()?;
    let mut last = String::new();
    loop {
        if warm.current_time() >= deadline {
            return Err(format!(
                "the workload was not ready before the warm timeout (last warmup status: {last})"
            )
            .into());
        }
        warm.run_for(secs(WARMUP_STEP_SECS))?;
        let (code, out) = host_bash(&mut warm, &cmd)?;
        if code != 0 {
            return Err(format!("{} warmup failed ({code}): {}", wl.cmd, out.trim()).into());
        }
        let status = workload::warmup_status(&out)
            .ok_or_else(|| format!("{} warmup printed no status: {out}", wl.cmd))?;
        last = status.detail;
        println!(
            "warmup vt {:.1}s: {}{last}",
            warm.current_time().as_secs_f64(),
            if status.ready { "ready, " } else { "" }
        );
        if status.ready {
            break;
        }
    }
    let (_, mem) = host_bash(&mut warm, MEM_REPORT)?;
    fs::write(args.out.join("guest-mem.txt"), mem)?;
    let cp = warm.checkpoint()?;
    println!("warm checkpoint at vt {:.1}s", cp.time().as_secs_f64());
    Ok(cp)
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
    run: RunParams<Opaque>,
    /// With an `original` scenario: where this run's decisions differ.
    decision_diff: Vec<String>,
    start: VirtTime,
    end: VirtTime,
    recording: Option<InputRecording>,
}

/// What a seed runs: the workload's plan, the exact config to deliver
/// (the plan's, or a tape's recorded one), and for a replay the recorded
/// decisions it reproduces (compared with this run's).
struct SeedPlan<'a> {
    plan: &'a WorkloadPlan,
    config: &'a str,
    original: Option<&'a Opaque>,
}

impl<'a> SeedPlan<'a> {
    fn new(plan: &'a WorkloadPlan, config: &'a str) -> Self {
        Self {
            plan,
            config,
            original: None,
        }
    }

    fn run(&self) -> &RunParams<Opaque> {
        &self.plan.run
    }
}

/// Runs `plan` on a branch of `warm` and collects its artifacts into `dir`.
/// `actions`: host commands to issue mid-run (a replayed branch's).
#[allow(clippy::too_many_arguments)]
fn run_seed(
    args: &CampaignArgs,
    wl: &Workload,
    warm: &Checkpoint,
    sink: &ConsoleSink,
    plan: &SeedPlan<'_>,
    inputs: Inputs,
    actions: &[HostAction],
    dir: &Path,
) -> Result<SeedRun> {
    fs::create_dir_all(dir)?;
    sink.open(&dir.join("console.log"))?;
    let run = plan.run();
    let mut record_tape = false;
    let (b, tape) = match inputs {
        // Per-seed randomness: RDRAND and guest getrandom() come from the
        // hypervisor's xorshift stream seeded per branch, served in-kernel
        // or (the same values) from the driver.
        Inputs::Seeded { record } if args.kernel_rng => {
            let mut b = warm.branch()?;
            b.reseed_rng(run.rng_seed)?;
            if record {
                b.set_record_inputs(true)?;
                record_tape = true;
            }
            (b, None)
        }
        Inputs::Seeded { record } => {
            // A source branch records its inputs anyway.
            record_tape = record;
            let b = warm.branch_with_input_source(SeededSource::new(run.rng_seed))?;
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
    fs::write(dir.join("config.json"), plan.config)?;
    let started = begin(wl, run, b, tape, plan.config)?;
    finish(wl, plan, started, actions, record_tape, dir)
}

/// A seed's branch after the workload's `start` hook.
struct Started {
    d: Driver,
    /// Where the branch forked (the warm checkpoint).
    start: VirtTime,
}

/// Applies the scenario's forced preemption to `b` and delivers `config`
/// with the workload's `start` hook.
fn begin(
    wl: &Workload,
    run: &RunParams<Opaque>,
    b: Branch,
    tape: Option<TapeCursor>,
    config: &str,
) -> Result<Started> {
    let seed = run.seed;
    let preempt = run.preempt;
    let mut d = Driver { b, tape };
    // Per-seed forced preemption, scoped to this branch (and inherited by
    // checkpoints of it); it is driver-side, so it applies to every workload.
    apply_preempt(&mut d.b, preempt)?;
    println!(
        "seed {seed}: branch forked at vt {:.1}s (preempt period {})",
        d.b.current_time().as_secs_f64(),
        preempt.period
    );
    let start = d.b.current_time();
    let (code, out) = host_bash(&mut d, &wl.guest_cmd("start", Some(config)))?;
    println!(
        "seed {seed}: started at vt {:.1}s",
        d.b.current_time().as_secs_f64()
    );
    if code != 0 {
        return Err(format!("{} start failed ({code}): {out}", wl.cmd).into());
    }
    Ok(Started { d, start })
}

/// Runs a started branch to `run_secs` after its fork (issuing `actions`
/// at their times), finalizes, and collects artifacts, verdict and scenario
/// (as the workload observed it) into `dir`.
fn finish(
    wl: &Workload,
    plan: &SeedPlan<'_>,
    started: Started,
    actions: &[HostAction],
    record_tape: bool,
    dir: &Path,
) -> Result<SeedRun> {
    let run = plan.run();
    let Started { mut d, start } = started;
    let end = start + secs(run.run_secs);
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
            run.seed,
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
        let (code, out) = host_bash(&mut d, &wl.guest_cmd("finalize", None))?;
        if code != 0 {
            return Err(format!("{} finalize failed ({code}): {out}", wl.cmd).into());
        }
        assertions = fetch(&mut d, ASSERTIONS_OUT, 0)?;
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
        let rec = bedrock_assertions::Assertion::always(
            bedrock_assertions::Condition::Bool(false),
            format!("D/guest-exited: {kind}"),
            bedrock_assertions::Location::new("bedrock-dst", 0, 0),
        );
        assertions.extend_from_slice(format!("{}\n", serde_json::to_string(&rec)?).as_bytes());
    }
    fs::write(dir.join("assertions.jsonl"), &assertions)?;
    let mut v = verdict::aggregate(&String::from_utf8_lossy(&assertions), &plan.plan.required);
    v.swarm = Some(run.swarm.clone());
    fs::write(dir.join("verdict.json"), serde_json::to_vec_pretty(&v)?)?;

    let observed = wl.observe(&ObserveRequest {
        decisions: plan.plan.decisions.clone(),
        original: plan.original.cloned(),
        events_path: dir.join("events.jsonl").to_string_lossy().into_owned(),
    })?;
    fs::write(dir.join(SCENARIO_FILE), observed.decisions.get())?;
    let recording = record_tape.then(|| d.b.input_recording().clone());
    Ok(SeedRun {
        verdict: v,
        run: run.clone(),
        decision_diff: observed.decision_diff,
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
        seed: run.run.seed,
        bedrock_dst: manifest::bedrock_dst_identity(),
        environment: env.clone(),
        campaign: Opaque::parse(&serde_json::to_string(args)?)?,
        warm_checkpoint_instructions: run.start.instructions(),
        branch_end_instructions: run.end.instructions(),
        rng_seed: run.run.rng_seed,
        preempt: run.run.preempt,
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
        args.workload_planner.as_deref(),
        DEFAULT_TSC_FREQUENCY,
        args.boot_seed,
    )?)
}

/// One seed in summary.json.
#[derive(Serialize)]
struct SeedSummary {
    pass: bool,
    failures: Vec<String>,
    swarm: Option<Opaque>,
}

/// `<out>/summary.json`.
#[derive(Serialize)]
struct CampaignSummary {
    seeds: BTreeMap<u64, SeedSummary>,
    failed: usize,
    wall_secs: u64,
    seeds_per_hour: f64,
}

/// Prints a plan's notes for `who`.
fn print_notes(who: &str, plan: &WorkloadPlan) {
    for n in &plan.notes {
        println!("{who}: {n}");
    }
}

fn campaign(mut args: CampaignArgs) -> Result<()> {
    fs::create_dir_all(&args.out)?;
    args.resolve_workload_args()?;
    fs::write(
        args.out.join("campaign.json"),
        serde_json::to_vec_pretty(&args)?,
    )?;
    let wl = args.workload(None);
    let env = probe_environment(&args)?;
    let derivation = Derivation::from_env();
    let sink = Arc::new(ConsoleSink::default());
    sink.open(&args.out.join("boot-console.log"))?;
    let wall = Instant::now();
    let warm = boot(&args, sink.clone(), any_preempt(&args))?;
    let mut seeds = BTreeMap::new();
    for seed in args.seed_start..args.seed_start + args.seeds {
        let t = Instant::now();
        let dir = args.out.join(format!("seed-{seed}"));
        let plan = wl.config(&args.plan_request(seed, &derivation))?;
        print_notes(&format!("seed {seed}"), &plan);
        let run = run_seed(
            &args,
            &wl,
            &warm,
            &sink,
            &SeedPlan::new(&plan, plan.config.get()),
            Inputs::Seeded {
                record: !args.no_tape,
            },
            &[],
            &dir,
        )?;
        write_trace(&args, &env, &run, &dir, None)?;
        let v = run.verdict;
        let failures: Vec<String> = v.failures.keys().cloned().collect();
        println!(
            "seed {seed}: {} ({:.0}s wall) {failures:?}",
            if v.pass { "PASS" } else { "FAIL" },
            t.elapsed().as_secs_f64(),
        );
        seeds.insert(
            seed,
            SeedSummary {
                pass: v.pass,
                failures,
                swarm: v.swarm,
            },
        );
    }
    let hours = wall.elapsed().as_secs_f64() / 3600.0;
    let report = CampaignSummary {
        failed: seeds.values().filter(|s| !s.pass).count(),
        seeds,
        wall_secs: wall.elapsed().as_secs(),
        seeds_per_hour: args.seeds as f64 / hours.max(1e-9),
    };
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
fn load_recorded(run_dir: &Path, force: bool, planner: Option<&Path>) -> Result<Recorded> {
    let m: Manifest = serde_json::from_slice(&fs::read(run_dir.join(MANIFEST_FILE))?)
        .map_err(|e| format!("{}: {e}", run_dir.join(MANIFEST_FILE).display()))?;
    // The binaries are the ones the tape was recorded with, wherever the
    // manifest says they were.
    let mut args = CampaignArgs::from_json(m.campaign.get())?;
    args.workload_planner = planner.map(Path::to_path_buf).or(args.workload_planner);
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

fn read_scenario(run_dir: &Path) -> Result<Opaque> {
    let path = run_dir.join(SCENARIO_FILE);
    Opaque::parse(&fs::read_to_string(&path)?)
        .map_err(|e| format!("{}: {e}", path.display()).into())
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
    let planner = r.workload_planner.as_deref();
    if !r.tape && !r.scenario {
        // Re-derive the seed from the campaign arguments.
        let mut args = CampaignArgs::read(&out.join("campaign.json"))?;
        let seed = seed_of(run_dir)?;
        let replay_out = run_dir.join("replay");
        args.out = replay_out.clone();
        args.seed_start = seed;
        args.seeds = 1;
        args.workload_planner = planner.map(Path::to_path_buf).or(args.workload_planner);
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
    let mode = if r.tape { "tape" } else { "scenario" };
    let replay_out = run_dir.join(format!("replay-{mode}"));
    fs::create_dir_all(&replay_out)?;
    let sink = Arc::new(ConsoleSink::default());
    let dir = replay_out.join(&name);
    let (args, rec) = if r.tape {
        let rec = load_recorded(run_dir, r.force, planner)?;
        (rec.args.clone(), Some(rec))
    } else {
        let mut args = CampaignArgs::read(&out.join("campaign.json"))?;
        args.workload_planner = planner.map(Path::to_path_buf).or(args.workload_planner);
        (args, None)
    };
    let mut args = args;
    args.out = replay_out.clone();
    fs::write(
        replay_out.join("campaign.json"),
        serde_json::to_vec_pretty(&args)?,
    )?;
    let wl = args.workload(None);
    // The recorded decisions, delivered as they are: the planner derives
    // nothing from the seed.
    let plan =
        wl.config(&args.recorded_request(seed_of(run_dir).unwrap_or_default(), &recorded))?;
    let seed = plan.run.seed;
    let preempt = plan.run.preempt.period != 0;
    sink.open(&replay_out.join("boot-console.log"))?;
    let run = match rec {
        Some(rec) => {
            let warm = boot_recorded(&rec, &args, sink.clone(), preempt, r.force)?;
            // The exact config the original delivered (it is also in the
            // tape's first host action, which the driver must match).
            let config = fs::read_to_string(run_dir.join("config.json"))?;
            let actions = branch.map(|b| b.actions).unwrap_or_default();
            let sp = SeedPlan {
                original: Some(&recorded),
                ..SeedPlan::new(&plan, &config)
            };
            run_seed(
                &args,
                &wl,
                &warm,
                &sink,
                &sp,
                Inputs::Tape(rec.tape),
                &actions,
                &dir,
            )?
        }
        None => {
            let warm = boot(&args, sink.clone(), preempt)?;
            let sp = SeedPlan {
                original: Some(&recorded),
                ..SeedPlan::new(&plan, plan.config.get())
            };
            let run = run_seed(
                &args,
                &wl,
                &warm,
                &sink,
                &sp,
                Inputs::Seeded { record: true },
                &[],
                &dir,
            )?;
            let env = probe_environment(&args)?;
            write_trace(&args, &env, &run, &dir, None)?;
            run
        }
    };

    let diverged = diverged_artifacts(run_dir, &dir);
    let exact = r.tape || plan.replays_exactly;
    if !run.decision_diff.is_empty() {
        return Err(format!(
            "{mode} replay of seed {seed} made other decisions:\n  {}",
            run.decision_diff.join("\n  ")
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
                 (the workload made decisions in the guest, so the replay delivered a different \
                 config; replay --tape, or a campaign whose planner makes every decision, \
                 replays exactly)"
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

/// `<branches>/summary.json`.
#[derive(Serialize)]
struct BranchSummary<'a> {
    parent: String,
    at: String,
    at_instructions: u64,
    at_run_secs: f64,
    cut: Cut,
    vary: Vary,
    branches: &'a [BranchResult],
}

/// `bedrock-dst branch`: re-execute a recorded seed to a moment, checkpoint
/// there, and run `--seeds` branches from the checkpoint.
fn branch_cmd(a: &BranchArgs) -> Result<()> {
    let run_dir = a.run_dir.as_path();
    let rec = load_recorded(run_dir, a.force, a.workload_planner.as_deref())?;
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
    let wl = args.workload(None);
    let parent_seed = rec.manifest.seed;
    let parent_plan = wl.config(&args.recorded_request(parent_seed, &parent))?;
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
        parent_plan.run.preempt.period != 0,
        a.force,
    )?;
    let fork = warm.time();
    let target = a.at.resolve(tape_rec, fork)?;
    let end = fork + secs(parent_plan.run.run_secs);
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
    let mut started = begin(&wl, &parent_plan.run, b, Some(cursor), &config)?;
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
    let Started { d, start } = started;
    let checkpoint = d.b.checkpoint()?;
    let prefix_console = fs::read(out.join("prefix-console.log")).unwrap_or_default();
    let streams = rng_streams(&rec.manifest);
    // Decision times count from the workload's start hook (the tape's first
    // host action).
    let after_start = VirtSecs((at - start_io).as_secs_f64());

    let mut results = Vec::new();
    for i in a.seed_start..a.seed_start + a.seeds {
        let name = format!("branch-{i}");
        let dir = out.join(&name);
        fs::create_dir_all(&dir)?;
        sink.open_with(&dir.join("console.log"), &prefix_console)?;
        let seed = branching::branch_seed(&parent_sha, at, i);
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
        // The branch's decisions: the parent's, with fresh randomness
        // recorded and (`--vary decisions|both`) what was not applied by the
        // moment re-drawn by the planner.
        let plan = wl.config(&PlanRequest {
            redraw: a.vary.decisions().then_some(Redraw {
                after: after_start,
                seed,
            }),
            rng_seed,
            ..args.recorded_request(parent_seed, &parent)
        })?;
        print_notes(&name, &plan);
        if a.vary.decisions() {
            let action = plan
                .action
                .as_deref()
                .ok_or("the planner returned no action for the re-drawn decisions")?;
            actions.push(HostAction {
                at_instructions: at.instructions(),
                command: format!("{} {action}", wl.cmd),
            });
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
            &wl,
            &SeedPlan::new(&plan, &config),
            Started {
                d: Driver { b, tape: cursor },
                start,
            },
            &actions,
            true,
            &dir,
        )?;
        let info = BranchInfo {
            parent: run_dir.to_string_lossy().into_owned(),
            parent_seed,
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
        serde_json::to_vec_pretty(&BranchSummary {
            parent: run_dir.to_string_lossy().into_owned(),
            at: a.at.to_string(),
            at_instructions: at.instructions(),
            at_run_secs: at_run,
            cut,
            vary: a.vary,
            branches: &results,
        })?,
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
    let args = CampaignArgs::from_json(m.campaign.get())?;
    let wl = args.workload(a.workload_planner.as_deref());
    let s = read_scenario(run_dir)?;
    let run_secs = wl.config(&args.recorded_request(m.seed, &s))?.run.run_secs;
    let obs = wl.observe(&ObserveRequest {
        decisions: s,
        original: None,
        events_path: run_dir.join("events.jsonl").to_string_lossy().into_owned(),
    })?;
    let summary = m.tape.as_ref().ok_or("the run recorded no tape")?;
    let tape = Tape::from_bytes(&fs::read(run_dir.join(&summary.file))?)?;
    let start = VirtTime::from_instructions(m.warm_checkpoint_instructions, tape.tsc_frequency);
    let start_label = format!("`{}` (branch from after it)", wl.guest_cmd("start", None));
    let list = branching::moments(
        &obs.moments,
        obs.events_origin_ns,
        &start_label,
        &tape.recording,
        start,
        run_secs,
    );
    println!(
        "{}: forked at vt {:.3}s, {} s run, {} randomness inputs on the tape",
        run_dir.display(),
        start.as_secs_f64(),
        run_secs,
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
    let failed = fs::read(run_dir.join("verdict.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<verdict::VerdictPass>(&b).ok())
        .is_some_and(|v| !v.pass);
    if failed
        && !obs
            .moments
            .iter()
            .any(|m| m.what.starts_with("first failure"))
    {
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
    fn workload_flags_parse() {
        let a = campaign_args(&[]);
        assert_eq!(a.workload_cmd, "tempo-dst");
        assert!(a.workload_planner.is_none() && a.workload_args.is_none());
        assert_eq!(a.workload(None).planner, PathBuf::from("tempo-dst"));
        let a = campaign_args(&[
            "--workload-cmd",
            "wl",
            "--workload-planner",
            "/p/wl",
            "--workload-arg",
            "a=1",
            "--workload-arg",
            "b=x",
            "--workload-config",
            "w.json",
        ]);
        assert_eq!(a.workload_arg, ["a=1", "b=x"]);
        assert_eq!(a.workload_config.as_deref(), Some(Path::new("w.json")));
        let wl = a.workload(None);
        assert_eq!(
            (wl.cmd.as_str(), wl.planner.as_path()),
            ("wl", Path::new("/p/wl"))
        );
        assert_eq!(a.workload(Some(Path::new("/q"))).planner, Path::new("/q"));
        // The raw flags are not recorded; the parsed arguments are.
        let mut a = a;
        a.workload_args = Some(Opaque::parse(r#"{"a":1,"b":"x"}"#).unwrap());
        let text = serde_json::to_string(&a).unwrap();
        assert!(!text.contains("workload_arg\"") && !text.contains("workload_config"));
        let back = CampaignArgs::from_json(&text).unwrap();
        assert_eq!(back.workload_args, a.workload_args);
        assert_eq!(back.workload_cmd, "wl");
        assert_eq!(back.args().get(), r#"{"a":1,"b":"x"}"#);
        // Unresolved: the workload's defaults.
        assert_eq!(campaign_args(&[]).args().get(), "{}");
        // A campaign.json without workload_args (from before the contract)
        // is refused, not run with the workload's defaults.
        let mut old = campaign_args(&[]);
        old.workload_args = None;
        let err = CampaignArgs::from_json(&serde_json::to_string(&old).unwrap())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("predates the workload contract"), "{err}");
    }

    #[test]
    fn preempt_is_a_stable_per_seed_draw() {
        let args = campaign_args(&[]);
        let draws: Vec<_> = (0..64).map(|s| preempt_for(&args, s)).collect();
        // Pinned so a change to the draw (which would silently change what
        // old seeds mean) is caught.
        let periods: Vec<u64> = draws[..8].iter().map(|d| d.period).collect();
        assert_eq!(
            periods,
            [200_000, 0, 0, 0, 0, 2_000_000, 2_000_000, 20_000_000]
        );
        for p in &draws {
            assert!(PREEMPT_PERIODS.contains(&p.period));
            assert_eq!(
                p.period == 0,
                p.seed == 0,
                "seed is set exactly when enabled"
            );
        }
        for p in PREEMPT_PERIODS {
            assert!(
                draws.iter().any(|d| d.period == *p),
                "period {p} never drawn"
            );
        }
        // Delivered to the planner, which records it in the swarm.
        assert_eq!(args.plan_request(5, &Derivation::CURRENT).preempt, draws[5]);
        // --no-preempt turns it off for every seed.
        let off = campaign_args(&["--no-preempt"]);
        assert!((0..64).all(|s| preempt_for(&off, s) == Preempt::default()));
        assert!(!any_preempt(&off));
        assert!(any_preempt(&campaign_args(&["--seeds", "8"])));
        // A changed derivation draws other periods and salts the planner.
        let d = Derivation {
            preempt_salt: PREEMPT_SALT ^ 0xdead,
            salt: Some(0xdead),
        };
        assert_ne!(
            (0..16)
                .map(|s| preempt_for_with(&args, s, &d))
                .collect::<Vec<_>>(),
            draws[..16]
        );
        assert_eq!(args.plan_request(1, &d).derivation_salt, Some(0xdead));
        // ...but never changes recorded decisions' request.
        let s = Opaque::parse("{}").unwrap();
        assert_eq!(args.recorded_request(1, &s).derivation_salt, None);
    }

    #[test]
    fn warm_prefix_does_not_depend_on_the_seed_range() {
        // A one-seed replay rebuilds the warm checkpoint of a many-seed
        // campaign: the warmup command (which the guest runs) must match.
        let mut campaign = campaign_args(&["--seed-start", "11", "--seeds", "2"]);
        let mut replay = campaign_args(&["--seed-start", "12", "--seeds", "1"]);
        for a in [&mut campaign, &mut replay] {
            a.workload_args = Some(Opaque::parse(r#"{"k":"it's"}"#).unwrap());
        }
        let cmd = warmup_cmd(&campaign).unwrap();
        assert_eq!(cmd, warmup_cmd(&replay).unwrap());
        assert_eq!(
            cmd,
            r#"tempo-dst warmup '{"warmup":{"run_secs":180,"warm_blocks":10,"warm_timeout_secs":900},"args":{"k":"it'\''s"}}'"#
        );
        let other = campaign_args(&["--warm-blocks", "20"]);
        assert_ne!(
            warmup_cmd(&other).unwrap(),
            warmup_cmd(&campaign_args(&[])).unwrap()
        );
    }

    #[test]
    fn missing_preempt_ioctl_is_explained() {
        let msg = preempt_error(&io::Error::from_raw_os_error(ENOTTY)).to_string();
        assert!(msg.contains("lacks SET_PREEMPT_CONFIG"), "{msg}");
        assert!(msg.contains("--no-preempt"), "{msg}");
        let other = preempt_error(&io::Error::from_raw_os_error(22)).to_string();
        assert!(!other.contains("lacks"), "{other}");
    }

    /// A planner that answers every hook with a canned response.
    fn fake_planner(dir: &Path) -> PathBuf {
        let plan = r#"{"config":{"z":1,"a":[2]},"decisions":{
  "seed": 7, "x": "kept"
},"run":{"seed":7,"rng_seed":8,"run_secs":30,"preempt":{"period":0,"seed":0},"swarm":{"f":true}},"required":["S/a"],"replays_exactly":true,"action":"act '{}'","notes":["hi"]}"#;
        let script = format!(
            "#!/bin/sh\ncat > \"$(dirname \"$0\")/req-$1.json\"\ncase \"$1\" in\n\
             config) cat <<'EOF'\n{plan}\nEOF\n;;\n\
             parse-args) echo '{{\"k\": 1}}' ;;\n\
             observe) echo '{{\"decisions\": {{\"seed\": 7}}, \"decision_diff\": [\"d\"], \"moments\": [], \"events_origin_ns\": 5}}' ;;\n\
             *) echo \"bad hook $1\" >&2; exit 3 ;;\nesac\n"
        );
        let p = dir.join("planner");
        fs::write(&p, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn planner_hooks_speak_the_contract() {
        let dir = std::env::temp_dir().join(format!("bedrock-dst-planner-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut args = campaign_args(&["--workload-arg", "k=1", "--run-secs", "30"]);
        args.workload_planner = Some(fake_planner(&dir));
        args.resolve_workload_args().unwrap();
        assert_eq!(args.workload_args.as_ref().unwrap().get(), r#"{"k": 1}"#);
        let req: ArgsRequest =
            serde_json::from_slice(&fs::read(dir.join("req-parse-args.json")).unwrap()).unwrap();
        assert_eq!(req.args, ["k=1"]);
        let wl = args.workload(None);
        let plan = wl
            .config(&args.plan_request(7, &Derivation::CURRENT))
            .unwrap();
        // Workload payloads are kept byte for byte.
        assert_eq!(plan.config.get(), r#"{"z":1,"a":[2]}"#);
        assert_eq!(plan.decisions.get(), "{\n  \"seed\": 7, \"x\": \"kept\"\n}");
        assert_eq!(
            (plan.run.seed, plan.run.rng_seed, plan.run.run_secs),
            (7, 8, 30)
        );
        assert_eq!(
            plan.required,
            [bedrock_dst_contract::Signature::from("S/a")]
        );
        assert_eq!(plan.action.as_deref(), Some("act '{}'"));
        let sent: PlanRequest<Opaque, Opaque> =
            serde_json::from_slice(&fs::read(dir.join("req-config.json")).unwrap()).unwrap();
        assert_eq!(sent.seed, 7);
        assert_eq!(sent.args.get(), r#"{"k": 1}"#);
        assert_eq!(sent.campaign.run_secs, 30);
        let obs = wl
            .observe(&ObserveRequest {
                decisions: plan.decisions.clone(),
                original: None,
                events_path: "e".into(),
            })
            .unwrap();
        assert_eq!(obs.decision_diff, ["d"]);
        assert_eq!(obs.events_origin_ns, Some(5));
        // A failing hook reports its stderr.
        let bad = Workload {
            cmd: "x".into(),
            planner: dir.join("planner"),
        };
        let err = bad.call::<_, Opaque>("nope", &()).unwrap_err().to_string();
        assert!(err.contains("bad hook nope"), "{err}");
        let missing = Workload {
            cmd: "x".into(),
            planner: dir.join("none"),
        };
        assert!(missing
            .parse_args(&ArgsRequest::default())
            .unwrap_err()
            .to_string()
            .contains("--workload-planner"));
        fs::remove_dir_all(&dir).unwrap();
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
        assert!(b.out.is_none() && !b.force && b.workload_planner.is_none());
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
        assert!(Cli::try_parse_from(["x", "branch", "d"]).is_err());
        assert!(Cli::try_parse_from(["x", "branch", "d", "--at", "soon"]).is_err());
        assert!(Cli::try_parse_from(["x", "branch", "d", "--at", "1", "--vary", "all"]).is_err());
        let Cmd::Moments(m) =
            Cli::parse_from(["x", "moments", "out/seed-3", "--workload-planner", "p"]).cmd
        else {
            unreachable!()
        };
        assert_eq!(m.run_dir, Path::new("out/seed-3"));
        assert_eq!(m.workload_planner.as_deref(), Some(Path::new("p")));
    }

    #[test]
    fn manifests_round_trip_with_and_without_branch_info() {
        let file = |p: &str| manifest::InputFile {
            path: p.into(),
            sha256: "0".into(),
            bytes: 0,
        };
        let env = Environment {
            vmlinux: file("k"),
            initrd: file("i"),
            images: file("t"),
            compose: file("c"),
            image_metadata: vec![],
            bedrock_ko: Default::default(),
            workload_planner: None,
            build_info: Default::default(),
            tsc_frequency: DEFAULT_TSC_FREQUENCY,
            boot_seed: 1,
        };
        let mut args = campaign_args(&[]);
        args.workload_args = Some(Opaque::parse("{}").unwrap());
        let m = Manifest {
            version: manifest::MANIFEST_VERSION,
            seed: 3,
            bedrock_dst: manifest::bedrock_dst_identity(),
            environment: env,
            campaign: Opaque::parse(&serde_json::to_string(&args).unwrap()).unwrap(),
            warm_checkpoint_instructions: 10,
            branch_end_instructions: 20,
            rng_seed: 3,
            preempt: Preempt {
                period: 200_000,
                seed: 9,
            },
            tape: None,
            branch: None,
        };
        let text = serde_json::to_string_pretty(&m).unwrap();
        assert!(!text.contains("\"branch\": ") && !text.contains("\"workload_planner\": "));
        let back: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, m);
        assert_eq!(
            CampaignArgs::from_json(back.campaign.get())
                .unwrap()
                .run_secs,
            180
        );
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
                command: "wl redecide '{}'".into(),
            }],
        });
        let back: Manifest = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back, b);
        assert_eq!(rng_streams(&b)[0].seed, 9);
    }
}
