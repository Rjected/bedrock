# AMD page-plan cache miss classification (2026-10-16)

The isolated page-scoped global-plan cache had only a 1.82% hit rate on the
full pinned CI campaign. This follow-up samples the first failed cache-key
condition once per 256 eligible planner calls. The disposable EPYC 4245P /
HWE `7.0.0-38-generic` box had less than two minutes left, so the diagnostic
ran the same pinned assets and seed for **30 virtual seconds**. It passed
(`failed=0`) in 86 host seconds. This shortened workload and instrumented
timing are **not comparable** with the 180-second Intel or AMD CI campaigns.

The last captured kernel snapshot spans 18,874,368 eligible calls and 73,728
samples:

| First failed cache condition | Samples | Share |
| --- | ---: | ---: |
| Current virtual code page changed | 64,073 | 86.90% |
| Guarded translation-tree generation changed | 8,198 | 11.12% |
| All listed key conditions passed | 1,372 | 1.86% |
| All other conditions combined | 85 | 0.12% |

The cache stores only the last plan per vCPU. Page changes dominate misses;
relaxing one generation check cannot by itself create a large hit rate.
Earlier work already tested a broader exact-context global-plan cache on the
10,000-transfer workload: it hit 17.8% of lookups but increased preparation
cycles and elapsed time (see `amd-profile-2026-10-10.md`, "Cache exact guarded
global page plans"). The current one-entry page cache remains unmerged.

All 277 VMX unit tests passed. The in-box SVM suite hit its previously seen
one-retirement `svm_bench` assertion once; an immediate unchanged rebuild and
rerun passed the full suite. The diagnostic-only module was loaded only in
the disposable box, which expired after the run. Source is isolated on
`dan/amd-ci-page-cache-misses-1016`; the kernel snapshots and campaign status
are under ignored `target/boxctl-evidence/box-e675c0c1/ci-page-cache-misses/`.
