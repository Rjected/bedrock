// SPDX-License-Identifier: GPL-2.0

//! RDRAND/RDSEED modes and userspace input sources for a lab tree.
//!
//! The mode is picked once per tree (via [`LabOpts`]). `Seeded` and `Inherit`
//! run entirely in the hypervisor; [`RngMode::Source`] exits to a userspace
//! [`InputSource`] (e.g. [`SystemRng`] or a fuzzer-driven closure) per value.

use std::fs::File;
use std::io::Read;

use bedrock_vm::events::{IoChannelPhase, RandomSource};
use bedrock_vm::io_channel::{decode_request, IoTarget};
use bedrock_vm::{Event, EventRecord};

use crate::bash::BashTarget;
use crate::time::VirtTime;

/// One host-driven I/O action supplied by an [`InputSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoInput {
    pub at: VirtTime,
    pub target: BashTarget,
    pub command: String,
    /// Capture output into [`BashOutput`](crate::BashOutput).
    pub record_output: bool,
}

/// One randomness value served to the guest via RDRAND, RDSEED or
/// `HYPERCALL_GET_RANDOM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RandomInput {
    pub at: VirtTime,
    pub source: RandomSource,
    /// Requesting tgid for GET_RANDOM; 0 for RDRAND/RDSEED.
    pub pid: u32,
    /// The served buffer, or the RDRAND/RDSEED value's little-endian bytes.
    pub bytes: Vec<u8>,
}

/// Inputs consumed by a branch, suitable for replay. Reconstructed from the
/// branch's event stream (the single source of truth for what reached the
/// guest) via [`record_event`](Self::record_event).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputRecording {
    random_inputs: Vec<RandomInput>,
    io_inputs: Vec<IoInput>,
}

impl InputRecording {
    pub fn new() -> Self {
        Self::default()
    }

    /// All randomness served, in consumption order.
    pub fn random_inputs(&self) -> &[RandomInput] {
        &self.random_inputs
    }

    /// I/O actions queued from the branch's [`InputSource`], in queue order.
    pub fn io_inputs(&self) -> &[IoInput] {
        &self.io_inputs
    }

    /// Append the input carried by one record: `Randomness` yields a
    /// [`RandomInput`], an `IoChannel` *request* an [`IoInput`]; all else
    /// (including I/O responses) is ignored.
    ///
    /// I/O inputs use the record's emit TSC, not the request's `target_tsc`:
    /// source-driven requests are queued with target 0, so the emit TSC is where
    /// a replay must stop to re-inject the command.
    pub(crate) fn record_event(&mut self, record: &EventRecord<'_>, freq: u64) {
        match record.event() {
            Event::Randomness(p, bytes) => {
                let at = VirtTime::from_instructions(record.tsc(), freq);
                let source = RandomSource::from_u8(p.source);
                let (pid, bytes) = if source == RandomSource::GetRandom {
                    (p.pid, bytes.to_vec())
                } else {
                    (0, p.value.to_le_bytes().to_vec())
                };
                self.random_inputs.push(RandomInput {
                    at,
                    source,
                    pid,
                    bytes,
                });
            }
            Event::IoChannel(meta, data) if meta.phase == IoChannelPhase::Request as u8 => {
                let Some(req) = decode_request(data) else {
                    return;
                };
                self.io_inputs.push(IoInput {
                    at: VirtTime::from_instructions(record.tsc(), freq),
                    target: match req.target {
                        IoTarget::Host => BashTarget::Host,
                        IoTarget::Container(name) => BashTarget::Container(name.into_owned()),
                    },
                    command: req.command.into_owned(),
                    record_output: req.record_output,
                });
            }
            _ => {}
        }
    }
}

/// Replay source backed by an [`InputRecording`].
#[derive(Debug, Clone)]
pub struct RecordedInputSource {
    recording: InputRecording,
    /// Shared by RDRAND/RDSEED and GET_RANDOM to preserve recorded order.
    random_pos: usize,
    io_pos: usize,
}

impl RecordedInputSource {
    pub fn new(recording: InputRecording) -> Self {
        Self {
            recording,
            random_pos: 0,
            io_pos: 0,
        }
    }

    pub fn recording(&self) -> &InputRecording {
        &self.recording
    }
}

impl From<InputRecording> for RecordedInputSource {
    fn from(recording: InputRecording) -> Self {
        Self::new(recording)
    }
}

/// A userspace source of lab inputs: RNG values (with [`RngMode::Source`]) and
/// host-driven I/O actions.
///
/// Every checkpoint captures the source state and every branch gets its own
/// clone, so sibling branches' input streams are independent of drive order.
/// Closure sources therefore need `Clone` captured state.
pub trait InputSource: Send + Sync {
    /// `None` signals exhaustion, surfaced as
    /// [`RunOutcome::RngExhausted`](crate::RunOutcome::RngExhausted); keep
    /// returning `None` afterwards.
    fn next_rng_u64(&mut self) -> Option<u64>;

    /// Serve one `HYPERCALL_GET_RANDOM` request; must return exactly `len`
    /// bytes. Defaults to synthesizing from [`next_rng_u64`](Self::next_rng_u64).
    fn next_random(&mut self, len: usize, _pid: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let v = self.next_rng_u64().unwrap_or(0);
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn next_io_input(&mut self) -> Option<IoInput> {
        None
    }

    /// An independent copy: pulls on either must not affect the other.
    fn clone_box(&self) -> Box<dyn InputSource>;
}

impl<F: FnMut() -> Option<u64> + Send + Sync + Clone + 'static> InputSource for F {
    fn next_rng_u64(&mut self) -> Option<u64> {
        self()
    }
    fn clone_box(&self) -> Box<dyn InputSource> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn InputSource> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl InputSource for RecordedInputSource {
    fn next_rng_u64(&mut self) -> Option<u64> {
        let input = self.recording.random_inputs.get(self.random_pos)?;
        self.random_pos += 1;
        let mut buf = [0u8; 8];
        let n = input.bytes.len().min(8);
        buf[..n].copy_from_slice(&input.bytes[..n]);
        Some(u64::from_le_bytes(buf))
    }

    fn next_random(&mut self, len: usize, _pid: u32) -> Vec<u8> {
        // Zero-fill past the recording or a shorter recorded reply.
        let mut bytes = self
            .recording
            .random_inputs
            .get(self.random_pos)
            .map(|input| input.bytes.clone())
            .unwrap_or_default();
        self.random_pos += 1;
        bytes.resize(len, 0);
        bytes
    }

    fn next_io_input(&mut self) -> Option<IoInput> {
        let input = self.recording.io_inputs.get(self.io_pos).cloned();
        if input.is_some() {
            self.io_pos += 1;
        }
        input
    }

    fn clone_box(&self) -> Box<dyn InputSource> {
        Box::new(self.clone())
    }
}

/// How a tree serves guest `RDRAND`/`RDSEED`. Set once via
/// [`LabOpts::rng`](crate::LabOpts::rng) and inherited by every fork.
#[derive(Default)]
pub enum RngMode {
    /// Keep whatever RDRAND config the caller set on the [`Vm`](bedrock_vm::Vm).
    #[default]
    Inherit,
    /// Kernel-side `xorshift64` PRNG; its state forks with each branch.
    Seeded(u64),
    /// Exit to userspace and serve from this source (cloned per branch).
    Source(Box<dyn InputSource>),
}

/// A non-deterministic [`InputSource`] backed by `/dev/urandom`.
pub struct SystemRng {
    file: File,
}

impl SystemRng {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            file: File::open("/dev/urandom")?,
        })
    }
}

impl InputSource for SystemRng {
    fn next_rng_u64(&mut self) -> Option<u64> {
        let mut buf = [0u8; 8];
        // /dev/urandom doesn't short-read for 8 bytes.
        let _ = self.file.read_exact(&mut buf);
        Some(u64::from_le_bytes(buf))
    }
    fn clone_box(&self) -> Box<dyn InputSource> {
        Box::new(Self::new().expect("/dev/urandom"))
    }
}

#[cfg(test)]
#[path = "rng_tests.rs"]
mod tests;
