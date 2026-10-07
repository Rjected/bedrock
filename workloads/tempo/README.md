# Tempo under Bedrock

Installed checkout: `/home/ubuntu/bedrock`, branch `alexey/guest-ncpus`,
commit `4fdeb933879cdd9afcbdcb0f0ac0f0328f375335`.

This workload runs the pinned Tempo localnet image in bare development mode,
checks that affinity reports **five CPUs**, checks chain ID 1337 and advancing
blocks, then shuts down the Bedrock guest. Bedrock executes one virtual CPU;
`bedrock_ncpus=5` enables Tempo's parallel sparse-trie path.

From the repository root:

```sh
DOCKER='sudo docker' ./workloads/tempo/build.sh
sudo ./workloads/tempo/boot-host.sh  # only when the NixOS VM is stopped
./workloads/tempo/run.sh
```

The NixOS VM is already running. SSH access is local-only:
`ssh -p 2222 dev@127.0.0.1` (password `dev`), or root (password `root`).
Its `/dev/bedrock` module runs on Linux 6.18. The Ubuntu host kernel lacks
`CONFIG_RUST`, so Bedrock runs inside this VM rather than in the host kernel.
The RPC endpoint is inside the Bedrock guest; this setup does not expose it
as a host TCP port.

The successful run is saved in `verified-run.log`. It confirmed:

- `Tempo check: reported CPUs=5`
- `Tempo check: chain ID=0x539 (1337)`
- `TEMPO_BEDROCK_PASS: blocks advanced 0x0 -> 0x5`
- Eleven validator results with `strategy="sparse-trie"`, plus payload-builder
  sparse-trie wait timings.
- Clean shutdown through Bedrock's VMCALL, with exit status zero.

The container uses `--bare --block-time 200ms`. This image's full bootstrap
failed on a faucet expiry versus block timestamp mismatch, and its wrapper
passes whole-second intervals without a unit. Bare mode retains the dev node
and prefunded accounts, but skips faucet/liquidity setup. No transactions
were submitted by this smoke test.

`prepare-initrd.sh` sets `GOMAXPROCS=1` for Podman's Go runtime and starts the
journal stream early. Tempo's Rust runtime still sees five affinity CPUs.
`run.sh` stages workload files onto the NixOS VM's local filesystem because
9P reads directly into Bedrock's mapped guest memory returned EIO.

KVM does not expose EPT-friendly PEBS to this nested VM. The workload runs
with fallback timer injection, but this test does **not** establish exact
determinism. Native execution on a compatible Rust-enabled host kernel is
needed to validate that property.

## Txgen

The workload also includes `txgen-tempo` and `bench` from the pinned image
`ghcr.io/tempoxyz/txgen@sha256:5a13376e67209c218375ec42ae21e83dfca2998491b2c83292a0c722f4a96fbf`.
It generates pathUSD TIP-20 transfers from the ten prefunded development
accounts, using ordinary nonces, seed 99, and no faucet. The driver verifies
submission, inclusion, and zero reverted receipts, exports the full report
through Bedrock's file-store hypercall, then shuts down the guest.

```sh
DOCKER='sudo docker' ./workloads/tempo/build.sh
./workloads/tempo/run.sh txgen  # 1,000 transfers, target 100 TPS
TXGEN_COUNT=10000 TXGEN_TPS=1000 ./workloads/tempo/run.sh txgen
```

The full report is exported to `/tmp/tempo-workload/txgen-report.json` in the
NixOS VM. Retrieve it with:

```sh
sshpass -p root scp -P 2222 root@127.0.0.1:/tmp/tempo-workload/txgen-report.json ./txgen-report.json
```

The two verified runs are preserved in `txgen-report-100tps.json` and
`txgen-report-1000tps.json`, with logs and a compact `txgen-results.json`.

| Target TPS (guest time) | Transfers included | Failed / reverted | Included TPS (guest time) | Host-observed send phase | Total VM wall time |
| --- | --- | --- | --- | --- | --- |
| 100 | 1,000 | 0 / 0 | 89.3 | 7.56 s (~132 TPS) | 69.97 s |
| 1,000 | 10,000 | 0 / 0 | 819.7 | 28.70 s (~348 TPS) | 91.39 s |

Sparse-trie validator results appeared in both runs. Host phase timings come
from journal milestone arrival times, polled every 50 ms; they are approximate
and can include log-delivery delay. Guest-clock TPS and RPC latencies are
emulated-time measurements. These capped-rate tests do not establish maximum
throughput or native-versus-Bedrock overhead, and nested PEBS remains unavailable.

## Comparing traces

The paired 10,000-transfer runs use identical kernel, initrd, image archive,
compose file, txgen seed 99, and Bedrock seed 12345. Inputs and their SHA-256
hashes are recorded in `traces/inputs.json`. Each run starts a fresh Bedrock
guest and saves its complete event stream, console, exit statistics,
and benchmark report under `traces/run1/` or `traces/run2/`. The attempted
signed-transaction export failed with file-store buffer errors in both runs;
the comparison records the byte-for-byte transaction comparison as unknown.

Capture flags are `--events-jsonl PATH --event-categories all --exit-capture all
--no-memory-hash --exit-stats-json PATH`. Full exit tracing materially changes
wall-clock performance. Memory hashes are disabled; register and device
hashes are retained. `run.sh` accepts extra Bedrock CLI arguments after its
workload argument, but regenerates the initrd, so freeze the initrd once when
making a strict comparison across runs.

```sh
python3 workloads/tempo/compare-traces.py workloads/tempo/traces
python3 workloads/tempo/compare-traces.py workloads/tempo/traces --exclude-ept
```

The comparison uses the same deterministic exit fields as Bedrock's
`compare_exit_records`, excluding host timestamps and sequence numbers.
It also checks event-sequence gaps, the signed transaction stream's hash,
transaction success and reverts, sparse-trie execution, and payload hashes
at common heights. Results are saved in `traces/comparison.json`.
The second command separately saves `comparison-without-ept.json`, excluding
EPT fault exits (reason 48) to distinguish extra faults from later differences
in execution. Installing `orjson` is optional and speeds up large traces.

Both traced runs included 10,000 transfers with zero failures or reverts and
the same 113 payload hashes. Their benchmark reports recorded 819.672 included
TPS in guest time. Total wall time with tracing was 277.589 and 264.403 seconds.
The initial positional comparisons mismatched during startup, but complete
stream alignment identified trace-record loss: 4 exits missing from run 1 and
9 from run 2, each following a timer event. All 6,397,296 common captured exit
states match, as do the full timer, randomness, and serial streams. The event
producer stages a timer when its buffer fills, then drops the next exit record
before the run loop drains. See `traces/diagnosis.md` and
`traces/proposed-trace-drain-fix.patch`. Full memory hashing was disabled, so
this does not verify equality of all guest memory.

## Trace-buffer fix verified

The run-loop guard is now applied and the rebuilt module is loaded in the
NixOS VM. Two complete txgen reruns capture 6,397,309 deterministic exits
each and match directly without EPT filtering or alignment. Timer, randomness,
and serial streams also match; all 113 block hashes match, with 10,000 transfers
included in each run and zero failures or reverts. The regression fails without
the guard, and the patched workspace passes 253 tests (19 ignored), plus kernel
build and stack checks. See `traces-patched/summary.md` and `comparison.json`.
