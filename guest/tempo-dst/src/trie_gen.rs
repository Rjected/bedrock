// SPDX-License-Identifier: GPL-2.0

//! Generated storage-trie load: a pure function of the run seed.
//!
//! [`slots`] picks the RawStorage slots a run writes so that their trie paths
//! (`keccak256(bytes32(slot))`) form a random shape: an anchor slot, a stack of
//! slots sharing 1, 2, 3, ... leading nibbles with it (nested branches and
//! extensions), sometimes a pair sharing 7+ nibbles (leaves short enough to be
//! inlined in their parent), and a few unrelated slots. [`spec`] writes a txgen
//! spec of random inserts, updates and deletes over those slots, with values
//! from every encoding class and no-op holds of random length, so changes land
//! at every distance from the node's persistence cycles. Nothing here names a
//! particular bug; per-seed knobs (stack depth, value mix, holds) vary the mix
//! swarm-style.

use alloy_primitives::{keccak256, B256, U256};
use rand::{rngs::StdRng, Rng, SeedableRng};

/// Slots considered: enough keccak paths that 4-nibble stacks always exist
/// and 7-nibble pairs usually do.
const SEARCH: u64 = 1 << 17;
/// Written with zero for no-op holds; never written non-zero.
pub const NOOP_SLOT: u64 = 1 << 40;
/// RawStorage's CREATE address (deployed by `trie/deploy.yaml`).
const CONTRACT: &str = "0x5FbDB2315678afecb367f032d93F642f64180aa3";

const SHAPE_SALT: u64 = 0x7472_6965_7368_6170;
const OPS_SALT: u64 = 0x7472_6965_6f70_7300;

fn path(slot: u64) -> [u8; 32] {
    keccak256(B256::from(U256::from(slot))).0
}

/// Leading nibbles `a` and `b` share.
fn shared_nibbles(a: &[u8; 32], b: &[u8; 32]) -> usize {
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x != y {
            return 2 * i + usize::from(x >> 4 == y >> 4);
        }
    }
    64
}

/// The run's slots, sorted.
pub fn slots(seed: u64) -> Vec<u64> {
    let mut rng = StdRng::seed_from_u64(seed ^ SHAPE_SALT);
    let paths: Vec<[u8; 32]> = (0..SEARCH).map(path).collect();
    let anchor = rng.random_range(0..SEARCH);
    let mut out = vec![anchor];
    // Stack: for each level, 1-2 slots sharing exactly `level` nibbles with
    // the anchor, i.e. siblings of the anchor's path at that depth.
    let depth = rng.random_range(1..=5);
    for level in 1..=depth {
        let candidates: Vec<u64> = (0..SEARCH)
            .filter(|s| !out.contains(s))
            .filter(|s| shared_nibbles(&paths[anchor as usize], &paths[*s as usize]) == level)
            .collect();
        for _ in 0..rng.random_range(1..=2) {
            if !candidates.is_empty() {
                let s = candidates[rng.random_range(0..candidates.len())];
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    // Long shared prefix: short leaves, inlined in their parent when small.
    if rng.random_bool(0.5) {
        let mut by_prefix = std::collections::HashMap::new();
        let mut pairs = Vec::new();
        for s in 0..SEARCH {
            let p = &paths[s as usize];
            let key = (u32::from_be_bytes([p[0], p[1], p[2], p[3]])) >> 4;
            if let Some(prev) = by_prefix.insert(key, s) {
                pairs.push((prev, s));
            }
        }
        if !pairs.is_empty() {
            let (a, b) = pairs[rng.random_range(0..pairs.len())];
            for s in [a, b] {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    for _ in 0..rng.random_range(0..=3) {
        let s = rng.random_range(0..SEARCH);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out.sort_unstable();
    out
}

/// Per-seed operation mix.
struct Knobs {
    /// Weights of delete, small (< 0x80: inline-sized leaves), u64 and full
    /// 256-bit values.
    values: [u32; 4],
    /// Chance a step is a no-op hold, and the longest hold in steps.
    hold: f64,
    max_hold: usize,
}

fn value(rng: &mut StdRng, knobs: &Knobs) -> String {
    let total: u32 = knobs.values.iter().sum();
    let mut pick = rng.random_range(0..total);
    let mut class = 0;
    while pick >= knobs.values[class] {
        pick -= knobs.values[class];
        class += 1;
    }
    match class {
        0 => "0".into(),
        1 => rng.random_range(1..0x80u64).to_string(),
        2 => rng.random_range(0x80..=u64::MAX).to_string(),
        _ => format!("0x{}", hex(&rng.random::<[u8; 32]>())),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// txgen spec for load generation `generation` (0 at branch start, one more
/// per node restart): `steps` RawStorage writes over `slots`, in one sequence
/// so a single sender keeps them in nonce order.
pub fn spec(seed: u64, generation: u64, slots: &[u64], steps: usize) -> String {
    let mut knob_rng = StdRng::seed_from_u64(seed ^ OPS_SALT);
    let knobs = Knobs {
        values: [
            knob_rng.random_range(1..=4),
            knob_rng.random_range(0..=4),
            knob_rng.random_range(0..=4),
            knob_rng.random_range(0..=4),
        ],
        hold: [0.0, 0.05, 0.2][knob_rng.random_range(0..3)],
        max_hold: [4, 16, 64][knob_rng.random_range(0..3)],
    };
    let mut rng =
        StdRng::seed_from_u64(seed ^ OPS_SALT ^ generation.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let mut writes: Vec<(u64, String)> = Vec::with_capacity(steps);
    while writes.len() < steps {
        if rng.random_bool(knobs.hold) {
            let n = rng
                .random_range(1..=knobs.max_hold)
                .min(steps - writes.len());
            writes.extend(std::iter::repeat_n((NOOP_SLOT, "0".to_string()), n));
        } else {
            let slot = slots[rng.random_range(0..slots.len())];
            writes.push((slot, value(&mut rng, &knobs)));
        }
    }
    let mut out = format!(
        "# Generated by tempo-dst (trie_gen) for seed {seed}, load generation {generation}.\n\
         chain_id: 1337\n\
         accounts:\n  trie:\n    mnemonic: \"test test test test test test test test test test test junk\"\n    range: [0, 1]\n\
         artifacts:\n  RawStorage:\n    abi: raw-storage.json\n    bytecode: raw-storage.json\n\
         templates:\n  set_slot:\n    type: tempo\n    from: {{ pool: trie, select: {{ index: 0 }} }}\n    gas_limit: 1000000\n\
         \x20   max_fee_per_gas: 100000000000\n    max_priority_fee_per_gas: 1000000000\n\
         \x20   fee_token: \"0x20c0000000000000000000000000000000000000\"\n\
         \x20   call:\n      abi: RawStorage\n      function: set\n      args: []\n\
         sequences:\n  writes:\n    steps:\n"
    );
    for (i, (slot, value)) in writes.iter().enumerate() {
        out.push_str(&format!(
            "      - {{ name: w{i}, template: set_slot, with: {{ call: {{ to: \"{CONTRACT}\", args: [{slot}, \"{value}\"] }} }} }}\n"
        ));
    }
    out.push_str("mix:\n  - { sequence: writes, weight: 1 }\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_nibbles_counts_half_bytes() {
        let a = [0x12u8; 32];
        let mut b = a;
        b[1] = 0x13;
        assert_eq!(shared_nibbles(&a, &b), 3);
        b[1] = 0x22;
        assert_eq!(shared_nibbles(&a, &b), 2);
        assert_eq!(shared_nibbles(&a, &a), 64);
    }

    #[test]
    fn slots_are_deterministic_and_share_prefixes() {
        for seed in 0..8 {
            let s = slots(seed);
            assert_eq!(s, slots(seed));
            assert!(s.len() >= 2 && s.len() <= 16, "{s:?}");
            // Some pair shares at least one nibble: there is a branch below
            // the root.
            let paths: Vec<_> = s.iter().map(|x| path(*x)).collect();
            let deepest = (0..s.len())
                .flat_map(|i| (i + 1..s.len()).map(move |j| (i, j)))
                .map(|(i, j)| shared_nibbles(&paths[i], &paths[j]))
                .max()
                .unwrap();
            assert!(deepest >= 1, "seed {seed}: {s:?}");
        }
        assert_ne!(slots(1), slots(2));
    }

    #[test]
    fn some_seeds_get_inline_sized_pairs() {
        let deep = (0..16)
            .filter(|seed| {
                let p: Vec<_> = slots(*seed).iter().map(|x| path(*x)).collect();
                (0..p.len()).any(|i| (i + 1..p.len()).any(|j| shared_nibbles(&p[i], &p[j]) >= 7))
            })
            .count();
        assert!(deep > 0);
    }

    #[test]
    fn spec_is_deterministic_per_generation() {
        let s = slots(3);
        let a = spec(3, 0, &s, 50);
        assert_eq!(a, spec(3, 0, &s, 50));
        assert_ne!(a, spec(3, 1, &s, 50));
        assert_eq!(a.matches("template: set_slot").count(), 50);
        assert!(a.contains("sequences:\n  writes:"));
    }
}
