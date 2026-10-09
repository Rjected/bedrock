// SPDX-License-Identifier: GPL-2.0

//! Error types for `bedrock-lab`.

use std::fmt;

use bedrock_vm::ExitKind;

use crate::{BashTarget, VirtTime};

/// Errors returned by lab operations.
#[derive(Debug)]
pub enum LabError {
    /// The underlying VM operation failed.
    Vm(bedrock_vm::VmError),

    /// `run_until` target before the current time; use
    /// [`Checkpoint::rewind`](crate::Checkpoint::rewind) to go back.
    TargetInPast { current: VirtTime, target: VirtTime },

    /// No ancestor checkpoint at or before the rewind target.
    NoCheckpointBefore { target: VirtTime },

    /// Two times were combined with mismatched TSC frequencies.
    FrequencyMismatch { lhs: u64, rhs: u64 },

    /// An exit the lab couldn't handle while waiting (e.g. guest shut down).
    UnexpectedExit { at: VirtTime, kind: ExitKind },

    /// Queueing an [`InputSource`](crate::InputSource) I/O action failed.
    QueueInputIo {
        at: VirtTime,
        target: BashTarget,
        command: String,
        source: std::io::Error,
    },

    /// The I/O channel returned bytes the lab couldn't decode.
    BadResponse(String),

    /// The [`InputSource`](crate::InputSource) ran out of (or diverged from)
    /// its randomness while an I/O action was running; `reason` is its
    /// [`exhaustion`](crate::InputSource::exhaustion) explanation.
    InputExhausted { at: VirtTime, reason: String },

    /// The lab could not store a file chunk from the guest.
    FileStoreFailed {
        at: VirtTime,
        source: std::io::Error,
    },
}

impl fmt::Display for LabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vm(e) => write!(f, "vm error: {e}"),
            Self::TargetInPast { current, target } => write!(
                f,
                "run_until target {target:?} is before current time {current:?}; use Checkpoint::rewind to move backward"
            ),
            Self::NoCheckpointBefore { target } => write!(
                f,
                "no ancestor checkpoint at or before {target:?}"
            ),
            Self::FrequencyMismatch { lhs, rhs } => {
                write!(f, "TSC frequency mismatch: {lhs} vs {rhs}")
            }
            Self::UnexpectedExit { at, kind } => write!(
                f,
                "unexpected exit while waiting for I/O response at {at:?}: {kind:?}"
            ),
            Self::QueueInputIo {
                at,
                target,
                command,
                source,
            } => write!(
                f,
                "failed to queue InputSource I/O at {at:?} for {target:?} command {command:?}: {source}"
            ),
            Self::BadResponse(msg) => write!(f, "bad I/O channel response: {msg}"),
            Self::InputExhausted { at, reason } => {
                write!(f, "input source exhausted at {:.6}s: {reason}", at.as_secs_f64())
            }
            Self::FileStoreFailed { at, source } => write!(
                f,
                "storing a file chunk from the guest failed at {at:?}: {source:?}"
            ),
        }
    }
}

impl std::error::Error for LabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::QueueInputIo { source, .. } => Some(source),
            Self::Vm(e) => Some(e),
            _ => None,
        }
    }
}

impl From<bedrock_vm::VmError> for LabError {
    fn from(e: bedrock_vm::VmError) -> Self {
        Self::Vm(e)
    }
}

impl From<std::io::Error> for LabError {
    fn from(e: std::io::Error) -> Self {
        Self::Vm(bedrock_vm::VmError::Io(e))
    }
}

pub type Result<T> = std::result::Result<T, LabError>;
