# AMD scalar-fetch cause profile (2026-10-16)

An isolated module based on production `5fc001a` classified each scalar
instruction-fetch NPT fault during the pinned Tempo DST workload. The 279
local VMX tests, the disposable-box SVM suite, a 60-virtual-second diagnostic,
and the exact 180-virtual-second pinned seed all passed (`failed=0`). Tests
ran on EPYC 4245P / HWE `7.0.0-38-generic` box `box-5e63aad0` with the
unchanged pinned guest and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.

The counters persist across runs and log every 1,048,576 scalar faults. The
following approximate full-run counts subtract the short run's last logged
checkpoint from the full run's last logged checkpoint:

| Cause | Additional full-run faults | Share |
| --- | ---: | ---: |
| One scanned hazardous offset | 18,034,654 | 55.5% |
| Two offsets | 6,051,306 | 18.6% |
| Three offsets | 4,061,724 | 12.5% |
| Four offsets | 3,390,745 | 10.4% |
| Five or more offsets | 893,717 | 2.7% |
| Rejected page scan | 73,710 | 0.2% |
| Gate unavailable/dirty, table page, missing memo, boundary-only | 0 | 0% |

A one-in-8,192 follow-up sample logged 1,067 one-hazard fetches across 182
physical-page/hazard pairs. In 494 samples (46.3%), the sole hazard was at
offset `0xfff`, usually an isolated `0x0f` at a page boundary. A further
diagnostic counted the existing guarded single-alias safe-continuation proof:
it succeeded on 1,776,708 of its first 3,145,728 candidates (56.5%) in a
passing 60-virtual-second run. This identified repeated fetch faults on
pages whose boundary was already proven safe for the current guarded tree.
The resulting conditional execute-trust optimization is documented in
`contrib/AMD_CI_CONDITIONAL_BOUNDARY_TRUST_1016.md` on the production branch.

Raw kernel logs are under ignored
`target/boxctl-evidence/box-5e63aad0/scalar-cause/` in this worktree.
Instrumentation is diagnostic only. No module was built or loaded on the
workstation.
