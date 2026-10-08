// SPDX-License-Identifier: GPL-2.0

//! Independent reference for a contract's storage root, used by the E5 oracle.
//!
//! Rebuilds the secure Merkle-Patricia trie from scratch: key
//! `keccak256(bytes32(slot))`, value `rlp(minimal big-endian value)`, zero
//! values absent. Deliberately shares no code with reth's trie crates so a bug
//! there cannot hide in the reference.

use tiny_keccak::{Hasher, Keccak};

/// `(slot, value)` pairs as 32-byte big-endian words.
pub type Storage = Vec<([u8; 32], [u8; 32])>;

pub fn keccak(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut k = Keccak::v256();
    k.update(data);
    k.finalize(&mut out);
    out
}

enum Rlp {
    Bytes(Vec<u8>),
    List(Vec<Rlp>),
}

fn rlp_len(len: usize, offset: u8) -> Vec<u8> {
    if len < 56 {
        return vec![offset + len as u8];
    }
    let be = len.to_be_bytes();
    let be: Vec<u8> = be.iter().copied().skip_while(|b| *b == 0).collect();
    let mut out = vec![offset + 55 + be.len() as u8];
    out.extend(be);
    out
}

fn encode(item: &Rlp) -> Vec<u8> {
    match item {
        Rlp::Bytes(b) if b.len() == 1 && b[0] < 0x80 => b.clone(),
        Rlp::Bytes(b) => {
            let mut out = rlp_len(b.len(), 0x80);
            out.extend(b);
            out
        }
        Rlp::List(items) => {
            let payload: Vec<u8> = items.iter().flat_map(encode).collect();
            let mut out = rlp_len(payload.len(), 0xc0);
            out.extend(payload);
            out
        }
    }
}

/// Compact (hex-prefix) encoding of a nibble path.
fn hex_prefix(nibbles: &[u8], leaf: bool) -> Vec<u8> {
    let flag = if leaf { 2 } else { 0 };
    let mut n = if nibbles.len() % 2 == 1 {
        vec![flag + 1]
    } else {
        vec![flag, 0]
    };
    n.extend_from_slice(nibbles);
    n.chunks(2).map(|c| (c[0] << 4) | c[1]).collect()
}

/// A child reference: the node inline if its encoding is < 32 bytes,
/// otherwise its hash.
fn child_ref(node: Rlp) -> Rlp {
    let enc = encode(&node);
    if enc.len() < 32 {
        node
    } else {
        Rlp::Bytes(keccak(&enc).to_vec())
    }
}

fn build(items: &[(Vec<u8>, Vec<u8>)]) -> Rlp {
    if items.len() == 1 {
        let (path, value) = &items[0];
        return Rlp::List(vec![
            Rlp::Bytes(hex_prefix(path, true)),
            Rlp::Bytes(value.clone()),
        ]);
    }
    let first = &items[0].0;
    let common = (0..first.len())
        .take_while(|&i| items.iter().all(|(p, _)| p[i] == first[i]))
        .count();
    if common > 0 {
        let rest: Vec<_> = items
            .iter()
            .map(|(p, v)| (p[common..].to_vec(), v.clone()))
            .collect();
        return Rlp::List(vec![
            Rlp::Bytes(hex_prefix(&first[..common], false)),
            child_ref(build(&rest)),
        ]);
    }
    let mut children: Vec<Rlp> = (0..16u8)
        .map(|nibble| {
            let sub: Vec<_> = items
                .iter()
                .filter(|(p, _)| p[0] == nibble)
                .map(|(p, v)| (p[1..].to_vec(), v.clone()))
                .collect();
            if sub.is_empty() {
                Rlp::Bytes(Vec::new())
            } else {
                child_ref(build(&sub))
            }
        })
        .collect();
    // Branch value slot: always empty for fixed-length (32-byte) keys.
    children.push(Rlp::Bytes(Vec::new()));
    Rlp::List(children)
}

/// Storage root of `(slot, value)` pairs, slot and value as 32-byte
/// big-endian words. Zero values are absent from the trie.
pub fn storage_root(storage: &[([u8; 32], [u8; 32])]) -> [u8; 32] {
    let items: Vec<(Vec<u8>, Vec<u8>)> = storage
        .iter()
        .filter(|(_, v)| v.iter().any(|b| *b != 0))
        .map(|(slot, value)| {
            let path = keccak(slot)
                .iter()
                .flat_map(|b| [b >> 4, b & 0x0f])
                .collect();
            let minimal: Vec<u8> = value.iter().copied().skip_while(|b| *b == 0).collect();
            (path, encode(&Rlp::Bytes(minimal)))
        })
        .collect();
    if items.is_empty() {
        return keccak(&encode(&Rlp::Bytes(Vec::new())));
    }
    keccak(&encode(&build(&items)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(n: u64) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&n.to_be_bytes());
        w
    }

    fn root_hex(pairs: &[(u64, u64)]) -> String {
        let storage: Vec<_> = pairs.iter().map(|(s, v)| (word(*s), word(*v))).collect();
        format!("0x{}", hex(&storage_root(&storage)))
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn keccak_empty_vector() {
        assert_eq!(
            hex(&keccak(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn slot_paths_share_the_intended_prefixes() {
        let prefix = |s| hex(&keccak(&word(s)))[..3].to_string();
        assert_eq!(prefix(544), "120");
        assert_eq!(prefix(646), "121");
        assert_eq!(&prefix(131)[..2], "13");
        assert_eq!(&prefix(0)[..1], "2");
    }

    /// storageHash values reported by a Tempo dev node (reth 42fa3c5) for
    /// RawStorage blocks 21..29 running the collapse_abcd sequence.
    #[test]
    fn matches_node_roots_through_collapse_sequence() {
        let cases: &[(&[(u64, u64)], &str)] = &[
            (
                &[(544, 1)],
                "0x9c1bc8c6765b46c895c98d38aa670890e839c5d554c818d13cd0a61f2199193f",
            ),
            (
                &[(544, 1), (646, 1)],
                "0xdad20cc093ebc92d483c79fc4d80d6d10a91197dab5ab6314d9c52815f8d8ce7",
            ),
            (
                &[(544, 1), (646, 1), (131, 1)],
                "0x011bab17dd93212035bf2de0c571dead56386d42b3b4ed77b19b5d3dcdd7afed",
            ),
            (
                &[(544, 1), (646, 1), (131, 1), (0, 1)],
                "0x1daa48d84592389ab7fdbce646b4928231c028856284fda4bc04e4ad713c1bfc",
            ),
            (
                &[(544, 2), (646, 1), (131, 1), (0, 1)],
                "0x4c910ca842fa9193d277bb3c3ed573b7db52a6742ee703c14621ecf4d149670c",
            ),
            (
                &[(544, 2), (646, 1), (131, 1)],
                "0x669b39a03372fa136716aefbca5f41fd480a087c19dfbc455d81d799e8f56ae4",
            ),
            (
                &[(544, 2), (646, 1)],
                "0x8350eef39e704e8c7d0c7edc83173c39128ac37b1813b7f4669742a5de36b624",
            ),
            (
                &[(544, 2)],
                "0x138f24efcf384c1626fd3b56c309a77f467b63cdd40cc62a2de4641443895d07",
            ),
            (
                &[],
                "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            ),
        ];
        for (pairs, want) in cases {
            assert_eq!(&root_hex(pairs), want, "storage {pairs:?}");
        }
    }

    #[test]
    fn zero_values_are_absent() {
        assert_eq!(root_hex(&[(544, 1), (646, 0)]), root_hex(&[(544, 1)]));
        assert_eq!(root_hex(&[(0, 0)]), root_hex(&[]));
    }

    #[test]
    fn order_independent() {
        assert_eq!(
            root_hex(&[(0, 7), (131, 7), (646, 7)]),
            "0x5dc82150a47e75b4c75a9636ce883582e517b4451ec6f17520c433a6cd4648fe"
        );
        assert_eq!(
            root_hex(&[(646, 7), (0, 7), (131, 7)]),
            root_hex(&[(0, 7), (131, 7), (646, 7)])
        );
    }
}
