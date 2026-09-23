// SPDX-License-Identifier: GPL-2.0

//! Hash utilities for deterministic state hashing.
//!
//! XXH64 via the kernel's xxhash in kernel builds; disabled (returns 0) in cargo
//! builds.

/// Deterministic 64-bit state hash, implemented by device state structs.
pub trait StateHash {
    fn state_hash(&self) -> u64;
}

// Cargo build: hashing disabled (returns 0).

#[cfg(feature = "cargo")]
mod cargo_impl {
    /// XXH64 streaming hasher (no-op in cargo builds).
    pub struct Xxh64Hasher;

    impl Xxh64Hasher {
        pub fn new() -> Self {
            Self
        }

        #[inline]
        pub fn write_u8(&mut self, _byte: u8) {}

        #[inline]
        pub fn write_u16(&mut self, _val: u16) {}

        #[inline]
        pub fn write_u32(&mut self, _val: u32) {}

        #[inline]
        pub fn write_u64(&mut self, _val: u64) {}

        #[inline]
        pub fn write_bytes(&mut self, _bytes: &[u8]) {}

        #[inline]
        pub fn finish(&self) -> u64 {
            0
        }
    }

    impl Default for Xxh64Hasher {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Hash guest memory using XXH64 (disabled in cargo builds).
    pub fn hash_guest_memory(_memory: &[u8]) -> u64 {
        0
    }
}

// Kernel build: Linux kernel xxhash via C FFI.

#[cfg(not(feature = "cargo"))]
mod kernel_impl {
    /// XXH64 streaming hasher using the Linux kernel's xxhash implementation.
    pub struct Xxh64Hasher {
        state: crate::c_helpers::Xxh64State,
    }

    impl Xxh64Hasher {
        /// Create a new hasher with seed 0.
        pub fn new() -> Self {
            let mut state = crate::c_helpers::Xxh64State {
                total_len: 0,
                v1: 0,
                v2: 0,
                v3: 0,
                v4: 0,
                mem64: [0; 4],
                memsize: 0,
            };
            // SAFETY: state is a valid Xxh64State struct initialized above.
            // bedrock_xxh64_reset only writes to the pointed-to state.
            unsafe {
                crate::c_helpers::bedrock_xxh64_reset(&mut state, 0);
            }
            Self { state }
        }

        #[inline]
        pub fn write_u8(&mut self, byte: u8) {
            self.write_bytes(&[byte]);
        }

        #[inline]
        pub fn write_u16(&mut self, val: u16) {
            self.write_bytes(&val.to_le_bytes());
        }

        #[inline]
        pub fn write_u32(&mut self, val: u32) {
            self.write_bytes(&val.to_le_bytes());
        }

        #[inline]
        pub fn write_u64(&mut self, val: u64) {
            self.write_bytes(&val.to_le_bytes());
        }

        #[inline]
        pub fn write_bytes(&mut self, bytes: &[u8]) {
            // SAFETY: self.state is a valid initialized Xxh64State. bytes.as_ptr()
            // points to a valid byte slice of the given length.
            unsafe {
                crate::c_helpers::bedrock_xxh64_update(
                    &mut self.state,
                    bytes.as_ptr().cast::<core::ffi::c_void>(),
                    bytes.len(),
                );
            }
        }

        #[inline]
        pub fn finish(&self) -> u64 {
            // SAFETY: self.state is a valid initialized Xxh64State that has been
            // properly updated through write_* calls.
            unsafe { crate::c_helpers::bedrock_xxh64_digest(&self.state) }
        }
    }

    impl Default for Xxh64Hasher {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Hash guest memory using XXH64.
    pub fn hash_guest_memory(memory: &[u8]) -> u64 {
        // SAFETY: memory.as_ptr() points to a valid byte slice of the given length.
        // bedrock_xxh64 only reads from the provided buffer.
        unsafe {
            crate::c_helpers::bedrock_xxh64(
                memory.as_ptr().cast::<core::ffi::c_void>(),
                memory.len(),
                0,
            )
        }
    }
}

#[cfg(feature = "cargo")]
pub use cargo_impl::*;
#[cfg(not(feature = "cargo"))]
pub use kernel_impl::*;

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
