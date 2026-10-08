# Tempo DST: finding reth bugs under Bedrock

Run one Tempo dev node (reth-based) inside a Bedrock guest and perturb it
deterministically. One boot is shared by every seed; each seed then runs as its
own branch from a warm checkpoint, with different perturbations:

- **Thread schedules**: the node runs under `thread-fuzz` (sched_ext
  concurrency-fuzz), so the schedule is drawn from Bedrock's getrandom stream.
- **Crash/restart**: `tempo-dst nemesis` SIGKILLs and restarts the node at
  seed-derived times.
- **Load**: the pinned txgen image sends pathUSD transfers (fixed seed 99).

Every branch is a pure function of its seed, so a failure replays exactly:
`bedrock-dst replay <out>/seed-N`.

## Components

| Piece | Where | Role |
|---|---|---|
| `bedrock-dst` | `crates/bedrock-dst` | Host driver: boot, warm checkpoint, branch per seed, collect, verdict, replay |
| `tempo-dst` | `guest/tempo-dst` | In-guest nemesis and oracles, installed in the podman initrd |
| `workload-monitor` | `guest/workload-monitor` | Excuses container SIGKILL deaths that the nemesis logged |
| compose / run | `workloads/tempo-dst` | Node config, variants, campaign entry point |

Guest contract: assertions go to `/bedrock/assertions.jsonl` (serialized
`bedrock_assertions::Assertion`, message = `<signature>: <detail>`). Control
events go to `/bedrock/events.jsonl` (`{source, kind, container,
guest_time_ns, detail}`). The driver writes inputs to `/bedrock/in/config.json`.

## Oracles

| Signature | Checks |
|---|---|
| `E1/panic`, `E1/*-mismatch`, `E1/trie-diff-*`, `E1/persistence-*`, `E1/engine-fatal`, `E1/bad-block`, ... | Node log line matching a reth failure message (pinned reth `42fa3c5`; see `LOG_PATTERNS` in `guest/tempo-dst/src/oracle.rs`) |
| `E1/error-log/<target>` | Any ERROR-level node log line. Noisy by design; triage by target |
| `E1/trie-diff-*` | `--engine.state-root-task-compare-updates`: sparse-trie task vs. regular state-root updates differ |
| `E2/head-stalled` | Head did not advance for `liveness_secs` while the node was up |
| `E3/finalized-block-changed`, `E3/head-below-finalized` | The block the node reported finalized (`eth_getBlockByNumber("finalized")`) before a kill keeps its hash after the restart, and the head gets back to it. Unfinalized blocks may legitimately be rebuilt: reth unwinds to its persisted state-trie frontier, which trails the `Saved range of blocks` frontier (observed: saved 318, unwound to 308, finalized 269) |
| `E5/storage-root-mismatch` | `--load trie`: at every block, the RawStorage contract's `storageHash` (`eth_getProof`) equals a root rebuilt from scratch from its slot values (`eth_getStorageAt`) with alloy-trie's `HashBuilder`, not the node's incremental trie |
| `E6/account-proof-invalid`, `E6/storage-proof-invalid`, `E6/proof-value-mismatch` | `--load trie`: that `eth_getProof` response verifies (`alloy_trie::proof::verify_proof`): the account proof against the header `stateRoot`, each slot's proof against `storageHash`, and the proven values equal `eth_getStorageAt` |
| `E4/graceful-stop`, `E4/re-execute` | At the end of the run, the node stops cleanly, and `tempo re-execute` over `[1, head]` from its datadir agrees |
| `container <name> exit code is zero` | workload-monitor: no unexplained container death |
| `D/guest-exited` | The guest VM stopped mid-run (kernel panic, shutdown) |
| `S/kill`, `S/recovered`, `S/rewound-unfinalized`, `S/re-executed`, `S/load-included`, `S/trie-checked`, `S/trie-all-slots-live` | Coverage (Sometimes): the fault, crash-recovery unwind, recovery, and load paths actually ran |
| `C/missing/<signature>` | Required coverage never satisfied: `S/load-included` always, plus `S/trie-checked` and `S/trie-all-slots-live` with `--load trie`. A run whose load never landed proves nothing |

## Trie load (`--load trie`)

`trie/RawStorage.sol` writes exactly `sstore(slot, value)`, so its storage trie
is shaped only by the workload. `trie/trie.yaml` (txgen) deploys it from dev
account 0 (address `0x5FbDB2315678afecb367f032d93F642f64180aa3`) and runs
insert/update/delete sequences on raw slots whose Keccak paths share prefixes:
A = 544 (`120…`), B = 646 (`121…`), C = 131 (`13…`), D = 0 (`2…`).

| Sequence | Steps |
|---|---|
| `collapse_abcd` | A, B, C, D inserted (leaf → extension splits → root branch), A updated, then D, C, B, A deleted (branch → extension → leaf → empty) |
| `split_reverse` | D, C, B, A inserted, then deleted in insertion order |
| `delete_reinsert` | A/B branch collapses to a leaf, then re-splits |
| `noop_writes` | zero-writes to empty slots, same-value rewrites |

Every sequence starts and ends empty. One sender keeps steps in nonce order,
and ~5 tx/s puts about one step in each 200 ms block, so each block is one
trie mutation. Rebuild `raw-storage.json` with
`solc --combined-json abi,bin --optimize RawStorage.sol` (0.8.33).

## Running

On a bare-metal Intel host with EPT-friendly PEBS and `bedrock.ko` loaded
(`/dev/bedrock`). Nested VMs don't work: KVM hides `PEBS_BASELINE` from guests,
and without precise exits guests that spin in user code (e.g. podman's Go
runtime) never receive timer interrupts.

```sh
DOCKER='sudo docker' ./workloads/tempo-dst/build.sh
./workloads/tempo-dst/run.sh --seeds 20 --run-secs 180 --out dst-out
./workloads/tempo-dst/run.sh --variant no-prewarm --seeds 20 --out dst-out-noprewarm
./workloads/tempo-dst/run.sh --load trie --seeds 20 --out dst-out-trie
bedrock-dst replay dst-out/seed-3
```

Results: `dst-out/summary.json`, plus a `verdict.json` per seed.

## Task board

Ticked items exist in code and have unit tests. Claim a task by opening a
draft PR that names it.

### Prerequisites
- [x] **P1** Bare-metal host with a Rust-enabled 6.18 kernel and EPT-friendly PEBS
- [ ] **P2** First end-to-end smoke run: `run.sh --seeds 2 --run-secs 120`, then fix whatever breaks

### A: Driver (`crates/bedrock-dst`)
- [x] **A1** Boot, wait for `--warm-blocks`, warm checkpoint
- [x] **A2** Branch per seed, reseeding the in-VM PRNG (`Branch::reseed_rng`); deliver config; start oracle, nemesis, load; finalize; collect
- [x] **A3** Per-seed artifacts and `verdict.json` (Always failures deduped by signature, Sometimes coverage)
- [x] **A4** `replay <out>/seed-N` compares assertions, events, finalize and verdict byte-for-byte
- [ ] **A4b** Also compare exit-record streams (reuse `compare-traces.py`) for divergence localization
- [ ] **A5** Parallel seeds. A `Branch` is single-driver: shard seeds across processes, each booting its own prefix, or add multi-branch driving. Watch fork memory (oss-garage/bedrock#28)

### B: Crash faults
- [ ] **B1** Verify on host that the `tempo-data` volume survives `podman kill` + `start`
- [x] **B2** `tempo-dst nemesis`: seed-derived kill/restart plan, logged before each kill
- [x] **B3** Kill and restart events in `events.jsonl`
- [x] **B4** workload-monitor excuses nemesis SIGKILLs; panics and unexplained kills are still asserted
- [ ] **B5** Pause/resume (`podman pause`) and kill-during-persistence targeting (kill right after a `Saving range of blocks` line)

### C: Schedules
- [x] **C1** Node wrapped in `thread-fuzz` (compose `entrypoint`)
- [ ] **C2** Measure perturbation: across seeds, how much do block timing and tx inclusion actually vary? Decides whether oss-garage/bedrock#20 is needed
- [ ] **C3** Confirm `replay` is identical with thread-fuzz on

### D: Workload
- [x] **D0** txgen load per branch (pathUSD transfers; load only, not an oracle)
- [ ] **D1** Spike: tuner (`tempoxyz/tuner`) `StructureTxGenerator` → serializable tx program
- [ ] **D2** Genesis/fixture mapping: tuner fixture EOAs to dev accounts (mnemonic `test … junk`)
- [ ] **D3** Lowering + in-guest submitter: resolve nonces at submit time, sign Tempo AA and EVM envelopes, record accepted/rejected/unknown
- [x] **D4** Trie-shaping load (`--load trie`): raw-storage insert/update/delete on slots 544, 646, 131, 0, one step per block, checked by E5 and E6
- [ ] **D5** Coverage-guided mutation with tuner's mutator (needs G3)

### E: Oracles (`guest/tempo-dst`)
- [x] **E1** Log scanner over `journalctl CONTAINER_NAME=tempo` (survives restarts)
- [x] **E2** Liveness
- [x] **E3** Durability of finalized blocks across crash/restart (the `Saved range` frontier is not durable: reth unwinds to its state-trie frontier)
- [x] **E4** Graceful stop + `tempo re-execute`. Verify on host that `--chain dev` matches the dev node's chain spec (override with `TEMPO_DST_CHAIN`)
- [ ] **E4b** Independent state-root check: rebuild the trie from the final state, separate from the sparse trie
- [ ] **E5** Triage: tune `E1/error-log/*` noise from real runs and promote recurring targets to named signatures

### F: Negative controls (one per oracle, env-gated patches on Tempo `d3f3b28f`)
- [ ] **F1** Panic in a sparse-trie update → `E1/panic`
- [ ] **F2** Skip a persistence commit → `E3/*`
- [ ] **F3** Corrupt a receipt or state root that the producing node accepts → `E4/re-execute`
- [ ] **F4** Stall the engine after a restart → `E2/head-stalled`

### G: Variants and feedback
- [x] **G1** `run.sh --variant default|no-prewarm|parallel`
- [ ] **G1b** `bedrock_ncpus=1` vs 5 campaigns (pass `--cmdline`)
- [ ] **G2** Point the harness at upstream `reth --dev` so findings can go upstream directly
- [ ] **G3** sancov-instrumented Tempo build + `libpcguard` coverage collection per seed

**Milestone:** a campaign of thousands of seeds per day on one host; F1 to F4
are each caught; every failing seed replays exactly.
