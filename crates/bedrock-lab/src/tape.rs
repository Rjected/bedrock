// SPDX-License-Identifier: GPL-2.0

//! Input tapes: an [`InputRecording`] plus the window it covers, in a
//! versioned binary container, so a branch's inputs can be stored and later
//! replayed with a [`RecordedInputSource`](crate::RecordedInputSource).
//!
//! # Format (version 1)
//!
//! All integers little-endian.
//!
//! ```text
//! magic        8  b"BDRKTAPE"
//! version      u32 (1)
//! flags        u32 (0; reserved)
//! body_len     u64  bytes of body that follow
//! body:
//!   tsc_frequency u64
//!   start, end    u64, u64   retired-instruction counts (VirtTime)
//!   n_random      u64, then per input:
//!     at u64, source u8 (0 rdrand, 1 rdseed, 2 getrandom), pid u32,
//!     len u32, bytes[len]
//!   n_io          u64, then per input:
//!     at u64, target u8 (0 host, 1 container), [name: u32 len, bytes],
//!     command: u32 len, bytes (UTF-8), record_output u8
//! checksum     u64  FNV-1a 64 of body
//! ```
//!
//! A reader rejects another magic, a newer version, a truncated or trailing
//! body and a checksum mismatch.

use std::fmt;

use bedrock_vm::events::RandomSource;

use crate::bash::BashTarget;
use crate::rng::{InputRecording, IoInput, RandomInput};
use crate::time::VirtTime;

pub const TAPE_MAGIC: [u8; 8] = *b"BDRKTAPE";
pub const TAPE_VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 + 4 + 8;

/// A branch's recorded inputs over virtual time `[start, end]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tape {
    pub tsc_frequency: u64,
    /// Where the branch started (its origin checkpoint's time).
    pub start: VirtTime,
    /// Where the recording stopped (the branch's time when it was taken).
    pub end: VirtTime,
    pub recording: InputRecording,
}

/// Why a tape could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapeError {
    BadMagic,
    UnsupportedVersion(u32),
    Truncated,
    TrailingBytes(usize),
    Checksum { stored: u64, computed: u64 },
    BadField(&'static str),
}

impl fmt::Display for TapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TapeError::BadMagic => write!(f, "not an input tape (bad magic)"),
            TapeError::UnsupportedVersion(v) => write!(
                f,
                "input tape version {v} is newer than this reader ({TAPE_VERSION})"
            ),
            TapeError::Truncated => write!(f, "input tape is truncated"),
            TapeError::TrailingBytes(n) => write!(f, "input tape has {n} trailing bytes"),
            TapeError::Checksum { stored, computed } => write!(
                f,
                "input tape checksum mismatch (stored {stored:#x}, computed {computed:#x})"
            ),
            TapeError::BadField(what) => write!(f, "input tape has an invalid {what}"),
        }
    }
}

impl std::error::Error for TapeError {}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn source_byte(s: RandomSource) -> u8 {
    s as u8
}

fn source_from_byte(b: u8) -> Result<RandomSource, TapeError> {
    match b {
        0 => Ok(RandomSource::Rdrand),
        1 => Ok(RandomSource::Rdseed),
        2 => Ok(RandomSource::GetRandom),
        _ => Err(TapeError::BadField("randomness source")),
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(u32::try_from(b.len()).expect("tape field over 4 GiB"));
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], TapeError> {
        if self.0.len() < n {
            return Err(TapeError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, TapeError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, TapeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, TapeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8], TapeError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn string(&mut self, what: &'static str) -> Result<String, TapeError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| TapeError::BadField(what))
    }
    /// A count of items at least `min_item` bytes each: bounded by what is
    /// left, so a corrupt count cannot trigger a huge allocation.
    fn count(&mut self, min_item: usize) -> Result<usize, TapeError> {
        let n = self.u64()?;
        if n > (self.0.len() / min_item) as u64 {
            return Err(TapeError::Truncated);
        }
        Ok(n as usize)
    }
}

impl Tape {
    /// Encode as a version-[`TAPE_VERSION`] tape.
    pub fn to_bytes(&self) -> Vec<u8> {
        let freq = self.tsc_frequency;
        let mut w = Writer(Vec::new());
        w.u64(freq);
        w.u64(self.start.instructions());
        w.u64(self.end.instructions());
        let random = self.recording.random_inputs();
        w.u64(random.len() as u64);
        for r in random {
            w.u64(r.at.instructions());
            w.u8(source_byte(r.source));
            w.u32(r.pid);
            w.bytes(&r.bytes);
        }
        let io = self.recording.io_inputs();
        w.u64(io.len() as u64);
        for i in io {
            w.u64(i.at.instructions());
            match &i.target {
                BashTarget::Host => w.u8(0),
                BashTarget::Container(name) => {
                    w.u8(1);
                    w.bytes(name.as_bytes());
                }
            }
            w.bytes(i.command.as_bytes());
            w.u8(u8::from(i.record_output));
        }
        let body = w.0;
        let mut out = Vec::with_capacity(HEADER_LEN + body.len() + 8);
        out.extend_from_slice(&TAPE_MAGIC);
        out.extend_from_slice(&TAPE_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(body.len() as u64).to_le_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&fnv1a64(&body).to_le_bytes());
        out
    }

    /// Decode a tape written by [`to_bytes`](Self::to_bytes).
    pub fn from_bytes(data: &[u8]) -> Result<Self, TapeError> {
        let mut r = Reader(data);
        if r.take(8).map_err(|_| TapeError::BadMagic)? != TAPE_MAGIC {
            return Err(TapeError::BadMagic);
        }
        let version = r.u32()?;
        if version != TAPE_VERSION {
            return Err(TapeError::UnsupportedVersion(version));
        }
        let _flags = r.u32()?;
        let body_len = usize::try_from(r.u64()?).map_err(|_| TapeError::Truncated)?;
        let body = r.take(body_len)?;
        let stored = r.u64()?;
        if !r.0.is_empty() {
            return Err(TapeError::TrailingBytes(r.0.len()));
        }
        let computed = fnv1a64(body);
        if stored != computed {
            return Err(TapeError::Checksum { stored, computed });
        }

        let mut b = Reader(body);
        let freq = b.u64()?;
        if freq == 0 {
            return Err(TapeError::BadField("TSC frequency"));
        }
        let at = |v| VirtTime::from_instructions(v, freq);
        let start = at(b.u64()?);
        let end = at(b.u64()?);
        let n = b.count(8 + 1 + 4 + 4)?;
        let mut random = Vec::with_capacity(n);
        for _ in 0..n {
            let t = at(b.u64()?);
            let source = source_from_byte(b.u8()?)?;
            let pid = b.u32()?;
            let bytes = b.bytes()?.to_vec();
            random.push(RandomInput {
                at: t,
                source,
                pid,
                bytes,
            });
        }
        let n = b.count(8 + 1 + 4 + 1)?;
        let mut io = Vec::with_capacity(n);
        for _ in 0..n {
            let t = at(b.u64()?);
            let target = match b.u8()? {
                0 => BashTarget::Host,
                1 => BashTarget::Container(b.string("container name")?),
                _ => return Err(TapeError::BadField("I/O target")),
            };
            let command = b.string("command")?;
            let record_output = match b.u8()? {
                0 => false,
                1 => true,
                _ => return Err(TapeError::BadField("record_output")),
            };
            io.push(IoInput {
                at: t,
                target,
                command,
                record_output,
            });
        }
        if !b.0.is_empty() {
            return Err(TapeError::TrailingBytes(b.0.len()));
        }
        Ok(Tape {
            tsc_frequency: freq,
            start,
            end,
            recording: InputRecording::from_parts(random, io),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREQ: u64 = 2_995_200_000;

    fn sample() -> Tape {
        let t = |v| VirtTime::from_instructions(v, FREQ);
        Tape {
            tsc_frequency: FREQ,
            start: t(1_000),
            end: t(9_000),
            recording: InputRecording::from_parts(
                vec![
                    RandomInput {
                        at: t(1_500),
                        source: RandomSource::Rdrand,
                        pid: 0,
                        bytes: 0xdead_beef_u64.to_le_bytes().to_vec(),
                    },
                    RandomInput {
                        at: t(2_000),
                        source: RandomSource::GetRandom,
                        pid: 4242,
                        bytes: vec![7; 256],
                    },
                    RandomInput {
                        at: t(2_500),
                        source: RandomSource::Rdseed,
                        pid: 0,
                        bytes: vec![1, 2, 3, 4, 5, 6, 7, 8],
                    },
                ],
                vec![
                    IoInput {
                        at: t(3_000),
                        target: BashTarget::Host,
                        command: "tempo-dst start '{\"seed\":1}'".into(),
                        record_output: true,
                    },
                    IoInput {
                        at: t(4_000),
                        target: BashTarget::container("tempo"),
                        command: "true".into(),
                        record_output: false,
                    },
                ],
            ),
        }
    }

    #[test]
    fn binary_round_trip() {
        let tape = sample();
        let bytes = tape.to_bytes();
        assert_eq!(&bytes[..8], b"BDRKTAPE");
        assert_eq!(Tape::from_bytes(&bytes).unwrap(), tape);
        // An empty tape round-trips too.
        let empty = Tape {
            recording: InputRecording::new(),
            ..sample()
        };
        assert_eq!(Tape::from_bytes(&empty.to_bytes()).unwrap(), empty);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_json_round_trip() {
        let tape = sample();
        let json = serde_json::to_string(&tape).unwrap();
        assert!(json.contains("\"getrandom\""), "{json}");
        let back: Tape = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tape);
    }

    #[test]
    fn corrupt_tapes_are_rejected() {
        let bytes = sample().to_bytes();
        assert_eq!(Tape::from_bytes(b"NOTATAPE"), Err(TapeError::BadMagic));
        assert_eq!(Tape::from_bytes(&bytes[..4]), Err(TapeError::BadMagic));
        let mut v2 = bytes.clone();
        v2[8] = 2;
        assert_eq!(Tape::from_bytes(&v2), Err(TapeError::UnsupportedVersion(2)));
        assert_eq!(
            Tape::from_bytes(&bytes[..bytes.len() - 1]),
            Err(TapeError::Truncated)
        );
        let mut flipped = bytes.clone();
        flipped[HEADER_LEN + 30] ^= 1;
        assert!(matches!(
            Tape::from_bytes(&flipped),
            Err(TapeError::Checksum { .. })
        ));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            Tape::from_bytes(&trailing),
            Err(TapeError::TrailingBytes(1))
        );
    }
}
