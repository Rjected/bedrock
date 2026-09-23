// SPDX-License-Identifier: GPL-2.0

//! Configuration types for VM ioctls.

use crate::events::EventCategories;

pub use bedrock_vmx::ExitTrigger;

/// Single-step (MTF) within an emulated TSC range.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SingleStepConfig {
    /// Non-zero = enabled.
    pub enabled: u64,
    /// Inclusive.
    pub tsc_start: u64,
    /// Exclusive.
    pub tsc_end: u64,
}

/// Synthetic exit reason of periodic checkpoint `Exit` records.
pub const EXIT_REASON_CHECKPOINT: u32 = 0xFFFFFFFF;

/// Bit flag: skip memory hashing in exit records (set `memory_hash` to 0).
pub const EXIT_FLAG_NO_MEMORY_HASH: u32 = 1 << 0;
/// Bit flag: intercept guest #PF exceptions for determinism analysis.
pub const EXIT_FLAG_INTERCEPT_PF: u32 = 1 << 1;

/// Event-stream configuration ioctl payload. Enabling allocates the 1 MB
/// buffer; disabled categories cost one bit test at emit time. `Exit` records
/// need both [`EventCategories::EXIT`] and a non-[`Disabled`](ExitTrigger::Disabled)
/// `exit_trigger`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EventConfig {
    pub enabled: u32,
    /// [`EventCategories`] mask.
    pub categories: u32,
    /// [`ExitTrigger`] as u32.
    pub exit_trigger: u32,
    /// `EXIT_FLAG_*` bits.
    pub exit_flags: u32,
    /// `AtTsc` threshold or `Checkpoints` interval; ignored otherwise.
    pub exit_target_tsc: u64,
    /// No `Exit` records before this emulated TSC (0 = from the start).
    pub exit_start_tsc: u64,
}

impl EventConfig {
    /// Disabled config (frees the buffer, emits nothing).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Enabled with no exit trigger; see [`with_exit_trigger`](Self::with_exit_trigger).
    pub fn enabled(categories: EventCategories) -> Self {
        Self {
            enabled: 1,
            categories: categories.0,
            ..Default::default()
        }
    }

    /// `target_tsc` is the `AtTsc` threshold / `Checkpoints` interval, else 0.
    pub fn with_exit_trigger(mut self, trigger: ExitTrigger, target_tsc: u64) -> Self {
        self.exit_trigger = trigger as u32;
        self.exit_target_tsc = target_tsc;
        self
    }

    /// No `Exit` records before `start_tsc`.
    pub fn with_exit_start_tsc(mut self, start_tsc: u64) -> Self {
        self.exit_start_tsc = start_tsc;
        self
    }

    /// Skip memory hashing in exit records (`memory_hash` stays 0).
    pub fn with_no_memory_hash(mut self) -> Self {
        self.exit_flags |= EXIT_FLAG_NO_MEMORY_HASH;
        self
    }

    /// Intercept guest #PF exceptions for determinism analysis.
    pub fn with_intercept_pf(mut self) -> Self {
        self.exit_flags |= EXIT_FLAG_INTERCEPT_PF;
        self
    }

    pub fn categories(&self) -> EventCategories {
        EventCategories(self.categories)
    }
}
