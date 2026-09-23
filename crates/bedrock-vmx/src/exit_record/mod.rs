// SPDX-License-Identifier: GPL-2.0

//! Exit-record structure for non-determinism diagnosis.
//!
//! [`ExitRecord`] is the payload of an `EventKind::Exit` event: TSC, exit
//! reason, guest registers, device state hashes and a guest-memory hash.

mod hash;
mod record;

pub use hash::{hash_guest_memory, StateHash, Xxh64Hasher};
pub use record::{ExitRecord, EXIT_RECORD_FLAG_DETERMINISTIC, EXIT_RECORD_SIZE};
