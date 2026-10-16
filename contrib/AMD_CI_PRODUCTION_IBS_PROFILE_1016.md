# AMD pinned CI production IBS sample (2026-10-16)

The unchanged `dan/amd-tempo-dst` module ran the pinned one-seed,
180-virtual-second Tempo CI workload on disposable EPYC 4245P / HWE
`7.0.0-38-generic` box `box-d267068a`. The campaign passed with `failed=0`
in 189 host seconds. During its first 100 host seconds, `perf record -p
<bedrock-dst PID> -e ibs_op// -g` collected 387,151 samples with zero
reported lost samples. The timing includes profiling and is not an unprofiled
performance comparison.

The flat, self-only report attributed these shares of samples:

| Function | Share |
| --- | ---: |
| `svm_run_guest` | 17.87% |
| `collect_page_breakpoints` (forked VM) | 7.72% |
| Forked VM RUN loop | 7.39% |
| Root VM RUN loop | 5.33% |
| `__pi_memcpy` | 2.99% |
| `collect_page_breakpoints` (root VM) | 1.04% |

This agrees with the earlier profile: low-level VM entry/exit and breakpoint
proof collection remain significant. The profile alone does not isolate a
safe speedup. The accompanying full-campaign exit count remains the stronger
evidence that instruction-fetch NPT faults dominate exit traffic. These IBS
shares cover the first 100 host seconds only; they are not a full-campaign
wall-time decomposition.

The raw `perf.data` and campaign log are retained on the disposable box and
under ignored `target/boxctl-evidence/box-d267068a/` where collected. No
kernel module was built or loaded on the workstation.
