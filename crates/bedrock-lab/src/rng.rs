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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RandomInput {
    pub at: VirtTime,
    #[cfg_attr(feature = "serde", serde(with = "random_source_serde"))]
    pub source: RandomSource,
    /// The consumer: requesting tgid for GET_RANDOM; 0 for RDRAND/RDSEED
    /// (instruction draws carry no attribution).
    pub pid: u32,
    /// The served buffer, or the RDRAND/RDSEED value's little-endian bytes.
    pub bytes: Vec<u8>,
}

/// Inputs consumed by a branch, suitable for replay. Reconstructed from the
/// branch's event stream (the single source of truth for what reached the
/// guest) via [`record_event`](Self::record_event).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InputRecording {
    random_inputs: Vec<RandomInput>,
    io_inputs: Vec<IoInput>,
}

impl InputRecording {
    pub fn new() -> Self {
        Self::default()
    }

    /// A recording from explicit inputs (e.g. decoded from a [`Tape`](crate::Tape)).
    pub fn from_parts(random_inputs: Vec<RandomInput>, io_inputs: Vec<IoInput>) -> Self {
        Self {
            random_inputs,
            io_inputs,
        }
    }

    /// All randomness served, in consumption order.
    pub fn random_inputs(&self) -> &[RandomInput] {
        &self.random_inputs
    }

    /// I/O actions queued from the branch's [`InputSource`], in queue order.
    pub fn io_inputs(&self) -> &[IoInput] {
        &self.io_inputs
    }

    /// Where to cut this recording before randomness input `n`: the host
    /// actions recorded before that input's time go with the prefix. Past
    /// the end, the cut keeps everything recorded up to the last input.
    pub fn cut_at_index(&self, n: usize) -> Cut {
        let n = n.min(self.random_inputs.len());
        let io = match self.random_inputs.get(n) {
            Some(next) => self.io_inputs.iter().filter(|i| i.at < next.at).count(),
            None => match self.random_inputs.last() {
                Some(last) => self.io_inputs.iter().filter(|i| i.at <= last.at).count(),
                None => 0,
            },
        };
        Cut { random: n, io }
    }

    /// Where to cut this recording at virtual time `t`: every input recorded
    /// before `t` is in the prefix.
    pub fn cut_at_time(&self, t: VirtTime) -> Cut {
        Cut {
            random: self.random_inputs.iter().filter(|r| r.at < t).count(),
            io: self.io_inputs.iter().filter(|i| i.at < t).count(),
        }
    }

    /// The inputs before `cut`.
    pub fn prefix(&self, cut: Cut) -> InputRecording {
        let r = cut.random.min(self.random_inputs.len());
        let i = cut.io.min(self.io_inputs.len());
        Self::from_parts(
            self.random_inputs[..r].to_vec(),
            self.io_inputs[..i].to_vec(),
        )
    }

    /// The inputs from `cut` on.
    pub fn suffix(&self, cut: Cut) -> InputRecording {
        let r = cut.random.min(self.random_inputs.len());
        let i = cut.io.min(self.io_inputs.len());
        Self::from_parts(
            self.random_inputs[r..].to_vec(),
            self.io_inputs[i..].to_vec(),
        )
    }

    /// `self` followed by `rest` (e.g. a tape prefix and a branch's suffix).
    pub fn concat(&self, rest: &InputRecording) -> InputRecording {
        let mut out = self.clone();
        out.random_inputs.extend_from_slice(&rest.random_inputs);
        out.io_inputs.extend_from_slice(&rest.io_inputs);
        out
    }

    /// Whether `prefix` is a prefix of this recording on both streams.
    pub fn starts_with(&self, prefix: &InputRecording) -> bool {
        self.random_inputs.starts_with(&prefix.random_inputs)
            && self.io_inputs.starts_with(&prefix.io_inputs)
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
///
/// Strict: every request must match the next recorded input (same channel
/// and, for GET_RANDOM, the same length). Past the end of the recording or on
/// a mismatch it returns `None`, which the branch surfaces as
/// [`RunOutcome::RngExhausted`](crate::RunOutcome::RngExhausted);
/// [`InputSource::exhaustion`] then says why. It never invents bytes: a
/// replay that needs randomness the tape does not hold has diverged.
#[derive(Debug, Clone)]
pub struct RecordedInputSource {
    recording: InputRecording,
    /// Shared by RDRAND/RDSEED and GET_RANDOM to preserve recorded order.
    random_pos: usize,
    io_pos: usize,
    /// Why the source stopped serving randomness, once it has.
    exhausted: Option<String>,
}

impl RecordedInputSource {
    pub fn new(recording: InputRecording) -> Self {
        Self {
            recording,
            random_pos: 0,
            io_pos: 0,
            exhausted: None,
        }
    }

    pub fn recording(&self) -> &InputRecording {
        &self.recording
    }

    /// Randomness inputs served so far.
    pub fn random_consumed(&self) -> usize {
        self.random_pos
    }

    /// The next recorded randomness input if it matches the request (`len`:
    /// a GET_RANDOM of that many bytes; `None`: an RDRAND/RDSEED value);
    /// otherwise records why not and returns `None`.
    fn take_random(&mut self, want: &str, len: Option<usize>) -> Option<&RandomInput> {
        if self.exhausted.is_some() {
            return None;
        }
        let pos = self.random_pos;
        let total = self.recording.random_inputs.len();
        let Some(input) = self.recording.random_inputs.get(pos) else {
            self.exhausted = Some(format!(
                "input tape exhausted: the guest asked for {want} (randomness input #{pos}) \
                 but the tape holds only {total}"
            ));
            return None;
        };
        let is_get_random = input.source == RandomSource::GetRandom;
        let matches = match len {
            Some(n) => is_get_random && input.bytes.len() == n,
            None => !is_get_random,
        };
        if !matches {
            self.exhausted = Some(format!(
                "input tape diverged at randomness input #{pos} (vt {:.6}s): the guest asked \
                 for {want}, the tape recorded {:?} of {} bytes (pid {})",
                input.at.as_secs_f64(),
                input.source,
                input.bytes.len(),
                input.pid
            ));
            return None;
        }
        self.random_pos += 1;
        Some(input)
    }
}

impl From<InputRecording> for RecordedInputSource {
    fn from(recording: InputRecording) -> Self {
        Self::new(recording)
    }
}

/// Where a [`PrefixSource`] stops serving its recording: the number of
/// randomness inputs and of host actions in the prefix (see
/// [`InputRecording::cut_at_time`] and [`InputRecording::cut_at_index`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Cut {
    pub random: usize,
    pub io: usize,
}

/// Serves a recorded run up to a [`Cut`], then switches to `fallback`: the
/// source of a branch from a moment of a recorded run.
///
/// The prefix is served strictly, like a [`RecordedInputSource`]: a request
/// that does not match the next recorded input stops the source (the branch
/// diverged before the moment). Randomness and host actions switch
/// independently, each when its own prefix runs out. What a branch records
/// is therefore the recording's prefix followed by whatever the fallback
/// served, so the branch replays from its own tape. With the recording's
/// own suffix as fallback (a [`RecordedInputSource`] over
/// [`InputRecording::suffix`]) it serves exactly the recording.
///
/// A branch forked from a checkpoint taken at the cut has consumed the prefix
/// already: build its source with [`after_prefix`](Self::after_prefix).
#[derive(Clone)]
pub struct PrefixSource {
    prefix: RecordedInputSource,
    cut: Cut,
    fallback: Box<dyn InputSource>,
}

impl PrefixSource {
    pub fn new<S: InputSource + 'static>(
        recording: &InputRecording,
        cut: Cut,
        fallback: S,
    ) -> Self {
        let prefix = recording.prefix(cut);
        let cut = Cut {
            random: prefix.random_inputs.len(),
            io: prefix.io_inputs.len(),
        };
        Self {
            prefix: RecordedInputSource::new(prefix),
            cut,
            fallback: Box::new(fallback),
        }
    }

    /// This source with its prefix already consumed: everything comes from
    /// the fallback.
    pub fn after_prefix(mut self) -> Self {
        self.prefix.random_pos = self.cut.random;
        self.prefix.io_pos = self.cut.io;
        self
    }

    pub fn cut(&self) -> Cut {
        self.cut
    }

    /// Whether randomness still comes from the recorded prefix.
    pub fn in_prefix(&self) -> bool {
        self.prefix.random_pos < self.cut.random
    }

    /// Randomness inputs served from the prefix so far.
    pub fn prefix_consumed(&self) -> usize {
        self.prefix.random_pos
    }

    /// Whether the prefix stopped on a mismatched request.
    fn prefix_failed(&self) -> bool {
        self.prefix.exhausted.is_some()
    }
}

impl std::fmt::Debug for PrefixSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixSource")
            .field("cut", &self.cut)
            .field("random_pos", &self.prefix.random_pos)
            .field("io_pos", &self.prefix.io_pos)
            .finish_non_exhaustive()
    }
}

impl InputSource for PrefixSource {
    fn next_rng_u64(&mut self) -> Option<u64> {
        if self.in_prefix() || self.prefix_failed() {
            return self.prefix.next_rng_u64();
        }
        self.fallback.next_rng_u64()
    }

    fn next_random(&mut self, len: usize, pid: u32) -> Option<Vec<u8>> {
        if self.in_prefix() || self.prefix_failed() {
            return self.prefix.next_random(len, pid);
        }
        self.fallback.next_random(len, pid)
    }

    fn exhaustion(&self) -> Option<String> {
        match self.prefix.exhaustion() {
            Some(why) => Some(format!("in the recorded prefix: {why}")),
            None => self.fallback.exhaustion(),
        }
    }

    fn next_io_input(&mut self) -> Option<IoInput> {
        if self.prefix.io_pos < self.cut.io {
            return self.prefix.next_io_input();
        }
        self.fallback.next_io_input()
    }

    fn clone_box(&self) -> Box<dyn InputSource> {
        Box::new(self.clone())
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

    /// Serve one `HYPERCALL_GET_RANDOM` request for consumer `pid` (the
    /// requesting tgid): exactly `len` bytes, or `None` on exhaustion (as for
    /// [`next_rng_u64`](Self::next_rng_u64)). Defaults to synthesizing from
    /// `next_rng_u64`.
    fn next_random(&mut self, len: usize, _pid: u32) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            out.extend_from_slice(&self.next_rng_u64()?.to_le_bytes());
        }
        out.truncate(len);
        Some(out)
    }

    /// Why the source stopped serving randomness, once it has; `None` while
    /// it still serves or when it has no explanation.
    fn exhaustion(&self) -> Option<String> {
        None
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

/// A boxed source is a source (e.g. a [`PrefixSource`] fallback picked at
/// run time).
impl InputSource for Box<dyn InputSource> {
    fn next_rng_u64(&mut self) -> Option<u64> {
        (**self).next_rng_u64()
    }
    fn next_random(&mut self, len: usize, pid: u32) -> Option<Vec<u8>> {
        (**self).next_random(len, pid)
    }
    fn exhaustion(&self) -> Option<String> {
        (**self).exhaustion()
    }
    fn next_io_input(&mut self) -> Option<IoInput> {
        (**self).next_io_input()
    }
    fn clone_box(&self) -> Box<dyn InputSource> {
        (**self).clone_box()
    }
}

impl InputSource for RecordedInputSource {
    fn next_rng_u64(&mut self) -> Option<u64> {
        let input = self.take_random("an RDRAND/RDSEED value", None)?;
        let mut buf = [0u8; 8];
        let n = input.bytes.len().min(8);
        buf[..n].copy_from_slice(&input.bytes[..n]);
        Some(u64::from_le_bytes(buf))
    }

    fn next_random(&mut self, len: usize, pid: u32) -> Option<Vec<u8>> {
        let want = format!("GetRandom of {len} bytes (pid {pid})");
        Some(self.take_random(&want, Some(len))?.bytes.clone())
    }

    fn exhaustion(&self) -> Option<String> {
        self.exhausted.clone()
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

/// Stable name of a [`RandomSource`] in tapes and JSON.
#[cfg(feature = "serde")]
fn random_source_name(s: RandomSource) -> &'static str {
    match s {
        RandomSource::Rdrand => "rdrand",
        RandomSource::Rdseed => "rdseed",
        RandomSource::GetRandom => "getrandom",
    }
}

#[cfg(feature = "serde")]
fn random_source_from_name(name: &str) -> Option<RandomSource> {
    Some(match name {
        "rdrand" => RandomSource::Rdrand,
        "rdseed" => RandomSource::Rdseed,
        "getrandom" => RandomSource::GetRandom,
        _ => return None,
    })
}

/// Serde for [`RandomSource`] (a `bedrock-vm` type) by its stable name.
#[cfg(feature = "serde")]
mod random_source_serde {
    use bedrock_vm::events::RandomSource;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(s: &RandomSource, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(super::random_source_name(*s))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<RandomSource, D::Error> {
        let name = String::deserialize(de)?;
        super::random_source_from_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown random source {name:?}")))
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

/// The hypervisor's seeded stream ([`RngMode::Seeded`] /
/// [`Branch::reseed_rng`](crate::Branch::reseed_rng)) served from userspace:
/// the same xorshift64 values, the same GET_RANDOM fill (one value per 8
/// bytes, the last truncated), so a branch sees the bytes it would see
/// in-kernel.
///
/// What differs is how they are delivered: every draw exits to userspace,
/// exactly as a [`RecordedInputSource`] replay does. In-kernel serving
/// completes the instruction in one exit, while exit-to-userspace re-executes
/// it after the exit, so an interrupt pending at that exit is taken before
/// the draw instead of after it. A branch recorded with this source therefore
/// replays from its tape exactly; one recorded in-kernel may not.
#[derive(Debug, Clone)]
pub struct SeededSource {
    state: u64,
}

impl SeededSource {
    pub fn new(seed: u64) -> Self {
        // xorshift64 never leaves 0; the kernel substitutes 1 as well.
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    /// Skip `n` stream values (what [`draws`](Self::draws) says a run
    /// consumed), to continue a recorded run's stream.
    pub fn skip(mut self, n: u64) -> Self {
        for _ in 0..n {
            self.next();
        }
        self
    }

    /// Stream values `inputs` took from this source: one per RDRAND/RDSEED,
    /// one per started 8 bytes of a GET_RANDOM.
    pub fn draws(inputs: &[RandomInput]) -> u64 {
        inputs
            .iter()
            .map(|r| match r.source {
                RandomSource::GetRandom => r.bytes.len().div_ceil(8) as u64,
                _ => 1,
            })
            .sum()
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

impl InputSource for SeededSource {
    fn next_rng_u64(&mut self) -> Option<u64> {
        Some(self.next())
    }

    fn next_random(&mut self, len: usize, _pid: u32) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let n = (len - out.len()).min(8);
            out.extend_from_slice(&self.next().to_le_bytes()[..n]);
        }
        Some(out)
    }

    fn clone_box(&self) -> Box<dyn InputSource> {
        Box::new(self.clone())
    }
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
