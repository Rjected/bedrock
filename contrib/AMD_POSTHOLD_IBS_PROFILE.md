# AMD IBS profile after the safe multi-page hold

On 2026-10-15, the production branch at `712cce1` ran the exact pinned
180-virtual-second, one-seed Tempo DST campaign on `box-df9442c1` (EPYC
4245P, HWE `7.0.0-38-generic`). It passed with `failed=0` in **199 host
seconds** while `perf record -p <bedrock-dst PID> -e ibs_op// -g` sampled the
running process. The sampler captured **716,000 IBS operation samples with
zero lost samples**. This timing is profiled and should not be compared as a
normal unprofiled campaign time.

The flat, self-only report attributed these shares of IBS samples:

| Function | Share |
| --- | ---: |
| `svm_run_guest` | 17.03% |
| Forked VM RUN loop | 8.79% |
| `collect_page_breakpoints` | 8.01% |
| Root VM RUN loop | 3.74% |
| `__pi_memcpy` | 3.14% |

IBS sample share is a hotspot guide, not a wall-time decomposition. The
prior full-campaign profile also identified `svm_run_guest` and
`collect_page_breakpoints`; both remain material after the safe multi-page
hold. Guest-entry cost has a hardware component. The alias collector needs a
cache-hit-versus-page-walk breakdown before another cache change: the prior
hash-index trial passed but did not improve full-campaign time.

A separate 60-virtual-second diagnostic attempted to count alias proof cache
hits and fresh walks. The box stopped responding to SSH/Tailscale while that
campaign was active, so it has **no workload verdict or usable counters**.
That instrumentation is isolated in an uncommitted worktree and is not part
of the production branch. All module builds and loads were inside the box.
