# Regressions: rediscovering known reth bugs

Each entry rebuilds the Tempo node with one historical reth fix undone (or, for
an unmerged fix, uses the pinned node as the buggy build and the fix as the
control), then runs the **generic** campaign on both with identical seeds. The
entry names no trigger: it reproduces when the ordinary load, faults and
oracles find the bug on their own, which is the claim worth making about the
harness. Hit rate over seeds is the measure.

```sh
DOCKER='sudo docker' ./workloads/tempo-dst/regressions/run.sh reth-27267 --seeds 20 --run-secs 180
```

`<name>/regression.env`:

| Variable | Meaning |
|---|---|
| `RETH_REVERT` | Merged fix in the pinned reth; buggy = pinned with it reverted, fixed = pinned |
| `RETH_PATCH` | As `RETH_REVERT`, for fixes that no longer revert cleanly: a patch in the entry's directory |
| `RETH_FIX_PR`, `RETH_FIX_HEAD` | Unmerged fix; buggy = pinned, fixed = pinned with the PR applied |
| `VARIANT` | `run.sh --variant` (node flags; part of the boot prefix) |
| `CAMPAIGN_ARGS` | Extra `bedrock-dst campaign` arguments (normally just the load) |
| `EXPECT` | Signature prefix a buggy-image seed must fail with, and no fixed-image seed may |

`build.sh <name>` builds `bedrock/tempo-localnet:<name>-{buggy,fixed}` (Tempo
`d3f3b28f`, reth `42fa3c5`, every reth crate patched to a local checkout).

| Entry | Bug | Status |
|---|---|---|
| `reth-27267` | #27267: proof v2 emits inlined leaves as standalone proof nodes (`eth_getMultiProof`) | Defined; not run yet |
| `reth-26843` | #26843: proof v2 drops a clean sibling on branch collapse under a prefix set (historical `eth_getMultiProof`) | Defined; not run yet. Patch also reverts #27634 and rolls back four later proof v2 commits |
| `reth-27615` | #27615: range trie changesets miss nodes created and deleted while the trie frontier lags (historical proofs, disk unwinds) | Defined (`--variant masking`); not run yet |
| #27270 | Sparse-trie reuse across forks with identical state roots; publishing abandoned jobs' tries | Not defined: needs forks. Tempo disables the engine API, so this needs a fork driver |
| #27614 | Missing prefix invalidation for in-memory fork keys | Not defined: unreachable at the pin since #27634; needs that reverted plus a fork driver |
