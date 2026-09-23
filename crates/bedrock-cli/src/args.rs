// SPDX-License-Identifier: GPL-2.0

//! CLI argument parsing using clap derive macros.

use clap::{Parser, ValueEnum};

use bedrock_vm::boot::defaults;
use bedrock_vm::{ExitTrigger, DEFAULT_TSC_FREQUENCY};

/// Boot and run deterministic Linux guests on the bedrock hypervisor.
#[derive(Parser, Debug)]
#[command(name = "bedrock-cli")]
#[command(about = "Boot and run deterministic Linux guests on the bedrock hypervisor")]
#[command(version)]
pub struct Args {
    /// Path to vmlinux ELF image (required for root VMs, unused for forked VMs)
    pub vmlinux: Option<String>,

    /// Guest memory size in MB
    #[arg(short = 'm', long, default_value_t = defaults::MEMORY_MB)]
    pub memory: usize,

    /// Kernel command line
    #[arg(short = 'c', long, default_value = defaults::CMDLINE)]
    pub cmdline: String,

    /// Path to initramfs/initrd image
    #[arg(short = 'i', long)]
    pub initramfs: Option<String>,

    /// Write serial output to file (in addition to stdout)
    #[arg(short = 'l', long = "serial-log-file")]
    pub serial_log_file: Option<String>,

    /// RDRAND emulation mode
    #[arg(short = 'r', long = "rdrand-mode", value_enum, default_value_t = RdrandMode::Seeded)]
    pub rdrand_mode: RdrandMode,

    /// Seed/value for RDRAND (hex with 0x prefix or decimal)
    #[arg(short = 's', long = "rdrand-seed", default_value_t = defaults::RDRAND_SEED, value_parser = parse_u64)]
    pub rdrand_seed: u64,

    /// Write the unified event stream to a JSONL file (enables the event stream)
    #[arg(long = "events-jsonl")]
    pub events_jsonl: Option<String>,

    /// Event categories to capture, comma-separated. One or more of:
    /// exit, serial, inject, randomness, io_channel, diagnostic, all.
    #[arg(
        long = "event-categories",
        default_value = "serial,inject,randomness,io_channel"
    )]
    pub event_categories: String,

    /// Single-step (MTF) for TSC range (e.g., 79800000-79810000)
    #[arg(long = "single-step", value_parser = parse_tsc_range)]
    pub single_step: Option<(u64, u64)>,

    /// Capture `Exit` records into the event stream. One of:
    ///   all              — every exit
    ///   at-shutdown      — one record at guest shutdown (hashes full memory)
    ///   at-tsc:<N>       — one record when emulated TSC reaches <N>
    ///   checkpoints:<N>  — one record every <N> emulated-TSC ticks
    /// Implies the `exit` event category; requires `--events-jsonl` to drain.
    #[arg(long = "exit-capture", value_parser = parse_exit_capture, verbatim_doc_comment)]
    pub exit_capture: Option<ExitCaptureArg>,

    /// Capture `Exit` records only after emulated TSC reaches this threshold
    #[arg(long = "capture-exits-after-tsc", value_parser = parse_u64)]
    pub capture_exits_after_tsc: Option<u64>,

    /// Stop VM when emulated TSC reaches this value
    #[arg(long = "stop-at-tsc", value_parser = parse_u64, conflicts_with = "stop_at_vt")]
    pub stop_at_tsc: Option<u64>,

    /// Stop VM when virtual time reaches this value (seconds, e.g. 3374.539)
    #[arg(long = "stop-at-vt", conflicts_with = "stop_at_tsc")]
    pub stop_at_vt: Option<f64>,

    /// Skip memory hashing in exit records (memory_hash will be 0)
    #[arg(long = "no-memory-hash")]
    pub no_memory_hash: bool,

    /// Intercept guest #PF exceptions (logged and reinjected for determinism analysis)
    #[arg(long = "intercept-pf")]
    pub intercept_pf: bool,

    /// Create a forked VM from an existing parent VM ID
    #[arg(long = "parent-id", value_parser = parse_u64)]
    pub parent_id: Option<u64>,

    /// Wait for Ctrl-C at snapshot/stop-tsc points (for forked VM testing)
    #[arg(long = "wait")]
    pub wait: bool,

    /// Wall-clock timeout in seconds (VM run ends after this duration)
    #[arg(long = "wall-clock-timeout", value_parser = parse_f64)]
    pub wall_clock_timeout: Option<f64>,

    /// Write exit statistics to a JSON file
    #[arg(long = "exit-stats-json")]
    pub exit_stats_json: Option<String>,

    /// Emulated TSC frequency in Hz (defaults to the kernel's built-in default)
    #[arg(long = "virt-tsc-frequency", value_parser = parse_u64)]
    pub virt_tsc_frequency: Option<u64>,

    /// Queue a deterministic I/O channel action for the guest's bedrock-io
    /// module. Repeatable; actions fire in order, one in flight at a time.
    ///
    /// Optional prefix for the earliest fire time (default: immediately):
    ///   `tsc=<N>:<action>` or `vt=<seconds>:<action>` (at DEFAULT_TSC_FREQUENCY).
    ///
    /// Action bodies (a `rec:` prefix also captures and prints the output):
    ///   `exec:<container>:<cmd>`    — `podman exec <container> /bin/sh -c <cmd>`
    ///   `exec:host:<cmd>`           — `/bin/sh -c <cmd>` outside any container
    ///
    /// Examples:
    ///   --io-action 'exec:host:uname -a'
    ///   --io-action 'rec:exec:host:uname -a'
    ///   --io-action 'tsc=300000000:rec:exec:host:date'
    ///   --io-action 'vt=120.0:exec:bitcoind1:bitcoin-cli getblockchaininfo'
    #[arg(long = "io-action", value_parser = parse_scheduled_io_action, verbatim_doc_comment)]
    pub io_actions: Vec<ScheduledIoAction>,

    /// Expose a host file to the guest over the file-transmission hypercall.
    /// Repeatable. Format: `<guest-name>=<host-path>`. E.g. for a podman workload:
    ///   --file compose.yaml=workloads/bitcoin/compose.yaml
    ///   --file images.tar=workloads/bitcoin/images.tar
    #[arg(long = "file", value_parser = parse_file_arg, verbatim_doc_comment)]
    pub files: Vec<FileArg>,
}

/// One `--file <guest-name>=<host-path>` mapping.
#[derive(Clone, Debug)]
pub struct FileArg {
    pub name: String,
    pub path: String,
}

/// One parsed `--io-action`.
#[derive(Clone, Debug)]
pub struct ScheduledIoAction {
    /// Earliest emulated TSC at which the run loop queues this action (0 =
    /// at startup).
    pub target_tsc: u64,
    pub action: IoAction,
}

/// A bash command to run on the I/O channel.
#[derive(Clone, Debug)]
pub struct IoAction {
    /// `None` runs on the guest host.
    pub container: Option<String>,
    pub command: String,
    /// Also capture stdout+stderr for printing (it always goes to the journal).
    pub record_output: bool,
}

impl Args {
    pub fn should_capture_exits(&self) -> bool {
        self.single_step.is_some() || self.exit_capture.is_some()
    }

    /// Trigger policy and TSC from `--single-step` (takes precedence) or
    /// `--exit-capture`.
    pub fn exit_trigger(&self) -> (ExitTrigger, u64) {
        if self.single_step.is_some() {
            (ExitTrigger::TscRange, 0)
        } else if let Some(ec) = &self.exit_capture {
            (ec.trigger, ec.target_tsc)
        } else {
            (ExitTrigger::Disabled, 0)
        }
    }
}

/// Parsed `--exit-capture`.
#[derive(Clone, Debug)]
pub struct ExitCaptureArg {
    pub trigger: ExitTrigger,
    /// `AtTsc` threshold / `Checkpoints` interval; 0 otherwise.
    pub target_tsc: u64,
}

/// Parse `--exit-capture`: `all`, `at-shutdown`, `at-tsc:<N>`, `checkpoints:<N>`.
fn parse_exit_capture(s: &str) -> Result<ExitCaptureArg, String> {
    let s = s.trim();
    let arg = match s {
        "all" => ExitCaptureArg {
            trigger: ExitTrigger::AllExits,
            target_tsc: 0,
        },
        "at-shutdown" => ExitCaptureArg {
            trigger: ExitTrigger::AtShutdown,
            target_tsc: 0,
        },
        _ if s.starts_with("at-tsc:") => ExitCaptureArg {
            trigger: ExitTrigger::AtTsc,
            target_tsc: parse_u64(&s["at-tsc:".len()..])?,
        },
        _ if s.starts_with("checkpoints:") => ExitCaptureArg {
            trigger: ExitTrigger::Checkpoints,
            target_tsc: parse_u64(&s["checkpoints:".len()..])?,
        },
        _ => {
            return Err(format!(
                "invalid --exit-capture '{}'. Expected: all | at-shutdown | at-tsc:<N> | checkpoints:<N>",
                s
            ))
        }
    };
    Ok(arg)
}

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RdrandMode {
    /// Use seeded PRNG (deterministic)
    #[default]
    Seeded,
    /// Exit to userspace
    Userspace,
}

/// Parse a u64 value from a string, supporting hex (0x prefix) and decimal.
fn parse_u64(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| format!("Invalid hex value: {}", s))
    } else {
        s.parse().map_err(|_| format!("Invalid number: {}", s))
    }
}

/// Parse a TSC range in the format "START-END".
fn parse_tsc_range(s: &str) -> Result<(u64, u64), String> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid TSC range '{}'. Expected format: START-END (e.g., 79800000-79810000)",
            s
        ));
    }
    let start = parse_u64(parts[0])?;
    let end = parse_u64(parts[1])?;
    if end <= start {
        return Err(format!(
            "Invalid TSC range: end ({}) must be greater than start ({})",
            end, start
        ));
    }
    Ok((start, end))
}

fn parse_f64(s: &str) -> Result<f64, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("Invalid number: {}", s))
}

/// Parse `<guest-name>=<host-path>`, splitting on the first `=`.
fn parse_file_arg(s: &str) -> Result<FileArg, String> {
    let (name, path) = s
        .split_once('=')
        .ok_or_else(|| format!("Invalid --file '{}'. Expected <guest-name>=<host-path>", s))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("Empty guest name in --file '{}'", s));
    }
    if path.is_empty() {
        return Err(format!("Empty host path in --file '{}'", s));
    }
    Ok(FileArg {
        name: name.to_string(),
        path: path.to_string(),
    })
}

/// Parse `[tsc=<N>:|vt=<seconds>:]<body>`.
fn parse_scheduled_io_action(s: &str) -> Result<ScheduledIoAction, String> {
    let (target_tsc, body) = if let Some(rest) = s.strip_prefix("tsc=") {
        let (value, body) = rest.split_once(':').ok_or_else(|| {
            format!(
                "Invalid scheduled action '{}'. Expected tsc=<N>:<action>",
                s
            )
        })?;
        (parse_u64(value)?, body)
    } else if let Some(rest) = s.strip_prefix("vt=") {
        let (value, body) = rest.split_once(':').ok_or_else(|| {
            format!(
                "Invalid scheduled action '{}'. Expected vt=<seconds>:<action>",
                s
            )
        })?;
        let secs: f64 = value
            .trim()
            .parse()
            .map_err(|_| format!("Invalid virtual time '{}' in '{}'", value, s))?;
        if secs < 0.0 {
            return Err(format!("Negative virtual time in '{}'", s));
        }
        // `--virt-tsc-frequency` isn't known at parse time; use `tsc=` for
        // precise alignment with a custom frequency.
        let tsc = (secs * DEFAULT_TSC_FREQUENCY as f64) as u64;
        (tsc, body)
    } else {
        (0u64, s)
    };
    let action = parse_io_action_body(body)?;
    Ok(ScheduledIoAction { target_tsc, action })
}

/// Parse `[rec:]exec:<host|container>:<cmd>`. `<cmd>` may contain colons; a
/// container literally named `host` cannot be targeted.
fn parse_io_action_body(s: &str) -> Result<IoAction, String> {
    let (record_output, body) = match s.strip_prefix("rec:") {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let rest = body.strip_prefix("exec:").ok_or_else(|| {
        format!(
            "Invalid I/O action '{}'. Expected [rec:]exec:host:<cmd> or [rec:]exec:<container>:<cmd>",
            s
        )
    })?;
    let (target, cmd) = rest.split_once(':').ok_or_else(|| {
        format!(
            "Invalid exec action '{}'. Expected exec:<host|container>:<cmd>",
            s
        )
    })?;
    if cmd.is_empty() {
        return Err(format!("Empty command in '{}'", s));
    }
    let container = if target == "host" {
        None
    } else {
        if target.is_empty() {
            return Err(format!("Empty container name in '{}'", s));
        }
        Some(target.to_string())
    };
    Ok(IoAction {
        container,
        command: cmd.to_string(),
        record_output,
    })
}
