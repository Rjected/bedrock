// SPDX-License-Identifier: GPL-2.0

//! File-transmission channel — the host half of the `HYPERCALL_FILE_FETCH` ABI.
//!
//! The guest pulls workload files from the host at boot through one 1 MB
//! feedback buffer registered under [`FILE_XFER_BUFFER_ID`]:
//!
//! 1. The guest writes a request header into the start of the buffer
//!    (`offset`, `name_len`, then the file name) and issues
//!    `HYPERCALL_FILE_FETCH`, which exits to userspace as
//!    [`ExitKind::FileFetch`](crate::ExitKind::FileFetch).
//! 2. The host ([`FileServer::serve`]) reads the request out of the
//!    host-mapped buffer, reads up to `buffer_size - HEADER_LEN` bytes of the
//!    named file at `offset`, and overwrites the buffer with a response header
//!    (`result`) followed by the data.
//! 3. The guest reads `result` back out of the buffer, writes the bytes to its
//!    local file, advances `offset`, and loops until `result == 0` (EOF).
//!
//! The hypervisor treats the buffer as opaque; keep this framing in sync with
//! `guest/file-fetch.c`.
//!
//! ## Determinism
//!
//! Served bytes depend only on the host file and chunk boundaries only on the
//! buffer size. Transfers happen before `HYPERCALL_READY`, so forks never
//! re-fetch.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::Vm;

/// Feedback-buffer id of the guest's file-transfer buffer.
pub const FILE_XFER_BUFFER_ID: &[u8] = b"bedrock-file-xfer";

/// Header size. Request: `u64 offset | u32 name_len | u32 reserved` + name;
/// response: `i64 result | u64 reserved` + data. The response overwrites the
/// request only after it is fully consumed.
pub const FILE_XFER_HEADER_LEN: usize = 16;

/// Response `result` sentinel: the requested file is unknown or unreadable.
pub const FILE_XFER_RESULT_NOT_FOUND: i64 = -1;

/// Serves host files into a guest's file-transfer buffer; call
/// [`serve`](Self::serve) on every [`ExitKind::FileFetch`](crate::ExitKind::FileFetch).
pub struct FileServer {
    files: HashMap<String, FileEntry>,
    /// Resolved on first fetch.
    slot: Option<usize>,
}

struct FileEntry {
    path: PathBuf,
    /// Opened lazily, kept open across chunks.
    handle: Option<File>,
}

impl FileServer {
    /// `files` are `(guest_name, host_path)` pairs.
    pub fn new<I, S, P>(files: I) -> Self
    where
        I: IntoIterator<Item = (S, P)>,
        S: Into<String>,
        P: AsRef<Path>,
    {
        let files = files
            .into_iter()
            .map(|(name, path)| {
                (
                    name.into(),
                    FileEntry {
                        path: path.as_ref().to_path_buf(),
                        handle: None,
                    },
                )
            })
            .collect();
        Self { files, slot: None }
    }

    /// An empty server still answers every fetch with not-found.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Answer one fetch with the next chunk, returning its size (0 at EOF). An
    /// unknown or unopenable file gets [`FILE_XFER_RESULT_NOT_FOUND`] and
    /// `Ok(0)`; only host-side failures are errors.
    pub fn serve(&mut self, vm: &mut Vm) -> io::Result<usize> {
        let slot = self.resolve_slot(vm)?;

        // Read-write so the response lands in the guest's pages.
        if vm.feedback_buffer_mut_at(slot).is_none() {
            vm.map_feedback_buffer_mut_at(slot)?;
        }
        let buf = vm
            .feedback_buffer_mut_at(slot)
            .ok_or_else(|| io::Error::other("file-xfer buffer disappeared after mapping"))?;

        if buf.len() < FILE_XFER_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file-xfer buffer smaller than header",
            ));
        }

        let offset = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let name_len = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        let data_cap = buf.len() - FILE_XFER_HEADER_LEN;
        if FILE_XFER_HEADER_LEN + name_len > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file-xfer request name_len {} overflows buffer", name_len),
            ));
        }
        let name =
            String::from_utf8_lossy(&buf[FILE_XFER_HEADER_LEN..FILE_XFER_HEADER_LEN + name_len])
                .into_owned();

        let Some(entry) = self.files.get_mut(&name) else {
            write_result(buf, FILE_XFER_RESULT_NOT_FOUND);
            return Ok(0);
        };

        if entry.handle.is_none() {
            match File::open(&entry.path) {
                Ok(f) => entry.handle = Some(f),
                Err(_) => {
                    write_result(buf, FILE_XFER_RESULT_NOT_FOUND);
                    return Ok(0);
                }
            }
        }
        let file = entry.handle.as_mut().expect("handle opened above");

        file.seek(SeekFrom::Start(offset))?;
        let n = read_up_to(
            file,
            &mut buf[FILE_XFER_HEADER_LEN..FILE_XFER_HEADER_LEN + data_cap],
        )?;
        write_result(buf, n as i64);
        Ok(n)
    }

    fn resolve_slot(&mut self, vm: &Vm) -> io::Result<usize> {
        if let Some(slot) = self.slot {
            return Ok(slot);
        }
        let slots = vm.feedback_buffer_slots_for_id(FILE_XFER_BUFFER_ID)?;
        let slot = slots.first().copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "guest issued HYPERCALL_FILE_FETCH but registered no bedrock-file-xfer buffer",
            )
        })?;
        self.slot = Some(slot);
        Ok(slot)
    }
}

/// Write the response header.
fn write_result(buf: &mut [u8], result: i64) {
    buf[0..8].copy_from_slice(&result.to_le_bytes());
    buf[8..16].fill(0);
}

/// Read until `dst` is full or EOF. Retrying short reads keeps chunk
/// boundaries deterministic.
fn read_up_to<R: Read>(r: &mut R, dst: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < dst.len() {
        match r.read(&mut dst[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}
