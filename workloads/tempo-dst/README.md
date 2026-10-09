# Tempo DST: finding reth bugs under Bedrock

Run one Tempo dev node (reth-based) inside a Bedrock guest and perturb it
deterministically. One boot is shared by every seed; each seed then runs as its
own branch from a warm checkpoint, with different perturbations:

- **Thread schedules**: the node runs under `thread-fuzz` (sched_ext
  concurrency-fuzz), so the schedule is drawn from Bedrock's getrandom stream.
  A seccomp filter keeps every node thread in SCHED_EXT: reth's
  `deprioritize_background_threads` asks for SCHED_IDLE and gets EPERM, since a
  thread that left sched_ext would run in the fair class ahead of every other
  node thread and could starve them all. Set `THREAD_FUZZ_SECCOMP=0` in the
  node's environment to turn the filter off for debugging.
- **Crash/restart**: `tempo-dst nemesis` SIGKILLs and restarts the node at
  seed-derived times.
- **Load**: the pinned txgen image sends pathUSD transfers (fixed seed 99).
- **Forced preemption** (swarm): each seed draws a preemption period (off,
  200k, 2M or 20M guest instructions) from a host-side PRNG of the seed;
  the driver applies it to the branch (`Branch::set_preempt`) for every load,
  with or without `--reference`. `--no-preempt` turns it off and never calls
  the ioctl. A `bedrock.ko` without `SET_PREEMPT_CONFIG` is rejected before
  boot when any seed would use it.

Every branch is a pure function of its seed, so a failure replays exactly:
`bedrock-dst replay <out>/seed-N`. Each seed also records its inputs and
decisions, so it replays without re-deriving anything from the seed (see
[Trace replay](#trace-replay)). Each seed's swarm record
(`{"preempt": {"period", "seed"}, "load", "reference", "nemesis"}`) is under
`"swarm"` in its `config.json`, `verdict.json` and `summary.json` entry, so
failures can be grouped by feature (`regressions/hunt.sh` prints that table).

## Components

| Piece | Where | Role |
|---|---|---|
| `bedrock-dst` | `crates/bedrock-dst` | Host driver: boot, warm checkpoint, branch per seed, collect, verdict, replay |
| `tempo-dst` | `guest/tempo-dst` | In-guest nemesis and oracles, fetched into the guest at boot |
| `workload-monitor` | `guest/workload-monitor` | Forwards assertions and excuses container SIGKILL deaths that the nemesis logged |
| compose / run | `workloads/tempo-dst` | Node config, variants, campaign entry point |

Guest contract: each writer appends serialized `bedrock_assertions::Assertion`
records to its own `/bedrock/assertions/<writer>.jsonl` file. Records include a
`timestamp_unix_nano` and a message of `<signature>: <detail>`. Guest init
forwards complete lines to the serial log; at the end of each seed, the guest
merges all files by timestamp for the host's `assertions.jsonl` and verdict.
Control events go to `/bedrock/events.jsonl` (`{source, kind, container,
guest_time_ns, detail}`). The driver writes inputs to `/bedrock/in/config.json`.

## Oracles

| Signature | Checks |
|---|---|
| `E1/panic`, `E1/*-mismatch`, `E1/trie-diff-*`, `E1/persistence-*`, `E1/engine-fatal`, `E1/bad-block`, ... | Node log line matching a reth failure message (pinned reth `42fa3c5`; see `LOG_PATTERNS` in `guest/tempo-dst/src/oracle.rs`) |
| `E1/error-log/<target>` | Any ERROR-level node log line. Noisy by design; triage by target |
| `E1/trie-diff-*` | `--engine.state-root-task-compare-updates`: sparse-trie task vs. regular state-root updates differ |
| `E2/head-stalled` | Head did not advance for `liveness_secs` while the node was up |
| `E3/finalized-block-changed`, `E3/head-below-finalized` | The block the node reported finalized (`eth_getBlockByNumber("finalized")`) before a kill keeps its hash after the restart, and the head gets back to it. Unfinalized blocks may legitimately be rebuilt: reth unwinds to its persisted state-trie frontier, which trails the `Saved range of blocks` frontier (observed: saved 318, unwound to 308, finalized 269) |
| `E5/storage-root-mismatch`, `E5/{multiproof,untargeted}-storage-root-mismatch` | `--load trie`: at every checked block, the RawStorage contract's `storageHash` (`eth_getProof`, `eth_getProof` without targets, `eth_getMultiProof`) equals a root rebuilt from scratch from its slot values (`eth_getStorageAt`) with alloy-trie's `HashBuilder`, not the node's incremental trie |
| `E6/account-proof-invalid`, `E6/storage-proof-invalid`, `E6/proof-value-mismatch` (and `multiproof-`/`untargeted-` forms) | `--load trie`: each proof response verifies (`alloy_trie::proof::verify_proof`): the account proof against the header `stateRoot`, each slot's proof against `storageHash`, and the proven values equal `eth_getStorageAt` |
| `E8/tip20-supply-changed`, `E8/tip20-balances-not-conserved`, `E8/tip20-balance-exceeds-supply` | `--load tip20`: at every checked block, the token's `totalSupply` is the minted supply, the holders' `balanceOf` sum to it, and none exceeds it (all reads `eth_call` at that block's hash, EIP-1898) |
| `E8/tip20-transfers-unexplained` | `--load tip20`: a block's balances are exactly its parent's plus the token's `Transfer` logs in the block's receipts (`eth_getBlockReceipts`; reverted transfers must leave none): catches partially applied blocks and reads that mix two versions of the state |
| `E8/tip20-snapshot-changed` | `--load tip20`: pinned blocks' balances, re-read every tick while the block stays canonical (after more blocks, persistence of the block, node restarts), are unchanged |
| `E9/chain-link-broken` | `--load chain`: in canonical receipt order, each ChainOfBlocks `Appended` log has the next index and link `keccak(prev, payload, index)`: the node never executed an append against the wrong pre-state |
| `E9/chain-state-mismatch` | `--load chain`: at every checked block B, the stored `length`, `head` and `links[length-1]` (`eth_getStorageAt` at B) equal the chain rebuilt from the receipts of blocks `..=B`; one swept old link per tick at the head too |
| `E9/chain-shrunk` | `--load chain`: the stored length never decreases between checked canonical blocks |
| `E9/chain-not-prefix` | `--load chain`: the chain stored at an older block (depths 2-48 and 49-250, like E5) is a prefix of the chain stored at the head; state against state, no receipts |
| `E9/chain-entry-lost` | `--load chain`: after a restart, the head still holds the chain stored at the block finalized before the kill |
| `E9/chain-history-changed` | `--load chain`: re-reading a pinned finalized block's stored chain (one pin per kill, re-read round-robin) gives the value read when it was pinned, across persistence cycles and restarts |
| `E7/reference-<E1 pattern>`, `E7/reference-rejected` | `--reference`: the reference node logs one of E1's patterns (e.g. `E7/reference-bad-block`, `E7/reference-state-root-mismatch`) or rejects a block (`Invalid block error on new payload`, pipeline validation/execution error) the primary produced |
| `E7/reference-stalled` | `--reference`: the reference's head stays more than 32 blocks behind the primary's for `liveness_secs` while the primary is up (clock restarts at each primary restart) |
| `E7/block-hash-differs`, `E7/state-root-differs` | `--reference`: at the highest block both nodes have, they disagree for `liveness_secs` (a crash may rebuild unfinalized blocks; the reference must reorg onto them), named by whether the state roots differ too; or the same header hash comes with different `stateRoot`s |
| `E7/proof-differs` | `--reference --load trie`: on blocks both nodes agree on (new ones, plus E5/E6's depth sweep), RawStorage's `eth_getProof` `storageHash` and proven values, `eth_getMultiProof` `storageHash`, and `eth_getStorageAt` values are equal on both |
| `E7/reference-graceful-stop`, `E7/reference-repair-trie` | `--reference`: the reference stops cleanly and `tempo db repair-trie --dry-run` finds no trie inconsistencies in its datadir |
| `E4/graceful-stop`, `E4/repair-trie`, `E4/re-execute` | At the end of the run, the node stops cleanly, `tempo db repair-trie --dry-run` finds no trie inconsistencies in its datadir, and `tempo re-execute` over `[1, head]` agrees |
| `container <name> exit code is zero` | workload-monitor: no unexplained container death |
| `D/guest-exited` | The guest VM stopped mid-run (kernel panic, shutdown) |
| `S/kill`, `S/recovered`, `S/rewound-unfinalized`, `S/re-executed`, `S/load-included`, `S/trie-checked`, `S/trie-all-slots-live`, `S/chain-appended`, `S/chain-survived-restart`, `S/chain-history-reread` | Coverage (Sometimes): the fault, crash-recovery unwind, recovery, and load paths actually ran |
| `S/tip20-checked`, `S/tip20-transfers-in-block`, `S/tip20-pinned-read-survived-persistence`, `S/tip20-pinned-read-survived-restart` | Coverage for E8: a block passed; a block with 2+ transfers passed; a block pinned before it was persisted (newer than the last `Saved range of blocks`) read the same after a save covered it; a block pinned before a node restart read the same after it |
| `S/reference-compared`, `S/reference-synced`, `S/reference-followed-restart` | Coverage with `--reference`: a common block was compared, the reference reached the primary's head, and after a primary restart it followed past the pre-kill head |
| `C/missing/<signature>` | Required coverage never satisfied: `S/load-included` always, plus `S/trie-checked` with `--load trie`; with `--load tip20` all four `S/tip20-*` above; with `--load chain` `S/chain-appended` and `S/chain-survived-restart` (the `-survived-restart` signatures only when every seed's nemesis plan has a kill); plus `S/reference-compared` and `S/reference-synced` with `--reference`. A run whose load never landed proves nothing |

## Trie load (`--load trie`)

`trie/RawStorage.sol` writes exactly `sstore(slot, value)`, so its storage trie
is shaped only by the workload. `trie/deploy.yaml` deploys it from dev account 0
before the warm checkpoint (address `0x5FbDB2315678afecb367f032d93F642f64180aa3`).

Each run's writes are generated from its seed (`guest/tempo-dst/src/trie_gen.rs`,
`tempo-dst trie-spec <seed> <generation> <steps>` prints one), with no
hand-picked trigger for any particular bug:

- **Slots** form a random trie shape: an anchor, a stack of slots sharing 1, 2,
  3, ... leading keccak nibbles with it (nested branches and extensions),
  sometimes a pair sharing 7+ nibbles (leaves small enough to be inlined in
  their parent), and a few unrelated slots.
- **Writes** are random inserts, updates and deletes over those slots, with
  values from every encoding class (zero, < 0x80, u64, full 256-bit), and
  no-op holds of random length so changes land at every distance from the
  node's persistence cycles. Stack depth, the value mix and holds vary per seed.
- Each node restart starts a new load generation: a fresh write stream over the
  same slots, from nonces re-read from the node.

E5/E6 check every new block plus two older ones per tick (depths 2-48 and
49-250), with `eth_getProof`, `eth_getProof` without storage targets, and
`eth_getMultiProof`.

## TIP-20 load (`--load tip20`)

A bank-transfer workload: a closed set of holders of a fresh TIP-20 token
whose whole supply they hold, so transfers among them conserve it.
`tip20/deploy.yaml` runs before the warm checkpoint (`tempo-dst deploy-tip20`):
dev account 9 creates the token through the TIP20Factory precompile
(`createToken`, salt `0xb1d`, at `0x20C0000000000000000000008a34063c54ebdca3`),
grants itself `ISSUER_ROLE`, and mints `k * 10^24` to holder `k`, dev accounts
1-8 (supply `36 * 10^24`). Nothing mints or burns afterwards, and fees are paid
in pathUSD, so the token's supply never moves.

Each run's transfers are generated from its seed (`guest/tempo-dst/src/tip20.rs`,
`tempo-dst tip20-spec <seed> <generation> <steps>`): a per-seed subset of 2-8
holders sends, interleaved, so each block holds several concurrent transfers;
amounts mix zero, small, large and overdrawn (more than the supply: the
transfer reverts and must move nothing), with some self-transfers. Each node
restart starts a fresh transfer stream from nonces re-read from the node.

E8 (`Tip20Oracle`) reads every block by hash: the holders' balances and the
supply (conservation), checked against the parent's balances plus the block's
`Transfer` logs (one generation per block), plus the E5/E6 historical depth
sweep. It pins the run's first block and the latest few multiples of 25, and
re-reads them every tick; a pin whose block is rewound by crash recovery is
dropped (unfinalized blocks may be rebuilt; E3 covers finalized ones).

## Chain load (`--load chain`)

The [chain of blocks](https://antithesis.com/docs/resources/chain-of-blocks/)
workload, against one node's own history instead of replicas.
`chain/ChainOfBlocks.sol` keeps `(length, head)` and every link, with
`append(payload)` computing `links[i] = head = keccak256(head, payload, i)`
from its *stored* head, and logs each append. `chain/deploy.yaml` deploys it
from dev account 9 before the warm checkpoint (address
`0x700b6A60ce7EaaEA56F065753d8dcB9653dbAD35`) and appends entry 0.

- The writer is one sender in nonce order (`guest/tempo-dst/src/cob_gen.rs`,
  `tempo-dst chain-spec <seed> <generation> <steps>`), payloads
  `keccak(seed, generation, step)`. Linking in the contract means it never
  needs the on-chain head: after a crash drops the pool, the next generation
  extends whatever chain the node kept.
- E9 (`guest/tempo-dst/src/cob.rs`) rebuilds the chain from `eth_getLogs`
  (receipts) and recomputes every link from its payload, then holds the
  node's state to it at explicit block numbers. `head` commits to the whole
  history, so one slot read checks every earlier append; `length`/`head` are
  rewritten every append (changeset history), `links[i]` is written once
  (old, cold slots).
- Rebuilt unfinalized blocks are legal: when a synced block's hash changes,
  E9 drops the replaced blocks' appends and re-reads their logs.
- Outside Bedrock: `TEMPO_DST_RPC=http://127.0.0.1:8545 tempo-dst chain-check
  [block]` checks every append block once; `tempo-dst chain-watch <secs>` runs
  the stateful oracle and treats RPC loss/return as kill/restart.

## Reference node (`--reference`, E7)

`run.sh ... --reference` keeps compose.yaml's `# >>> reference` blocks: a
second node, `tempo-ref` (same image, RPC on 8547, own `tempo-ref-data`
volume), that re-executes every primary block on reth's simplest paths:
`--engine.disable-prewarming --engine.disable-precompile-cache
--engine.state-root-fallback --engine.disable-state-cache
--engine.num-state-masking-blocks 0 --engine.persistence-threshold 3`. It
runs plain `tempo` (no thread-fuzz) and the nemesis never kills it, so it is
the baseline the primary is compared against.

- It follows the primary with `--follow http://127.0.0.1:8545
  --follow.nocertify`, Tempo's switch for reth's `--debug.rpc-consensus-url`
  (fetch blocks over RPC, drive the engine with newPayload/FCU) that skips the
  commonware consensus stack. `--debug.rpc-consensus-url` alone fails outside
  `--dev`: Tempo then requires `--consensus.signing-key`. Chain: `--chain
  dev` (same genesis as `--dev`).
- The RPC follower only sees blocks produced while it polls. Blocks from
  before it started, or the ones a primary crash unwinds and rebuilds, come
  over p2p: the primary gets a fixed `--p2p-secret-key-hex` and the reference
  names it in `--trusted-peers` (discovery stays off). While the primary is
  down the follower logs `alloy_rpc_client::poller` errors, so ERROR lines on
  the reference are not findings; only E1's named patterns are.
- Verified outside Bedrock on the pinned image: a reference started 450
  blocks late backfills, then matches every header hash and state root; after
  a SIGKILL that made the primary unwind 637 to 627 and rebuild, the
  reference reorgs onto the rebuilt blocks within ~15 s; proofs match.

Off by default until validated in campaigns.

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
./workloads/tempo-dst/run.sh --load tip20 --seeds 20 --out dst-out-tip20
./workloads/tempo-dst/run.sh --load chain --seeds 20 --out dst-out-chain
./workloads/tempo-dst/run.sh --load trie --reference --seeds 20 --out dst-out-ref
bedrock-dst replay dst-out/seed-3
```

Results: `dst-out/summary.json`, plus a `verdict.json` per seed.

## Trace replay

A seed is a compressed run: a numeric seed only reproduces a run while the
code that turns seeds into inputs stays the same. Each seed directory
therefore also keeps what the run actually consumed and decided, in two
complementary forms (the rr/Hypothesis/Jepsen split: an exact input tape,
plus a human-readable decision file):

| File | Holds | Replays with |
|---|---|---|
| `tape.bin` | every randomness value Bedrock served the branch (RDRAND, RDSEED, getrandom, with the requesting pid) and every host action (`tempo-dst start/finalize`, artifact fetches) with its virtual time | `bedrock-dst replay --tape <out>/seed-N` |
| `scenario.json` | every harness decision by value: swarm record (preemption period and seed, load, reference, nemesis), nemesis kill plan, per-load-generation `txgen_seed`/`spec_seed`, run parameters, and what the guest reported (`observed`: plan event, per-generation spec keccak256 and trie slots). A `meaning` map documents each field | `bedrock-dst replay --scenario <out>/seed-N` |
| `manifest.json` | what the tape is tied to: sha256 of vmlinux, initrd, images.tar, compose file and fetched tempo-dst binary, image tags/config digests/labels from images.tar, `bedrock.ko` srcversion and file sha256 (`$BEDROCK_KO` or `modinfo`), TSC frequency, boot seed, warm-checkpoint and branch-end virtual time, campaign args, `--build-info KEY=VALUE` pairs (e.g. Tempo/reth revisions), tape sha256 and per-consumer draw counts | checked by `replay --tape` |

**Tape format** (`bedrock_lab::Tape`, version 1, little-endian): magic
`BDRKTAPE`, version, flags, body length, body, FNV-1a checksum. The body is
the TSC frequency, start/end virtual time, then the randomness inputs
(`at`, source, pid, bytes) and the I/O inputs (`at`, target, command,
record_output). Readers reject other magics, newer versions, truncation and
checksum mismatches. `InputRecording`, `Tape` and friends also derive serde
(bedrock-lab feature `serde`) for JSON dumps.

Branches get the seed's RNG stream from the driver (`bedrock_lab::SeededSource`:
the hypervisor's xorshift64 values and getrandom fill, byte for byte), so
every draw exits to userspace, exactly as it does when a tape replays. Serving
it in-kernel (`Branch::reseed_rng`, `--kernel-rng`, what older campaigns
did) can be recorded too (`Branch::set_record_inputs`: the hypervisor emits a
`Randomness` record in seeded mode as well), but does not replay exactly: the
in-kernel path completes a VMCALL/RDRAND in one exit, the userspace path
re-executes it, so an interrupt pending at that exit lands on the other side
of the draw. Seen live: a `--kernel-rng` tape diverged at input #95 of 537
(same bytes, 563 instructions later); the default replayed identically. The
extra exits are cheap (~540 draws per 90 s run; thread-fuzz draws through BPF
prandom, not getrandom). `--no-tape` skips writing the tape.

`run.sh` builds `tempoInitrd` and the static `tempo-dst` binary through Nix.
The prepared initrd is cached in the store and reused across invocations;
the binary is served through the file-fetch channel at boot. The tape manifest
pins both files by sha256.

**`replay --tape`** (validated live: a `--decisions explicit --load trie` seed with preemption, two nemesis kills and three load generations replays identically) re-checks the manifest against the current files and
module (refusing on any mismatch unless `--force`), boots, checks the warm
checkpoint lands at the recorded virtual time, then branches it with a
`RecordedInputSource`: randomness comes from the tape (the in-VM PRNG is not
used), and the lab queues each recorded host action at its recorded virtual
time. The driver re-issues its usual commands and each must equal the next
recorded one. A replay that asks for randomness the tape does not have (or
of another channel or length) stops with an error naming the input index; it
never invents bytes. At the end the replay's own recording must equal the
tape, and the artifacts must be byte-identical.

**`replay --scenario`** boots like the campaign, reseeds the RNG with
`rng_seed`, applies `swarm.preempt`, and delivers the recorded decisions in
`config.json` (`nemesis_plan`, `load.generations`; the guest uses explicit
values and derives only what is absent). It never calls the driver's swarm
draw, nemesis planner or seed derivations. Whether it is byte-identical
depends on who made the decisions in the original run:

- `--decisions explicit` campaigns: the driver derives the kill plan and load
  seeds from the seed before the run and delivers them, so the replay
  delivers the same config and must match byte for byte. This changes which
  kill plan each seed gets (drawn by the driver, not from guest getrandom),
  so it is opt-in.
- `--decisions guest` (default, and all older campaigns): the nemesis draws
  its plan in the guest; the driver reads it back from the `plan` event. The
  replay has to add it to the config, which shifts guest timing, so it
  checks only that the same decisions were made (plan, per-generation seeds
  and spec keccak) and reports artifact differences as a note.

Either way the replay fails if the guest made other decisions (e.g. the trie
slot or spec generator changed). `BEDROCK_DST_DERIVATION_SALT=<n>` perturbs
the driver's derivations, to check that a scenario replay does not depend on
them.

`--spec-seeds-from-getrandom` moves each generated load's spec seed onto
Bedrock's controlled stream: the guest draws it from getrandom when the
generation starts (attributed to its pid on the tape, logged in the `load`
event, copied into `scenario.json`). It changes every seed's load and makes
scenario replays decision-identical rather than byte-identical, so it is
opt-in.

**What covers what.** The tape covers everything that reaches the guest,
including thread-fuzz schedules and Tempo's and the kernel's own randomness,
but only for the binaries in the manifest. The scenario survives changes to
how decisions are derived from seeds, but not to randomness inside the guest
that is not a harness decision (thread-fuzz, Tempo's RNG): it reproduces that
only through `rng_seed`, i.e. while the binaries stay the same. Restarting a
run part-way is by re-execution from the warm checkpoint (a tape prefix),
not disk snapshots: see [Branching](#branching-from-a-moment).

## Branching from a moment

One recorded seed can be turned into many nearby runs, Antithesis-style:
re-execute it exactly to a moment `T`, checkpoint there, and fork `K`
branches that continue from `T` with other randomness and/or other
decisions. Each branch is a full seed run (oracles, artifacts, verdict) with
its own `tape.bin`, `scenario.json` and `manifest.json`, so it replays with
`replay --tape` and can itself be branched.

```sh
bedrock-dst moments dst-out/seed-7               # pick T
bedrock-dst branch dst-out/seed-7 --at 31.2 --seeds 50          # fresh randomness
bedrock-dst branch dst-out/seed-7 --at 31.2 --seeds 8 --vary both
bedrock-dst branch dst-out/seed-7 --at input:812 --vary none --seeds 1  # = the original
bedrock-dst replay --tape dst-out/seed-7/branches-31.2s/branch-3
```

**How.** `branch` checks the manifest like `replay --tape` (refusing other
binaries unless `--force`), boots the warm checkpoint, and branches it with a
`bedrock_lab::PrefixSource`: the tape's inputs before `T` served strictly
(randomness in order; host actions, i.e. `tempo-dst start`, at their recorded
virtual times), with the driver's commands checked against them. At `T` it
checkpoints (in memory) and forks each branch from there with a
`PrefixSource` past its prefix, so a branch's input recording is the tape's
prefix followed by what its suffix source served. Each branch then runs to the
original end time, finalizes and is judged exactly like a campaign seed.
`Checkpoint::branch_with_input_source` replaces the checkpoint's source
entirely, including a host action it had pulled but not queued.

**`--at`** takes virtual seconds since the run forked from the warm
checkpoint (`31.2`; what `moments` prints), absolute virtual time
(`vt:110.5`, as `console.log` stamps it), or "just before randomness input
`n`" (`input:812`). The run stops at the first exit at or after `T`; the
branch records where it actually forked (`manifest.json` `branch.at_instructions`,
`branch.cut`: how many randomness inputs and host actions came from the
parent). `T` must lie after the start action and before the run's end.

**`moments <seed-dir>`** lists moments to branch from: the start action,
each nemesis kill and restart, each later load generation, and, per Always
signature, its first failure, the last pass before it, and 5 s before it.
Failure times come from `assertion`/`first-failure` events (the guest emits
one per signature and process, with the guest time of its last pass);
guest times are mapped to virtual time through the run's first event, so
they are good to a few hundred ms (`input:<n>` is exact).

**`--vary`** (what changes after `T`):

| `--vary` | Randomness after `T` | Decisions after `T` |
|---|---|---|
| `none` | the rest of the tape | the original's (the run is the original, byte for byte) |
| `rng` (default) | a fresh `SeededSource` per branch | the original's (the kill plan and load seeds are already in the guest) |
| `decisions` | the original's stream continued (`SeededSource` of the stretch in force, skipped past what the prefix drew) | re-drawn |
| `both` | fresh | re-drawn |

Re-drawing keeps every kill that fired before `T` and every load generation
started before it (generation 0 and one per restart done); the rest of the kill
plan is drawn like the guest planner would, starting after `T`, and later
generations get fresh txgen and spec seeds. The driver delivers them at `T`
with `tempo-dst redecide '<json>'`: the guest waits until the nemesis is
between faults, stops it, keeps whatever already happened (the guest knows
exactly which kills fired; the driver only estimates it from the kill times),
writes the merged plan into `config.json` and starts `tempo-dst nemesis
--resume`, which finishes a kill in flight and continues on the original
nemesis clock. The merged plan is the nemesis' last `plan` event, so it is
what the branch's `scenario.json` records. `redecide` is a host action on the
branch's tape and in `manifest.json` `branch.actions`; `replay --tape`
issues it again at its time.

**Determinism.** Branch `i`'s seed is `branch_seed(parent tape sha256, T, i)`,
so a branch is a pure function of the parent tape, the moment and the branch
seed. `--vary none` must reproduce the parent exactly (artifacts and tape),
and `branch` fails if it does not. A branch's manifest records `parent`,
`parent_tape_sha256`, `at`, `cut`, `vary`, `branch`, `rng_seed`, and
`rng_streams` (which `SeededSource` served which stretch of the run's
randomness, so `--vary decisions` on a branch continues the right stream).

**Output** (default `<seed-dir>/branches-<at>/`): `boot-console.log`,
`prefix-console.log` (the prefix, also at the top of each branch's
`console.log`), `branch-<i>/` (a seed directory), and `summary.json`; the
driver prints signatures × branches like `hunt.sh`, plus how many branches
are distinct runs (by assertions and events) and how many equal the parent.

**Use cases.**

- *Explore around a failure.* For a seed that hits #16 (a wrong block), branch
  50 ways from 5 s before the first wrong block: `moments` prints the
  `5 s before first failure: E9/...` line, then `branch <seed> --at <that>
  --seeds 50`. How many branches still fail, and with which signatures,
  says whether the bug needs that exact schedule or just that state.
- *Find siblings*: `--vary both` from a kill shortly before the failure
  re-rolls the later kills and loads, finding other paths to the same or
  neighboring signatures.
- *Minimize*: branch from later and later moments (or `--vary rng` from just
  before the failure) to find how little of the run must be replayed for the
  failure to stay reproducible; each failing branch is a smaller, replayable
  seed.

**Validated live** on a `--load trie --run-secs 90` seed on the pinned image
(one kill at 27 s, failing with the known E5/E6 multiproof/untargeted
signatures at ~73 s): `--vary none` from the restart (30.9 s, input 89 of
370) reproduced every artifact, the tape and console.log byte for byte; 4
`--vary rng` branches from there gave 4 distinct runs (3 pass, 1 with the same
E5/E6 signatures); rerunning branch 1 alone gave the identical tape and
artifacts; 3 `--vary both` branches from 15 s re-drew kills at 16/56, 17/51
and 39 s and passed; `replay --tape` of an rng branch and of a `both` branch
(with its `redecide`) were identical.

**Limitations.** A branch needs the parent's binaries (it re-executes the
tape). `--vary decisions` needs a guest with `redecide` (this tree; older
initrds lack it) and changes guest timing at `T` by the host action itself.
Moments derived from guest events are approximate. Branches run
sequentially in one process from one in-memory checkpoint.

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
- [x] **A6** Trace replay: per-seed `tape.bin` + `manifest.json` (`replay --tape`) and `scenario.json` (`replay --scenario`); `--decisions explicit`
- [ ] **A6b** Validate `replay --tape`/`--scenario` across loads and many seeds (one trie seed done)
- [x] **A6c** Branching from a moment of a recorded run: `bedrock-dst branch --at T --vary none|rng|decisions|both` (`PrefixSource`, `tempo-dst redecide`), `bedrock-dst moments`

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
- [x] **D4** Trie-shaping load (`--load trie`): raw-storage writes over per-seed generated slot shapes and values, checked by E5 and E6
- [x] **D6** TIP-20 bank-transfer load (`--load tip20`): per-seed transfers among a closed set of holders of a fresh token, checked by E8
- [x] **D7** Chain-of-blocks load (`--load chain`): hash-chained appends from one sender, checked by E9
- [ ] **D5** Coverage-guided mutation with tuner's mutator (needs G3)

### E: Oracles (`guest/tempo-dst`)
- [x] **E1** Log scanner over `journalctl CONTAINER_NAME=tempo` (survives restarts)
- [x] **E2** Liveness
- [x] **E3** Durability of finalized blocks across crash/restart (the `Saved range` frontier is not durable: reth unwinds to its state-trie frontier)
- [x] **E4** Graceful stop + trie repair dry run + `tempo re-execute`. Verify on host that `--chain dev` matches the dev node's chain spec (override with `TEMPO_DST_CHAIN`)
- [ ] **E4b** Independent state-root check: rebuild the trie from the final state, separate from the sparse trie
- [x] **E7** Reference-node differential oracle (`--reference`): a vanilla-flags follower re-executes every block; logs, lag, header/state-root and proof answers compared (`guest/tempo-dst/src/reference.rs`)
- [ ] **E7b** First campaign with `--reference`; then consider defaulting it on
- [x] **E8** TIP-20 ledger: conservation, per-block explanation by `Transfer` logs, pinned snapshots stable across persistence and restarts
- [ ] **E8b** Run `--load tip20` under Bedrock and tune the pin cadence and required coverage from real runs
- [x] **E9** Chain of blocks: receipts-rebuilt hash chain vs. state at every block, prefix/monotonic history, finalized entries and pinned history across restarts
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
