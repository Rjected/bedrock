// SPDX-License-Identifier: GPL-2.0

//! Ioctl encoding and constants for bedrock device.

use std::mem::size_of;

use super::config::{EventConfig, SingleStepConfig};
use super::exit::VmExit;
use super::stats::ExitStats;
use crate::rdrand::RdrandConfig;
use crate::Regs;

/// Ioctl magic number ('B' for Bedrock).
const BEDROCK_IOC_MAGIC: u8 = b'B';

// Ioctl direction bits
pub(super) const IOC_WRITE: u64 = 1;
pub(super) const IOC_READ: u64 = 2;

// Ioctl encoding shifts
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = 8;
const IOC_SIZESHIFT: u64 = 16;
const IOC_DIRSHIFT: u64 = 30;

/// Encode an ioctl number for reading data (_IOR).
const fn ioctl_ior(ty: u8, nr: u8, size: usize) -> u64 {
    ((IOC_READ) << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((nr as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)
}

/// Encode an ioctl number for writing data (_IOW).
const fn ioctl_iow(ty: u8, nr: u8, size: usize) -> u64 {
    ((IOC_WRITE) << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((nr as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)
}

/// Configuration passed to CREATE_ROOT_VM ioctl.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CreateVmConfig {
    pub memory_size: u64,
    /// Emulated TSC frequency in Hz.
    pub tsc_frequency: u64,
}

// Device ioctls (on /dev/bedrock)
// _IOW('B', 0, CreateVmConfig) - takes a configuration struct as argument
pub(crate) const BEDROCK_CREATE_ROOT_VM: u64 =
    ioctl_iow(BEDROCK_IOC_MAGIC, 0, size_of::<CreateVmConfig>());

// VM ioctls (on VM file descriptor)
pub(crate) const BEDROCK_VM_GET_REGS: u64 = ioctl_ior(BEDROCK_IOC_MAGIC, 1, size_of::<Regs>());
pub(crate) const BEDROCK_VM_SET_REGS: u64 = ioctl_iow(BEDROCK_IOC_MAGIC, 2, size_of::<Regs>());
pub(crate) const BEDROCK_VM_RUN: u64 = ioctl_ior(BEDROCK_IOC_MAGIC, 3, size_of::<VmExit>());
pub(crate) const BEDROCK_VM_SET_RDRAND_CONFIG: u64 =
    ioctl_iow(BEDROCK_IOC_MAGIC, 4, size_of::<RdrandConfig>());
pub(crate) const BEDROCK_VM_SET_RDRAND_VALUE: u64 =
    ioctl_iow(BEDROCK_IOC_MAGIC, 5, size_of::<u64>());
pub(crate) const BEDROCK_VM_SET_SINGLE_STEP: u64 =
    ioctl_iow(BEDROCK_IOC_MAGIC, 6, size_of::<SingleStepConfig>());
pub(crate) const BEDROCK_VM_GET_EXIT_STATS: u64 =
    ioctl_ior(BEDROCK_IOC_MAGIC, 7, size_of::<ExitStats>());
pub(crate) const BEDROCK_VM_SET_STOP_TSC: u64 = ioctl_iow(BEDROCK_IOC_MAGIC, 8, size_of::<u64>());
pub(crate) const BEDROCK_VM_GET_VM_ID: u64 = ioctl_ior(BEDROCK_IOC_MAGIC, 9, size_of::<u64>());
pub(crate) const BEDROCK_VM_SET_EVENT_CONFIG: u64 =
    ioctl_iow(BEDROCK_IOC_MAGIC, 13, size_of::<EventConfig>());

/// Max bytes served per `HYPERCALL_GET_RANDOM` (the guest loops for more).
/// Must match `bedrock_vmx::RANDOM_REPLY_MAX`.
pub const RANDOM_REPLY_MAX: usize = 256;

/// The pending `HYPERCALL_GET_RANDOM` request.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct RandomRequest {
    /// Requester's `current->tgid`.
    pub pid: u32,
    /// Capped at `RANDOM_REPLY_MAX`.
    pub len: u32,
}

/// Reply bytes for the pending `GET_RANDOM` request, inline so the ABI carries
/// no pointers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RandomBytes {
    pub len: u32,
    pub _reserved: u32,
    pub data: [u8; RANDOM_REPLY_MAX],
}

impl Default for RandomBytes {
    fn default() -> Self {
        Self {
            len: 0,
            _reserved: 0,
            data: [0; RANDOM_REPLY_MAX],
        }
    }
}

// _IOR('B', 14, RandomRequest) - read the pending GET_RANDOM request (pid+len).
pub(crate) const BEDROCK_VM_GET_RANDOM_REQUEST: u64 =
    ioctl_ior(BEDROCK_IOC_MAGIC, 14, size_of::<RandomRequest>());

// _IOW('B', 15, RandomBytes) - stage the reply bytes for the pending request.
pub(crate) const BEDROCK_VM_SET_RANDOM_BYTES: u64 =
    ioctl_iow(BEDROCK_IOC_MAGIC, 15, size_of::<RandomBytes>());

// Device ioctls (on /dev/bedrock)
// _IOW('B', 1, u64) - takes parent VM ID as argument
pub(crate) const BEDROCK_CREATE_FORKED_VM: u64 = ioctl_iow(BEDROCK_IOC_MAGIC, 1, size_of::<u64>());

/// Request structure for getting feedback buffer info.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct FeedbackBufferInfoRequest {
    /// An unregistered index reports `registered = 0`.
    pub index: u32,
    pub _reserved: u32,
}

// _IOR('B', 10, FeedbackBufferInfoRequest) - get feedback buffer registration info
pub(crate) const BEDROCK_VM_GET_FEEDBACK_BUFFER_INFO: u64 = ioctl_ior(
    BEDROCK_IOC_MAGIC,
    10,
    size_of::<FeedbackBufferInfoRequest>(),
);

/// Maximum size of an I/O channel request or response payload (one 4KB page).
pub const IO_CHANNEL_BUF_SIZE: usize = 4096;

/// Payload for both `BEDROCK_VM_QUEUE_IO_ACTION` and
/// `BEDROCK_VM_DRAIN_IO_RESPONSE`, with the data inline so the ABI carries no
/// pointers. The kernel copies `data` directly to/from `VmState` to avoid a
/// 4KB stack burst.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoActionPayload {
    /// QUEUE: bytes supplied. DRAIN: capacity on input, bytes written on output.
    pub len: u32,
    pub _reserved: u32,
    /// QUEUE only: 0 = fire when interruptible; otherwise PEBS lands the IRQ
    /// at exactly this emulated TSC.
    pub target_tsc: u64,
    pub data: [u8; IO_CHANNEL_BUF_SIZE],
}

// SAFETY of the `Default` impl: `IoActionPayload` is plain old data with no
// invariants; an all-zero state means "empty payload, no data, no target".
impl Default for IoActionPayload {
    fn default() -> Self {
        Self {
            len: 0,
            _reserved: 0,
            target_tsc: 0,
            data: [0; IO_CHANNEL_BUF_SIZE],
        }
    }
}

// _IOW('B', 11, IoActionPayload) - queue an I/O channel request for the guest
pub(crate) const BEDROCK_VM_QUEUE_IO_ACTION: u64 =
    ioctl_iow(BEDROCK_IOC_MAGIC, 11, size_of::<IoActionPayload>());

// _IOR('B', 12, IoActionPayload) - drain the most recent I/O channel response
pub(crate) const BEDROCK_VM_DRAIN_IO_RESPONSE: u64 =
    ioctl_ior(BEDROCK_IOC_MAGIC, 12, size_of::<IoActionPayload>());

/// Max feedback-buffer id length; must match
/// `bedrock_vmx::FEEDBACK_BUFFER_ID_MAX_LEN`.
pub const FEEDBACK_BUFFER_ID_MAX_LEN: usize = 128;

/// A guest-registered feedback buffer. `id` (e.g. a build-id) need not be
/// unique; see `bedrock_vmx::FeedbackBufferInfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FeedbackBufferInfo {
    /// Original guest virtual address.
    pub gva: u64,
    pub size: u64,
    pub num_pages: u64,
    /// 0 = no, 1 = yes.
    pub registered: u32,
    pub index: u32,
    pub id_len: u32,
    pub _reserved: u32,
    /// Zero past `id_len`.
    pub id: [u8; FEEDBACK_BUFFER_ID_MAX_LEN],
}

impl Default for FeedbackBufferInfo {
    fn default() -> Self {
        Self {
            gva: 0,
            size: 0,
            num_pages: 0,
            registered: 0,
            index: 0,
            id_len: 0,
            _reserved: 0,
            id: [0u8; FEEDBACK_BUFFER_ID_MAX_LEN],
        }
    }
}

impl FeedbackBufferInfo {
    pub fn id_bytes(&self) -> &[u8] {
        &self.id[..self.id_len as usize]
    }
}
