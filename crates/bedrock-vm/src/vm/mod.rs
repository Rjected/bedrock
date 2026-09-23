// SPDX-License-Identifier: GPL-2.0

//! Vm - Userspace interface to a bedrock VM.

mod config;
mod exit;
mod ioctl;
mod stats;

pub use config::{EventConfig, ExitTrigger, SingleStepConfig, EXIT_REASON_CHECKPOINT};
pub use exit::{ExitKind, VmExit};
pub use ioctl::{
    FeedbackBufferInfo, FeedbackBufferInfoRequest, IoActionPayload, RandomBytes, RandomRequest,
    IO_CHANNEL_BUF_SIZE, RANDOM_REPLY_MAX,
};
pub use stats::{ExitStatEntry, ExitStats, ExitStatsReport, IoctlStats};

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::slice;
use std::time::Instant;

use crate::rdrand::RdrandConfig;
use crate::Regs;
use ioctl::*;

/// Path to the bedrock device file.
pub const BEDROCK_DEVICE_PATH: &str = "/dev/bedrock";

/// Default guest memory size (4 GB).
pub const DEFAULT_MEMORY_SIZE: usize = 4 * 1024 * 1024 * 1024;

/// Default TSC frequency (2995.2 MHz) for deterministic time emulation.
pub use bedrock_vmx::DEFAULT_TSC_FREQUENCY;

/// Size of the unified event buffer (1 MB), mmap'd to userspace.
pub use crate::events::EVENT_BUFFER_SIZE;

/// Per-buffer mmap slot size (1 MB), shared with the kernel via `bedrock_vmx`.
/// Each buffer is capped at this size; the number of buffers is unbounded.
const FEEDBACK_BUFFER_SLOT_SIZE: usize = bedrock_vmx::FEEDBACK_BUFFER_SLOT_SIZE as usize;

/// Fixed sentinel mmap offset of the event buffer, above guest memory and the
/// unbounded feedback region. Shared with the kernel via `bedrock_vmx`.
const EVENT_BUFFER_MMAP_OFFSET: usize = bedrock_vmx::EVENT_BUFFER_MMAP_OFFSET as usize;

/// A userspace handle to a bedrock VM; dropping it unmaps everything and closes
/// the fd, releasing the VM.
///
/// Root VMs (`create()`) have direct guest-memory access; forked VMs (`fork()`,
/// `create_forked()`) share the parent's memory copy-on-write and do not.
pub struct Vm {
    fd: OwnedFd,
    /// `None` for forked VMs.
    memory_ptr: Option<NonNull<u8>>,
    /// 0 for forked VMs.
    memory_size: usize,
    /// `None` while the event stream is disabled.
    event_ptr: Option<NonNull<u8>>,
    event_mmap_offset: libc::off_t,
    /// Indexed by slot; grows on demand. Same length as `feedback_buffer_sizes`.
    feedback_buffer_ptrs: Vec<Option<NonNull<u8>>>,
    feedback_buffer_sizes: Vec<usize>,
    ioctl_stats: Cell<IoctlStats>,
    forked: bool,
}

// SAFETY: The mapped memory is owned exclusively by Vm and can be
// safely sent between threads.
unsafe impl Send for Vm {}
unsafe impl Sync for Vm {}

impl Drop for Vm {
    fn drop(&mut self) {
        unsafe {
            if let Some(ptr) = self.memory_ptr {
                libc::munmap(ptr.as_ptr() as *mut libc::c_void, self.memory_size);
            }
            if let Some(event_ptr) = self.event_ptr {
                libc::munmap(event_ptr.as_ptr() as *mut libc::c_void, EVENT_BUFFER_SIZE);
            }
            for i in 0..self.feedback_buffer_ptrs.len() {
                if let Some(fb_ptr) = self.feedback_buffer_ptrs[i] {
                    libc::munmap(
                        fb_ptr.as_ptr() as *mut libc::c_void,
                        self.feedback_buffer_sizes[i],
                    );
                }
            }
        }
    }
}

impl Vm {
    /// Create a root VM with `memory_size` bytes of guest memory (mapped into
    /// this process) and [`DEFAULT_TSC_FREQUENCY`].
    pub fn create(memory_size: usize) -> io::Result<Self> {
        Self::create_with_tsc_frequency(memory_size, DEFAULT_TSC_FREQUENCY)
    }

    pub fn create_with_tsc_frequency(memory_size: usize, tsc_frequency: u64) -> io::Result<Self> {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open(BEDROCK_DEVICE_PATH)?;

        Self::create_from_device(&device, memory_size, tsc_frequency)
    }

    /// Create a root VM with [`DEFAULT_MEMORY_SIZE`].
    pub fn create_default() -> io::Result<Self> {
        Self::create(DEFAULT_MEMORY_SIZE)
    }

    /// Create a root VM from an already-opened bedrock device.
    pub fn create_from_device(
        device: &File,
        memory_size: usize,
        tsc_frequency: u64,
    ) -> io::Result<Self> {
        if memory_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "memory size must be greater than 0",
            ));
        }
        if tsc_frequency == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tsc frequency must be greater than 0",
            ));
        }

        let config = CreateVmConfig {
            memory_size: memory_size as u64,
            tsc_frequency,
        };

        let fd = unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                BEDROCK_CREATE_ROOT_VM as libc::c_ulong,
                &config as *const CreateVmConfig,
            )
        };

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                memory_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let memory_ptr = Some(unsafe { NonNull::new_unchecked(ptr as *mut u8) });

        // Root mmap layout: mem | feedback[0..] (unbounded) | event-at-sentinel.
        let event_mmap_offset = EVENT_BUFFER_MMAP_OFFSET as libc::off_t;

        Ok(Self {
            fd,
            memory_ptr,
            memory_size,
            event_ptr: None,
            event_mmap_offset,
            feedback_buffer_ptrs: Vec::new(),
            feedback_buffer_sizes: Vec::new(),
            ioctl_stats: Cell::new(IoctlStats::default()),
            forked: false,
        })
    }

    /// Create a copy-on-write fork of the VM with `parent_vm_id`. The parent
    /// cannot run while it has live forks.
    pub fn create_forked(parent_vm_id: u64) -> io::Result<Self> {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open(BEDROCK_DEVICE_PATH)?;

        let fd = unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                BEDROCK_CREATE_FORKED_VM as libc::c_ulong,
                parent_vm_id,
            )
        };

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // Forked mmap layout: feedback[0..] (unbounded) | event-at-sentinel.
        let event_mmap_offset = EVENT_BUFFER_MMAP_OFFSET as libc::off_t;

        Ok(Self {
            fd,
            memory_ptr: None,
            memory_size: 0,
            event_ptr: None,
            event_mmap_offset,
            feedback_buffer_ptrs: Vec::new(),
            feedback_buffer_sizes: Vec::new(),
            ioctl_stats: Cell::new(IoctlStats::default()),
            forked: true,
        })
    }

    /// Create a copy-on-write fork of this VM.
    pub fn fork(&self) -> io::Result<Self> {
        let vm_id = self.get_vm_id()?;
        Self::create_forked(vm_id)
    }

    pub fn is_forked(&self) -> bool {
        self.forked
    }

    pub fn is_root(&self) -> bool {
        !self.forked
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    // --- Memory access (root VMs only) ---

    /// Guest memory; errors on forked VMs.
    pub fn memory(&self) -> io::Result<&[u8]> {
        match self.memory_ptr {
            Some(ptr) => Ok(unsafe { slice::from_raw_parts(ptr.as_ptr(), self.memory_size) }),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "forked VMs do not have direct memory access",
            )),
        }
    }

    /// Mutable guest memory; errors on forked VMs.
    pub fn memory_mut(&mut self) -> io::Result<&mut [u8]> {
        match self.memory_ptr {
            Some(ptr) => Ok(unsafe { slice::from_raw_parts_mut(ptr.as_ptr(), self.memory_size) }),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "forked VMs do not have direct memory access",
            )),
        }
    }

    /// Guest memory size; 0 for forked VMs.
    pub fn memory_size(&self) -> usize {
        self.memory_size
    }

    // --- Deterministic I/O channel ---

    /// Queue an I/O channel request for the guest's next IRQ. `target_tsc == 0`
    /// fires as soon as the guest is interruptible; otherwise PEBS lands the IRQ
    /// at exactly that emulated TSC.
    ///
    /// The IRQ is held until the guest registers its page via `bedrock-io.ko`.
    /// Fails with `EBUSY` if a request is already in flight.
    pub fn queue_io_action(&self, data: &[u8], target_tsc: u64) -> io::Result<()> {
        if data.len() > IO_CHANNEL_BUF_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "I/O action too large: {} > {}",
                    data.len(),
                    IO_CHANNEL_BUF_SIZE
                ),
            ));
        }

        // Boxed to keep 4KB off the stack.
        let mut payload = Box::new(IoActionPayload::default());
        payload.len = data.len() as u32;
        payload.target_tsc = target_tsc;
        payload.data[..data.len()].copy_from_slice(data);

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_QUEUE_IO_ACTION as libc::c_ulong,
                payload.as_ref() as *const IoActionPayload,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Drain the I/O response after `ExitKind::IoResponse` (empty if none),
    /// freeing the slot for the next queue.
    pub fn drain_io_response(&self) -> io::Result<Vec<u8>> {
        let mut payload = Box::new(IoActionPayload::default());
        // On input, `len` is the buffer capacity.
        payload.len = IO_CHANNEL_BUF_SIZE as u32;

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_DRAIN_IO_RESPONSE as libc::c_ulong,
                payload.as_mut() as *mut IoActionPayload,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let n = (payload.len as usize).min(IO_CHANNEL_BUF_SIZE);
        Ok(payload.data[..n].to_vec())
    }

    // --- Registers ---

    /// Read all VM registers.
    pub fn get_regs(&self) -> io::Result<Regs> {
        let start = Instant::now();
        let mut regs = Regs::default();

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_GET_REGS as libc::c_ulong,
                &mut regs as *mut Regs,
            )
        };

        self.record_ioctl_time(|s| {
            s.get_regs_ns += start.elapsed().as_nanos() as u64;
            s.get_regs_count += 1;
        });

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(regs)
    }

    /// Write all VM registers.
    pub fn set_regs(&self, regs: &Regs) -> io::Result<()> {
        let start = Instant::now();

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_REGS as libc::c_ulong,
                regs as *const Regs,
            )
        };

        self.record_ioctl_time(|s| {
            s.set_regs_ns += start.elapsed().as_nanos() as u64;
            s.set_regs_count += 1;
        });

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    // --- Execution ---

    /// Run the VM until it exits.
    pub fn run(&self) -> io::Result<VmExit> {
        let start = Instant::now();

        let mut exit = VmExit {
            exit_reason: 0,
            _reserved: 0,
            exit_qualification: 0,
            guest_physical_addr: 0,
            event_len: 0,
            _pad: 0,
            emulated_tsc: 0,
            tsc_frequency: 0,
        };

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_RUN as libc::c_ulong,
                &mut exit as *mut VmExit,
            )
        };

        self.record_ioctl_time(|s| {
            s.run_ns += start.elapsed().as_nanos() as u64;
            s.run_count += 1;
        });

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(exit)
    }

    // --- RDRAND configuration ---

    /// Configure RDRAND/RDSEED instruction emulation.
    pub fn set_rdrand_config(&self, config: &RdrandConfig) -> io::Result<()> {
        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_RDRAND_CONFIG as libc::c_ulong,
                config as *const RdrandConfig,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Set the value for the next RDRAND/RDSEED (ExitToUserspace mode).
    pub fn set_rdrand_value(&self, value: u64) -> io::Result<()> {
        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_RDRAND_VALUE as libc::c_ulong,
                &value as *const u64,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Read the pending request after [`ExitKind::VmcallGetRandom`]: PID and
    /// byte count (capped at `RANDOM_REPLY_MAX`).
    pub fn random_request(&self) -> io::Result<RandomRequest> {
        let mut req = RandomRequest::default();
        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_GET_RANDOM_REQUEST as libc::c_ulong,
                &mut req as *mut RandomRequest,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(req)
    }

    /// Stage reply bytes for the pending `HYPERCALL_GET_RANDOM`; the next
    /// `run()` completes the VMCALL. Truncated to `RANDOM_REPLY_MAX`.
    pub fn set_random_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let mut payload = RandomBytes::default();
        let n = bytes.len().min(RANDOM_REPLY_MAX);
        payload.data[..n].copy_from_slice(&bytes[..n]);
        payload.len = n as u32;

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_RANDOM_BYTES as libc::c_ulong,
                &payload as *const RandomBytes,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    // --- Event stream ---

    /// Configure the event stream (see [`EventConfig`]), allocating/freeing the
    /// kernel buffer and mapping/unmapping it here. Read it after each
    /// [`run`](Self::run) via [`event_buffer`](Self::event_buffer).
    pub fn set_event_config(&mut self, config: &EventConfig) -> io::Result<()> {
        let was_enabled = self.event_ptr.is_some();
        let want_enabled = config.enabled != 0;

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_EVENT_CONFIG as libc::c_ulong,
                config as *const EventConfig,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        if want_enabled && !was_enabled {
            let event_ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    EVENT_BUFFER_SIZE,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    self.fd.as_raw_fd(),
                    self.event_mmap_offset,
                )
            };

            if event_ptr == libc::MAP_FAILED {
                // Roll back the kernel-side enable so state stays consistent.
                let disabled = EventConfig::disabled();
                unsafe {
                    libc::ioctl(
                        self.fd.as_raw_fd(),
                        BEDROCK_VM_SET_EVENT_CONFIG as libc::c_ulong,
                        &disabled as *const EventConfig,
                    );
                }
                return Err(io::Error::last_os_error());
            }

            self.event_ptr = Some(unsafe { NonNull::new_unchecked(event_ptr as *mut u8) });
        } else if !want_enabled && was_enabled {
            if let Some(event_ptr) = self.event_ptr.take() {
                unsafe {
                    libc::munmap(event_ptr.as_ptr() as *mut libc::c_void, EVENT_BUFFER_SIZE);
                }
            }
        }

        Ok(())
    }

    /// Whether the event stream is enabled (buffer mapped).
    pub fn event_stream_enabled(&self) -> bool {
        self.event_ptr.is_some()
    }

    /// The whole event buffer; slice to `VmExit::event_len` and parse with
    /// [`crate::events::EventStream`].
    pub fn event_buffer(&self) -> Option<&[u8]> {
        self.event_ptr
            .map(|ptr| unsafe { slice::from_raw_parts(ptr.as_ptr(), EVENT_BUFFER_SIZE) })
    }

    // --- Feedback buffer ---

    /// Registration info for slot `index`, or `None` if unregistered.
    /// Registration is append-only and contiguous, so querying `0, 1, 2, …`
    /// until the first `None` enumerates all buffers.
    pub fn get_feedback_buffer_info_at(
        &self,
        index: usize,
    ) -> io::Result<Option<FeedbackBufferInfo>> {
        // The kernel reads the 8-byte request from the start of this buffer and
        // writes the larger response over it.
        let mut info = std::mem::MaybeUninit::<FeedbackBufferInfo>::uninit();

        let request = FeedbackBufferInfoRequest {
            index: index as u32,
            _reserved: 0,
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &request as *const FeedbackBufferInfoRequest as *const u8,
                info.as_mut_ptr() as *mut u8,
                std::mem::size_of::<FeedbackBufferInfoRequest>(),
            );
        }

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_GET_FEEDBACK_BUFFER_INFO as libc::c_ulong,
                info.as_mut_ptr(),
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: The ioctl succeeded and the kernel wrote FeedbackBufferInfo to the buffer
        let info = unsafe { info.assume_init() };

        if info.registered == 0 {
            Ok(None)
        } else {
            Ok(Some(info))
        }
    }

    /// [`get_feedback_buffer_info_at`](Self::get_feedback_buffer_info_at) slot 0.
    pub fn get_feedback_buffer_info(&self) -> io::Result<Option<FeedbackBufferInfo>> {
        self.get_feedback_buffer_info_at(0)
    }

    /// Map the registered feedback buffer at `index` read-only. Errors if
    /// unregistered or already mapped.
    ///
    /// For forked VMs the kernel CoWs every buffer page into this VM at map
    /// time, so the mapping stays coherent across later runs without remapping.
    pub fn map_feedback_buffer_at(&mut self, index: usize) -> io::Result<&[u8]> {
        if self.feedback_buffer_at(index).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("feedback buffer {} is already mapped", index),
            ));
        }

        let info = self.get_feedback_buffer_info_at(index)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no feedback buffer registered at index {}", index),
            )
        })?;

        let size = info.num_pages as usize * 4096;
        let offset = self.feedback_buffer_mmap_offset_at(index);

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.fd.as_raw_fd(),
                offset,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let ptr = unsafe { NonNull::new_unchecked(ptr as *mut u8) };
        self.ensure_feedback_slot(index);
        self.feedback_buffer_ptrs[index] = Some(ptr);
        self.feedback_buffer_sizes[index] = size;

        Ok(unsafe { slice::from_raw_parts(ptr.as_ptr(), size) })
    }

    /// [`map_feedback_buffer_at`](Self::map_feedback_buffer_at) slot 0.
    pub fn map_feedback_buffer(&mut self) -> io::Result<&[u8]> {
        self.map_feedback_buffer_at(0)
    }

    /// Read-write variant of [`map_feedback_buffer_at`](Self::map_feedback_buffer_at):
    /// writes land directly in the guest's pages (`bedrock_remap_pages` honours
    /// the VMA's `vm_page_prot`). Used by [`crate::file_xfer`].
    pub fn map_feedback_buffer_mut_at(&mut self, index: usize) -> io::Result<&mut [u8]> {
        if self.feedback_buffer_at(index).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("feedback buffer {} is already mapped", index),
            ));
        }

        let info = self.get_feedback_buffer_info_at(index)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no feedback buffer registered at index {}", index),
            )
        })?;

        let size = info.num_pages as usize * 4096;
        let offset = self.feedback_buffer_mmap_offset_at(index);

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd.as_raw_fd(),
                offset,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let ptr = unsafe { NonNull::new_unchecked(ptr as *mut u8) };
        self.ensure_feedback_slot(index);
        self.feedback_buffer_ptrs[index] = Some(ptr);
        self.feedback_buffer_sizes[index] = size;

        Ok(unsafe { slice::from_raw_parts_mut(ptr.as_ptr(), size) })
    }

    /// Mutable view of a mapped buffer. Writing faults unless it was mapped
    /// with [`map_feedback_buffer_mut_at`](Self::map_feedback_buffer_mut_at).
    pub fn feedback_buffer_mut_at(&mut self, index: usize) -> Option<&mut [u8]> {
        if index >= self.feedback_buffer_ptrs.len() {
            return None;
        }

        self.feedback_buffer_ptrs[index].map(|ptr| unsafe {
            slice::from_raw_parts_mut(ptr.as_ptr(), self.feedback_buffer_sizes[index])
        })
    }

    /// Unmap the feedback buffer at `index` early (drop also unmaps).
    pub fn unmap_feedback_buffer_at(&mut self, index: usize) {
        if index >= self.feedback_buffer_ptrs.len() {
            return;
        }

        if let Some(ptr) = self.feedback_buffer_ptrs[index].take() {
            unsafe {
                libc::munmap(
                    ptr.as_ptr() as *mut libc::c_void,
                    self.feedback_buffer_sizes[index],
                );
            }
            self.feedback_buffer_sizes[index] = 0;
        }
    }

    pub fn unmap_feedback_buffer(&mut self) {
        self.unmap_feedback_buffer_at(0);
    }

    /// The feedback buffer at `index`, if mapped.
    pub fn feedback_buffer_at(&self, index: usize) -> Option<&[u8]> {
        if index >= self.feedback_buffer_ptrs.len() {
            return None;
        }

        self.feedback_buffer_ptrs[index].map(|ptr| unsafe {
            slice::from_raw_parts(ptr.as_ptr(), self.feedback_buffer_sizes[index])
        })
    }

    pub fn feedback_buffer(&self) -> Option<&[u8]> {
        self.feedback_buffer_at(0)
    }

    /// Ascending slot indices whose id matches `id` (ids are not unique).
    /// Issues one ioctl per registered slot.
    pub fn feedback_buffer_slots_for_id(&self, id: &[u8]) -> io::Result<Vec<usize>> {
        let mut hits = Vec::new();
        let mut slot = 0;
        while let Some(info) = self.get_feedback_buffer_info_at(slot)? {
            if info.id_bytes() == id {
                hits.push(slot);
            }
            slot += 1;
        }
        Ok(hits)
    }

    /// Grow the slot bookkeeping vectors so `index` is in range.
    fn ensure_feedback_slot(&mut self, index: usize) {
        if self.feedback_buffer_ptrs.len() <= index {
            self.feedback_buffer_ptrs.resize(index + 1, None);
            self.feedback_buffer_sizes.resize(index + 1, 0);
        }
    }

    /// Slot `index` lives at `base + index * FEEDBACK_BUFFER_SLOT_SIZE`.
    fn feedback_buffer_mmap_offset_at(&self, index: usize) -> libc::off_t {
        let base_offset = if self.forked {
            // Forked layout: feedback_base(0) | event-at-sentinel
            0
        } else {
            // Root layout: mem | feedback_base(mem_size) | event-at-sentinel
            self.memory_size
        };

        (base_offset + index * FEEDBACK_BUFFER_SLOT_SIZE) as libc::off_t
    }

    // --- Execution control ---

    /// Set the TSC value at which the VM should stop.
    pub fn set_stop_at_tsc(&self, tsc: Option<u64>) -> io::Result<()> {
        let value = tsc.unwrap_or(0);

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_STOP_TSC as libc::c_ulong,
                &value as *const u64,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Enable single-stepping (MTF) for a specific TSC range.
    pub fn set_single_step_range(&self, tsc_start: u64, tsc_end: u64) -> io::Result<()> {
        let config = SingleStepConfig {
            enabled: 1,
            tsc_start,
            tsc_end,
        };

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_SINGLE_STEP as libc::c_ulong,
                &config as *const SingleStepConfig,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Disable single-stepping (MTF).
    pub fn disable_single_step(&self) -> io::Result<()> {
        let config = SingleStepConfig {
            enabled: 0,
            tsc_start: 0,
            tsc_end: 0,
        };

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_SET_SINGLE_STEP as libc::c_ulong,
                &config as *const SingleStepConfig,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    // --- Statistics ---

    /// Get the unique VM identifier.
    pub fn get_vm_id(&self) -> io::Result<u64> {
        let mut vm_id: u64 = 0;

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_GET_VM_ID as libc::c_ulong,
                &mut vm_id as *mut u64,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(vm_id)
    }

    /// Retrieve exit handler performance statistics.
    pub fn get_exit_stats(&self) -> io::Result<ExitStats> {
        let mut stats = ExitStats::default();

        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                BEDROCK_VM_GET_EXIT_STATS as libc::c_ulong,
                &mut stats as *mut ExitStats,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(stats)
    }

    /// Get userspace ioctl timing statistics.
    pub fn get_ioctl_stats(&self) -> IoctlStats {
        self.ioctl_stats.get()
    }

    fn record_ioctl_time<F: FnOnce(&mut IoctlStats)>(&self, f: F) {
        let mut stats = self.ioctl_stats.get();
        f(&mut stats);
        self.ioctl_stats.set(stats);
    }

    // --- Convenience methods ---

    /// Read-modify-write the registers.
    pub fn modify_regs<F: FnOnce(&mut Regs)>(&self, f: F) -> io::Result<()> {
        let mut regs = self.get_regs()?;
        f(&mut regs);
        self.set_regs(&regs)
    }

    pub fn rip(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.rip)
    }

    pub fn set_rip(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.rip = value)
    }

    pub fn rsp(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.gprs.rsp)
    }

    pub fn set_rsp(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.gprs.rsp = value)
    }

    pub fn rax(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.gprs.rax)
    }

    pub fn set_rax(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.gprs.rax = value)
    }

    pub fn rbx(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.gprs.rbx)
    }

    pub fn set_rbx(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.gprs.rbx = value)
    }

    pub fn rcx(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.gprs.rcx)
    }

    pub fn set_rcx(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.gprs.rcx = value)
    }

    pub fn rdx(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.gprs.rdx)
    }

    pub fn set_rdx(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.gprs.rdx = value)
    }

    pub fn rdi(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.gprs.rdi)
    }

    pub fn set_rdi(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.gprs.rdi = value)
    }

    pub fn rsi(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.gprs.rsi)
    }

    pub fn set_rsi(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.gprs.rsi = value)
    }

    pub fn rflags(&self) -> io::Result<u64> {
        Ok(self.get_regs()?.rflags)
    }

    pub fn set_rflags(&self, value: u64) -> io::Result<()> {
        self.modify_regs(|r| r.rflags = value)
    }
}

#[cfg(test)]
#[path = "vm_tests.rs"]
mod tests;
