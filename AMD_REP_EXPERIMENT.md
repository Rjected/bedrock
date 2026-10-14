# AMD REP retirement experiment (not merge-ready)

This branch is an isolated experiment based on `dan/amd-tempo-dst` at
`1dc5451`. It removes recognized F3 MOVS/STOS instructions from the global
page-hazard set, bounds their native execution, and charges one virtual tick
when they finish. It also pauses host IBS during VMRUN as a diagnostic
workaround for profile-induced replay divergence. Neither change has been
accepted as the general AMD clock rule.

On an EPYC 4245P box with the HWE 7.0.0-38 kernel, the pinned one-seed Tempo
DST campaign passed in 326 host seconds, including a 269-second seed. The
current AMD branch took 599 seconds on the same workload; the Intel CI
reference is about 65 seconds. The 326-second result is **not an equivalent
Intel comparison**, because this experiment changes REP virtual time.

The SVM smoke/transition suite, 271 `bedrock-vmx` unit tests, 82 other
relevant library tests, and 1,024 fork replays of a 10,000-iteration REP
passed. The REP fork replay also passed with host IBS sampling on the guest
CPU. The full Tempo campaign was not repeated to completion; a second run
was stopped after finding the accounting gap below.

**Known correctness gap:** an entry *at* REP uses this branch's one-tick
rule, while a global batch that *reaches* REP can still charge the hardware
retirement count. Virtual time therefore depends on how the instruction was
entered. Removing the REP page hazard without changing the clock rule is not
a safe shortcut either: the SVM transition suite crossed an exact deadline
(`reason=258`, `vt=103` for a requested `vt=100`). Keep this branch separate
from `dan/amd-tempo-dst` until REP accounting is consistent across both
entry paths and exact-deadline tests pass.

The IBS pause code reads two host MSRs on every VM entry and is diagnostic.
Its runtime cost and host profiling semantics need review before production.
Kernel module builds and hardware tests were performed only inside the
ephemeral `boxctl` box; no module was loaded on the developer host.
