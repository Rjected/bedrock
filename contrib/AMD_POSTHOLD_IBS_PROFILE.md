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
hold. Guest-entry cost has a hardware component. The prior hash-index trial
passed but did not improve full-campaign time.

A separate 60-virtual-second diagnostic attempted to count alias proof cache
hits and fresh walks. The box stopped responding to SSH/Tailscale while that
campaign was active, so it has **no workload verdict or usable counters**.
The diagnostic was repeated with counters in the collector and reporting on
RUN return, on fresh EPYC 4245P/HWE box `box-4ed44560`. That shortened
campaign passed (`failed=0`, 117 host seconds), and a snapshot at 10,843,256
collector calls showed:

| Outcome | Calls | Share |
| --- | ---: | ---: |
| Cached proof hit | 10,358,397 | 95.5% |
| Reused multi-page proof | 127,412 | 1.2% |
| Composed single-page proofs | 90,059 | 0.8% |
| Fresh alias walk | 265,750 | 2.5% |

The numbers are diagnostic counters sampled before the run finished, not a
full-campaign measurement. They show the 8% IBS collector hotspot is mostly
cache-hit work, not repeated page-table walks. A direct validation of the
last single-page proof covered only about 6% of calls; its same-box shortened
run also took 117 seconds. Extending the direct validation to multi-page
proofs passed the shortened run in 120 seconds. Neither trial showed a speed
gain. The direct-validation code remains in an isolated, uncommitted
worktree; it was not merged. All module builds and loads were inside boxes.

An isolated reduction of the alias-proof cache from 2,048 to 512 entries
passed all 276 VMX unit tests and the in-box SVM suite. Same-box
60-virtual-second campaigns both passed: **118 host seconds** for the
unchanged cache and **118 seconds** for the smaller cache. This short
diagnostic does not justify changing the production cache size or predict
the full pinned campaign. The cache-size trial was not merged.
