// SPDX-License-Identifier: GPL-2.0

//! Bitmap-based VPID allocation with recycling. VPID 0 is reserved for VMX
//! root operation and never handed out (SDM Vol 3C §30.1).

use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

/// 65536 VPIDs / 64 bits per word (8KB).
const BITMAP_WORDS: usize = 1024;

/// Bitmap of in-use VPIDs. Bit i in word w represents VPID (w * 64 + i).
static VPID_BITMAP: VpidBitmap = VpidBitmap::new();

/// Where to start searching for a free VPID.
static SEARCH_HINT: AtomicU16 = AtomicU16::new(1);

struct VpidBitmap {
    words: [AtomicU64; BITMAP_WORDS],
}

impl VpidBitmap {
    const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const INIT_WORD: AtomicU64 = AtomicU64::new(0);
        Self {
            words: [INIT_WORD; BITMAP_WORDS],
        }
    }

    /// Try to allocate the specified VPID. Returns true if successful.
    fn try_allocate(&self, vpid: u16) -> bool {
        let word_idx = (vpid / 64) as usize;
        let bit_idx = vpid % 64;
        let mask = 1u64 << bit_idx;

        let old = self.words[word_idx].fetch_or(mask, Ordering::AcqRel);
        (old & mask) == 0
    }

    fn deallocate(&self, vpid: u16) {
        if vpid == 0 {
            return;
        }
        let word_idx = (vpid / 64) as usize;
        let bit_idx = vpid % 64;
        let mask = 1u64 << bit_idx;

        self.words[word_idx].fetch_and(!mask, Ordering::Release);
    }

    /// Allocate a free VPID, searching from `hint` and wrapping around.
    fn allocate_any(&self, hint: u16) -> Option<u16> {
        let start_word = (hint / 64) as usize;

        for word_idx in start_word..BITMAP_WORDS {
            if let Some(vpid) = self.try_allocate_in_word(word_idx) {
                return Some(vpid);
            }
        }

        for word_idx in 0..start_word {
            if let Some(vpid) = self.try_allocate_in_word(word_idx) {
                return Some(vpid);
            }
        }

        None
    }

    fn try_allocate_in_word(&self, word_idx: usize) -> Option<u16> {
        loop {
            let word = self.words[word_idx].load(Ordering::Acquire);
            if word == u64::MAX {
                return None;
            }

            let bit_idx = (!word).trailing_zeros() as u16;
            let vpid = (word_idx as u16) * 64 + bit_idx;

            // VPID 0 is reserved: mark it used and retry.
            if vpid == 0 {
                let mask = 1u64;
                self.words[0].fetch_or(mask, Ordering::AcqRel);
                continue;
            }

            if self.try_allocate(vpid) {
                return Some(vpid);
            }
            // Lost the race for this VPID; retry.
        }
    }

    /// Reset the bitmap (for testing/module reload); VPID 0 stays reserved.
    fn reset(&self) {
        for word in &self.words {
            word.store(0, Ordering::Release);
        }
        self.words[0].store(1, Ordering::Release);
    }
}

/// Allocate a unique nonzero VPID for a new VM. Thread-safe.
///
/// # Panics
///
/// Panics if all 65535 VPIDs are in use.
pub fn allocate_vpid() -> u16 {
    let hint = SEARCH_HINT.load(Ordering::Relaxed);

    match VPID_BITMAP.allocate_any(hint) {
        Some(vpid) => {
            SEARCH_HINT.store(vpid.wrapping_add(1), Ordering::Relaxed);
            vpid
        }
        None => panic!("VPID allocation failed: all 65535 VPIDs are in use"),
    }
}

/// Return a (nonzero) VPID to the pool when its VM is dropped.
pub fn deallocate_vpid(vpid: u16) {
    VPID_BITMAP.deallocate(vpid);

    let current_hint = SEARCH_HINT.load(Ordering::Relaxed);
    if vpid < current_hint {
        SEARCH_HINT.store(vpid, Ordering::Relaxed);
    }
}

/// Reset the allocator; only for module unload/reload.
///
/// # Safety
///
/// No VM may still be using an allocated VPID.
pub fn reset_vpid_counter() {
    VPID_BITMAP.reset();
    SEARCH_HINT.store(1, Ordering::Relaxed);
}

/// Likely next VPID (debug/test only; racy under concurrent allocation).
pub fn peek_next_vpid() -> u16 {
    SEARCH_HINT.load(Ordering::Relaxed)
}

/// Number of allocated VPIDs (debug/test only; O(n)).
pub fn count_allocated_vpids() -> usize {
    let mut count = 0;
    for word in &VPID_BITMAP.words {
        count += word.load(Ordering::Relaxed).count_ones() as usize;
    }
    count
}

#[cfg(test)]
#[path = "vpid_tests.rs"]
mod tests;
