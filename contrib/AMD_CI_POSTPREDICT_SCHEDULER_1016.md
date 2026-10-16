# AMD scheduler-page misses after two-successor prediction (2026-10-16)

Two diagnostic-only variants based on production `ce649f3` ran on
disposable EPYC 4245P / HWE `7.0.0-38-generic` box `box-2b77e4f7`,
with the pinned guest assets and driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
The first 60-virtual-second seed passed (`failed=0`) in 98 host seconds.
At its last checkpoint, 983,040 instruction-fetch faults still targeted
the pinned guest's scheduler physical page `0x1feb000`. About 93.2% followed
a global plan with at least one free hardware breakpoint slot. Among
one-in-4,096 fault samples, the leading preceding physical pages were
`0x1258000` (91 samples) and `0x1fe5000` (81).

The second variant sampled one in 256 trusted-source plan preparations on
those two source pages. At its last checkpoint, 12,800 samples found that
8,080 plans did not name the scheduler page as either predicted successor;
4,709 did name it and had its translation, guarded hazard, single-page
alias proof, and breakpoint budget available. Only 11 lacked an alias
proof, and no sample failed the other basic checks. These are samples of
source preparations, not a measured conditional prediction hit rate on
actual scheduler transitions. They point to successor selection rather
than proof rejection as the next hypothesis to test.

The second diagnostic run **failed** with a kernel PMU boundary error:
`count=17 budget=16 batch_count=16 bounded=true` at guest RIP
`0x14a98ec`. It has no valid workload verdict or timing. The diagnostic
instrumentation changed planner activity and logging, so the failure
cannot be attributed to unchanged production from this run alone. The
unchanged production module was restored and passed the full in-box SVM
suite. Raw counters and campaign status are under ignored
`target/boxctl-evidence/box-2b77e4f7/postpredict-sched/` in this
worktree. No module was built or loaded on the workstation.

An additional unchanged-production 60-virtual-second campaign on the
same box completed without a PMU boundary error but reported one
`E4/graceful-stop` workload failure (`failed=1`). That short diagnostic
also cannot serve as a passing control. Its status and log are under
ignored `target/boxctl-evidence/box-2b77e4f7/postpredict-production/`
in the production worktree. Earlier full-duration production runs on
this box passed; neither of these final short runs establishes a code
regression or a speed result.
