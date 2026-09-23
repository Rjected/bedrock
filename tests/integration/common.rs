//! Shared harness for the bedrock integration tests.
//!
//! The guest is booted to ready once per process ([`ready_checkpoint`] caches
//! the [`Checkpoint`] in a `OnceLock`); each test forks its own CoW branch, so
//! tests run in parallel without interfering.
//!
//! # Running
//!
//! Requires the bedrock module loaded plus the guest images and the workload
//! files the podman initrd fetches at boot:
//!
//! ```text
//! BEDROCK_VMLINUX=/path/to/vmlinux \
//! BEDROCK_INITRAMFS=/path/to/podman-initrd \
//! BEDROCK_COMPOSE=/path/to/workloads/integration-tests/compose.yaml \
//! BEDROCK_IMAGES=/path/to/workloads/integration-tests/images.tar \
//!     cargo test -p bedrock-integration-tests
//! ```
//!
//! The `integration-tests` nix app wires these up. Without them (e.g. plain
//! `just test`), each test skips, printing what is missing.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use bedrock_lab::{BashTarget, Branch, BranchId, Checkpoint, Event, EventSink, LabOpts, RngMode};
use bedrock_vm::{boot::defaults, load_kernel, LinuxBootConfig, VmBuilder};

/// Guest RAM; matches the Nix integration test (`-m 5120`).
const MEMORY_MB: usize = 5120;

/// Constant RDRAND/RDSEED seed, so every branch forks the same RNG state.
const BOOT_RNG_SEED: u64 = 0xbed0_0001;

static READY: OnceLock<Checkpoint> = OnceLock::new();
static SINK: OnceLock<Arc<CaptureSink>> = OnceLock::new();

/// Tree-wide [`EventSink`] retaining each branch's event records and serial
/// lines, bucketed by [`BranchId`] (one sink serves all parallel tests).
#[derive(Default)]
pub struct CaptureSink {
    records: Mutex<HashMap<BranchId, Vec<serde_json::Value>>>,
    serial: Mutex<HashMap<BranchId, Vec<String>>>,
}

impl CaptureSink {
    /// Drain `branch`'s deterministic records with host-timing fields stripped,
    /// so sibling branches doing identical work compare equal. Keep in sync
    /// with `contrib/determ-divergence.py`: `seq` is stripped because it also
    /// counts non-deterministic events.
    pub fn take_deterministic(&self, branch: BranchId) -> Vec<serde_json::Value> {
        /// See `DIAGNOSTIC_FIELDS` in `contrib/determ-divergence.py`.
        const PEBS_DIAGNOSTIC_FIELDS: &[&str] = &[
            "pebs_skid",
            "pebs_inst_delta",
            "pebs_tsc_offset_delta",
            "pebs_iters_since_arm",
            "pebs_arm_delta",
        ];
        let raw = self
            .records
            .lock()
            .unwrap()
            .remove(&branch)
            .unwrap_or_default();
        raw.into_iter()
            .filter(|r| r.get("deterministic").and_then(|d| d.as_bool()) == Some(true))
            .map(|mut r| {
                if let Some(obj) = r.as_object_mut() {
                    obj.remove("real_tsc");
                    obj.remove("seq");
                    if let Some(data) = obj.get_mut("data").and_then(|d| d.as_object_mut()) {
                        for field in PEBS_DIAGNOSTIC_FIELDS {
                            data.remove(*field);
                        }
                    }
                }
                r
            })
            .collect()
    }

    /// Non-draining snapshot of `branch`'s serial lines so far.
    pub fn serial_lines(&self, branch: BranchId) -> Vec<String> {
        self.serial
            .lock()
            .unwrap()
            .get(&branch)
            .cloned()
            .unwrap_or_default()
    }
}

impl EventSink for CaptureSink {
    fn on_event(&self, event: Event<'_>) {
        match event {
            // The borrowed record can't outlive this call; serialize it now.
            Event::Record { branch, record } => {
                if let Ok(value) = serde_json::to_value(record.to_json()) {
                    self.records
                        .lock()
                        .unwrap()
                        .entry(branch)
                        .or_default()
                        .push(value);
                }
            }
            Event::SerialLine { branch, line, .. } => {
                self.serial
                    .lock()
                    .unwrap()
                    .entry(branch)
                    .or_default()
                    .push(String::from_utf8_lossy(line).into_owned());
            }
            _ => {}
        }
    }
}

pub fn capture_sink() -> Arc<CaptureSink> {
    SINK.get_or_init(|| Arc::new(CaptureSink::default()))
        .clone()
}

struct GuestEnv {
    vmlinux: String,
    initramfs: String,
    /// Host paths of the workload files served to the guest at boot.
    compose: String,
    images: String,
}

fn can_run() -> Option<GuestEnv> {
    if !Path::new(bedrock_vm::BEDROCK_DEVICE_PATH).exists() {
        return None;
    }
    Some(GuestEnv {
        vmlinux: std::env::var("BEDROCK_VMLINUX").ok()?,
        initramfs: std::env::var("BEDROCK_INITRAMFS").ok()?,
        compose: std::env::var("BEDROCK_COMPOSE").ok()?,
        images: std::env::var("BEDROCK_IMAGES").ok()?,
    })
}

/// The shared ready checkpoint, booted on first use; `None` means skip:
///
/// ```ignore
/// let Some(ready) = common::ready_checkpoint() else {
///     return common::skip("boot to ready");
/// };
/// ```
pub fn ready_checkpoint() -> Option<Checkpoint> {
    let env = can_run()?;
    // get_or_init blocks concurrent first-callers, so the guest boots once.
    let cp = READY.get_or_init(|| boot_ready(&env).expect("boot guest to ready checkpoint"));
    Some(cp.clone())
}

/// Print a skip notice naming exactly what is missing, so a partial setup
/// doesn't look like a silent pass.
pub fn skip(what: &str) {
    let mut missing: Vec<String> = Vec::new();
    if !Path::new(bedrock_vm::BEDROCK_DEVICE_PATH).exists() {
        missing.push(format!(
            "the bedrock module loaded ({} absent)",
            bedrock_vm::BEDROCK_DEVICE_PATH
        ));
    }
    for var in [
        "BEDROCK_VMLINUX",
        "BEDROCK_INITRAMFS",
        "BEDROCK_COMPOSE",
        "BEDROCK_IMAGES",
    ] {
        if std::env::var(var).is_err() {
            missing.push(var.to_string());
        }
    }
    eprintln!("SKIP {what}: needs {}", missing.join(", "));
}

fn boot_ready(env: &GuestEnv) -> Result<Checkpoint, Box<dyn std::error::Error>> {
    let mut vm = VmBuilder::new().memory_mb(MEMORY_MB).build()?;
    let kernel = std::fs::read(&env.vmlinux)?;
    let initrd = std::fs::read(&env.initramfs)?;

    let (kernel_entry, kernel_end) = {
        let memory = vm.memory_mut()?;
        load_kernel(memory, &kernel)?
    };

    let boot = LinuxBootConfig::new(kernel_entry, kernel_end)
        .cmdline(defaults::CMDLINE)
        .initramfs(&initrd);
    vm.setup_linux_boot(&boot)?;

    // Generous: the podman initrd has to load images and start the container.
    let deadline = vt!(120 s);
    let cp = Checkpoint::initial_when_ready_with(
        vm,
        deadline,
        LabOpts {
            sink: capture_sink(),
            rng: RngMode::Seeded(BOOT_RNG_SEED),
            files: vec![
                ("compose.yaml".to_string(), env.compose.clone()),
                ("images.tar".to_string(), env.images.clone()),
            ],
            ..Default::default()
        },
    )?;
    Ok(cp)
}

/// Hex sha256 of a host file via `sha256sum`; panics on failure.
pub fn host_sha256(path: &str) -> String {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("run sha256sum {path} on host: {e}"));
    assert!(out.status.success(), "host sha256sum {path} failed");
    let stdout = String::from_utf8(out.stdout).expect("sha256sum utf8");
    first_token(&stdout)
}

/// Hex sha256 of a guest file via bash; fails the test if it doesn't exist.
pub fn guest_sha256(branch: &mut Branch, path: &str) -> String {
    let out = branch
        .bash(BashTarget::host(), &format!("sha256sum {path}"), true)
        .expect("dispatch sha256sum in guest");
    assert!(
        out.success(),
        "guest `sha256sum {path}` failed (status={} exit={}) — file missing? output: {:?}",
        out.status,
        out.exit_code,
        out.output_lossy(),
    );

    first_token(&out.output_lossy())
}

/// The first whitespace-delimited token — `sha256sum` prints `<hex>  <path>`.
pub fn first_token(s: &str) -> String {
    s.split_whitespace().next().unwrap_or_default().to_string()
}
