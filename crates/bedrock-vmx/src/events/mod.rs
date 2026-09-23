// SPDX-License-Identifier: GPL-2.0

//! Unified event stream: one time-ordered TLV log merging serial output,
//! injected interrupts, served randomness, I/O channel transactions, and exit
//! records.
//!
//! This module holds the wire-format types shared by the producer and the
//! userspace reader (`bedrock_vm::events`).

mod types;

pub use types::{
    align_up, EventCategories, EventHeader, EventKind, InjectPayload, InjectSource,
    IoChannelPayload, IoChannelPhase, RandomPayload, RandomSource, EVENT_BUFFER_PAGES,
    EVENT_BUFFER_SIZE, EVENT_FLAG_DETERMINISTIC, EVENT_HEADER_SIZE,
};
