// SPDX-License-Identifier: GPL-2.0

//! Event sink: how consumers observe the tree. Each tree owns one [`EventSink`].

use bedrock_vm::events::EventKind;
use bedrock_vm::{EventRecord, EventStream, Vm};

use crate::branch::BranchId;
use crate::checkpoint::CheckpointId;
use crate::error::Result;
use crate::time::VirtTime;

/// An observable event in the lab's execution tree. Borrowed fields are only
/// valid during `on_event`; copy them out to retain them.
/// [`BranchId(0)`](crate::BranchId) is reserved for pre-ready root boot.
#[non_exhaustive]
#[derive(Debug)]
pub enum Event<'a> {
    /// One complete serial line (`\n` stripped). `at` is the TSC of its first
    /// byte, preserved across drains and checkpoints. Partial lines are
    /// carried into checkpoints but dropped with a branch.
    SerialLine {
        branch: BranchId,
        at: VirtTime,
        line: &'a [u8],
    },
    /// A new branch was forked from `origin`.
    BranchCreated {
        branch: BranchId,
        origin: CheckpointId,
        at: VirtTime,
    },
    /// `from_branch` and `parent` are `None` for the root.
    CheckpointCreated {
        checkpoint: CheckpointId,
        from_branch: Option<BranchId>,
        parent: Option<CheckpointId>,
        at: VirtTime,
    },
    /// A successful `HYPERCALL_REGISTER_FEEDBACK_BUFFER`. IDs are not unique
    /// (e.g. two processes running the same binary). Descendant branches
    /// inherit the registration; read it via
    /// [`Branch::feedback_buffers`](crate::Branch::feedback_buffers).
    FeedbackBufferRegistered {
        branch: BranchId,
        at: VirtTime,
        id: &'a [u8],
        slot: usize,
        size: u64,
    },
    /// One non-serial record from the branch's event stream (see
    /// [`Branch::set_event_config`](crate::Branch::set_event_config)).
    Record {
        branch: BranchId,
        record: EventRecord<'a>,
    },
}

/// Receives every [`Event`] produced by the tree. `on_event` runs on the thread
/// driving the branch, so it must be cheap and non-blocking.
///
/// Scratch branches created by [`Checkpoint::rewind`](crate::Checkpoint::rewind)
/// emit events too.
pub trait EventSink: Send + Sync {
    fn on_event(&self, event: Event<'_>);
}

/// Default sink; discards everything.
pub(crate) struct Discard;

impl EventSink for Discard {
    fn on_event(&self, _event: Event<'_>) {}
}

/// A serial line not yet terminated by `\n`.
#[derive(Default, Clone, Debug)]
pub(crate) struct PartialLine {
    pub(crate) bytes: Vec<u8>,
    /// TSC of the first byte; meaningful only when `bytes` is non-empty.
    pub(crate) start_tsc: u64,
}

/// Emit one [`Event::SerialLine`] per `\n` in a `Serial` record. Records are
/// stamped with their first byte's TSC, so a fresh line takes `record_tsc`
/// while a line continued across records keeps its earlier start.
pub(crate) fn serial_record_into_sink(
    bytes: &[u8],
    record_tsc: u64,
    freq: u64,
    branch: BranchId,
    sink: &dyn EventSink,
    partial: &mut PartialLine,
) {
    for &byte in bytes {
        if partial.bytes.is_empty() {
            partial.start_tsc = record_tsc;
        }
        if byte == b'\n' {
            let at = VirtTime::from_instructions(partial.start_tsc, freq);
            sink.on_event(Event::SerialLine {
                branch,
                at,
                line: &partial.bytes,
            });
            partial.bytes.clear();
        } else {
            partial.bytes.push(byte);
        }
    }
}

/// [`serial_record_into_sink`] over every `Serial` record (root-boot loop).
pub(crate) fn drain_serial_events(
    drained: &[u8],
    freq: u64,
    branch: BranchId,
    sink: &dyn EventSink,
    partial: &mut PartialLine,
) {
    for record in EventStream::new(drained) {
        if record.kind() == EventKind::Serial.as_u16() {
            serial_record_into_sink(record.payload, record.tsc(), freq, branch, sink, partial);
        }
    }
}

/// After a registration exit, read slot/size from RAX/RCX, look up the id and
/// emit [`Event::FeedbackBufferRegistered`]. `None` if registration failed.
pub(crate) fn emit_feedback_buffer_registered(
    vm: &Vm,
    at: VirtTime,
    branch: BranchId,
    sink: &dyn EventSink,
) -> Result<Option<(usize, u64)>> {
    let regs = vm.get_regs()?;
    let rax = regs.gprs.rax;
    if rax == u64::MAX {
        return Ok(None);
    }
    let slot = rax as usize;
    let size = regs.gprs.rcx;
    let info = vm.get_feedback_buffer_info_at(slot)?;
    if let Some(info) = info {
        sink.on_event(Event::FeedbackBufferRegistered {
            branch,
            at,
            id: info.id_bytes(),
            slot,
            size,
        });
        Ok(Some((slot, size)))
    } else {
        Ok(None)
    }
}
