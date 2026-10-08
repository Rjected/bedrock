// SPDX-License-Identifier: GPL-2.0

//! Storage-trie references for the E5 and E6 oracles, on alloy-trie.
//!
//! - [`storage_root`]: a contract's storage root rebuilt from scratch from its
//!   slot values (`HashBuilder` over the sorted hashed slots). The node computes
//!   roots incrementally (sparse trie, persisted trie updates), so a from-scratch
//!   build is an independent path even though reth also links alloy-trie.
//! - [`verify`]: an `eth_getProof` response checked against the block's state
//!   root: the account proof, then each storage proof against `storageHash`.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256, U64};
use alloy_trie::{proof::verify_proof, root::storage_root_unhashed, Nibbles, TrieAccount};
use serde::Deserialize;

/// `(slot, value)` pairs.
pub type Storage = Vec<(u64, U256)>;

/// Storage root of `storage`; zero values are absent from the trie.
pub fn storage_root(storage: &[(u64, U256)]) -> B256 {
    storage_root_unhashed(
        storage
            .iter()
            .filter(|(_, v)| !v.is_zero())
            .map(|(slot, v)| (B256::from(U256::from(*slot)), *v)),
    )
}

/// `eth_getProof` response (EIP-1186).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProof {
    pub address: Address,
    pub nonce: U64,
    pub balance: U256,
    pub code_hash: B256,
    pub storage_hash: B256,
    pub account_proof: Vec<Bytes>,
    pub storage_proof: Vec<StorageProof>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StorageProof {
    pub key: U256,
    pub value: U256,
    pub proof: Vec<Bytes>,
}

#[derive(Debug, PartialEq)]
pub enum ProofError {
    /// The account proof does not prove the reported account fields.
    Account(String),
    /// A storage proof does not prove the reported value under `storageHash`.
    Storage { slot: U256, error: String },
}

/// Checks `proof` against `state_root`. An account with all-default fields
/// must be proven absent.
pub fn verify(proof: &AccountProof, state_root: B256) -> Result<(), ProofError> {
    let account = TrieAccount {
        nonce: proof.nonce.to(),
        balance: proof.balance,
        storage_root: proof.storage_hash,
        code_hash: proof.code_hash,
    };
    let expected = (account != TrieAccount::default()).then(|| alloy_rlp::encode(account));
    verify_proof(
        state_root,
        Nibbles::unpack(keccak256(proof.address)),
        expected,
        &proof.account_proof,
    )
    .map_err(|e| ProofError::Account(e.to_string()))?;
    for sp in &proof.storage_proof {
        let expected = (!sp.value.is_zero()).then(|| alloy_rlp::encode(sp.value));
        verify_proof(
            proof.storage_hash,
            Nibbles::unpack(keccak256(B256::from(sp.key))),
            expected,
            &sp.proof,
        )
        .map_err(|e| ProofError::Storage {
            slot: sp.key,
            error: e.to_string(),
        })?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use alloy_trie::{proof::ProofRetainer, HashBuilder};
    use serde_json::Value;

    fn root_hex(pairs: &[(u64, u64)]) -> String {
        let storage: Vec<_> = pairs.iter().map(|(s, v)| (*s, U256::from(*v))).collect();
        storage_root(&storage).to_string()
    }

    /// Proof of `nodes`' leaves, retained for `targets`, as `(root, proof per target)`.
    fn build(leaves: &[(B256, Vec<u8>)], targets: &[B256]) -> (B256, Vec<Vec<Bytes>>) {
        let mut leaves = leaves.to_vec();
        leaves.sort();
        let retainer = ProofRetainer::from_iter(targets.iter().map(|t| Nibbles::unpack(*t)));
        let mut hb = HashBuilder::default().with_proof_retainer(retainer);
        for (key, value) in &leaves {
            hb.add_leaf(Nibbles::unpack(*key), value);
        }
        let root = hb.root();
        let nodes = hb.take_proof_nodes();
        let proofs = targets
            .iter()
            .map(|t| {
                nodes
                    .matching_nodes_sorted(&Nibbles::unpack(*t))
                    .into_iter()
                    .map(|(_, n)| n)
                    .collect()
            })
            .collect();
        (root, proofs)
    }

    /// An honest node's `(state_root, eth_getProof)` for a contract at
    /// `address` holding `storage`, proving `slots`.
    pub fn honest_proof(
        address: &str,
        storage: &[(u64, u64)],
        slots: &[u64],
    ) -> (B256, AccountProof) {
        let address: Address = address.parse().unwrap();
        let value = |s: u64| storage.iter().find(|(k, _)| *k == s).map_or(0, |(_, v)| *v);
        let leaves: Vec<_> = storage
            .iter()
            .filter(|(_, v)| *v != 0)
            .map(|(s, v)| {
                (
                    keccak256(B256::from(U256::from(*s))),
                    alloy_rlp::encode(U256::from(*v)),
                )
            })
            .collect();
        let targets: Vec<_> = slots
            .iter()
            .map(|s| keccak256(B256::from(U256::from(*s))))
            .collect();
        let (storage_hash, storage_proofs) = build(&leaves, &targets);
        let account = TrieAccount {
            nonce: 1,
            balance: U256::ZERO,
            storage_root: storage_hash,
            code_hash: keccak256(b"code"),
        };
        let key = keccak256(address);
        let (state_root, mut account_proofs) = build(&[(key, alloy_rlp::encode(account))], &[key]);
        let proof = AccountProof {
            address,
            nonce: U64::from(1),
            balance: U256::ZERO,
            code_hash: account.code_hash,
            storage_hash,
            account_proof: account_proofs.remove(0),
            storage_proof: slots
                .iter()
                .zip(storage_proofs)
                .map(|(s, proof)| StorageProof {
                    key: U256::from(*s),
                    value: U256::from(value(*s)),
                    proof,
                })
                .collect(),
        };
        (state_root, proof)
    }

    /// Real `eth_getProof` responses from a Tempo dev node (reth 42fa3c5)
    /// for the RawStorage contract, with the block's header `stateRoot`.
    fn fixture(block: u64) -> (B256, AccountProof, Vec<U256>) {
        let path = format!(
            "{}/testdata/proof_block{block}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        (
            v["state_root"].as_str().unwrap().parse().unwrap(),
            serde_json::from_value(v["proof"].clone()).unwrap(),
            serde_json::from_value(v["storage_at"].clone()).unwrap(),
        )
    }

    #[test]
    fn slot_paths_share_the_intended_prefixes() {
        let prefix = |s: u64| keccak256(B256::from(U256::from(s))).to_string()[2..5].to_string();
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
            root_hex(&[(646, 7), (0, 7), (131, 7)]),
            "0x5dc82150a47e75b4c75a9636ce883582e517b4451ec6f17520c433a6cd4648fe"
        );
    }

    /// Blocks 20 (empty storage), 21 (A only: three absence proofs) and 24
    /// (A, B, C, D live).
    #[test]
    fn node_proofs_verify() {
        for block in [20, 21, 24] {
            let (state_root, proof, storage_at) = fixture(block);
            assert_eq!(verify(&proof, state_root), Ok(()), "block {block}");
            let values: Vec<_> = proof.storage_proof.iter().map(|p| p.value).collect();
            assert_eq!(values, storage_at, "block {block}");
            let storage: Vec<_> = [544, 646, 131, 0].into_iter().zip(storage_at).collect();
            assert_eq!(storage_root(&storage), proof.storage_hash, "block {block}");
        }
    }

    #[test]
    fn tampered_node_proofs_fail() {
        let (state_root, proof, _) = fixture(24);
        assert!(matches!(
            verify(&proof, B256::ZERO),
            Err(ProofError::Account(_))
        ));

        let mut p = proof.clone();
        p.storage_hash = fixture(21).1.storage_hash;
        assert!(matches!(
            verify(&p, state_root),
            Err(ProofError::Account(_))
        ));

        let mut p = proof.clone();
        p.storage_proof[0].value = U256::from(2);
        assert!(matches!(
            verify(&p, state_root),
            Err(ProofError::Storage { .. })
        ));

        // Claiming a live slot is absent.
        let mut p = proof.clone();
        p.storage_proof[3].value = U256::ZERO;
        assert!(matches!(
            verify(&p, state_root),
            Err(ProofError::Storage { .. })
        ));

        // A storage proof with its leaf dropped.
        let mut p = proof.clone();
        p.storage_proof[1].proof.pop();
        assert!(matches!(
            verify(&p, state_root),
            Err(ProofError::Storage { .. })
        ));

        // Claiming an absent slot is live (block 21: only A is live).
        let (state_root, mut p, _) = fixture(21);
        p.storage_proof[1].value = U256::from(1);
        assert!(matches!(
            verify(&p, state_root),
            Err(ProofError::Storage { .. })
        ));
    }

    #[test]
    fn honest_proofs_verify() {
        let addr = "0x5fbdb2315678afecb367f032d93f642f64180aa3";
        for storage in [
            &[][..],
            &[(544, 1)],
            &[(544, 1), (646, 3), (131, 1), (0, 9)],
        ] {
            let (root, p) = honest_proof(addr, storage, &[544, 646, 131, 0]);
            assert_eq!(verify(&p, root), Ok(()), "storage {storage:?}");
        }
    }
}
