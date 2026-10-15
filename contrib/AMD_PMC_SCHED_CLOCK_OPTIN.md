# AMD sched-clock PMC1 trial

This isolated branch combines the per-VM AMD PMC clock opt-in from
`dan/amd-pmc-clock-optin-1015` with the earlier guest patch that reads PMC1
inside Linux `native_sched_clock()`. The guest image is
`de6267929714add0f4ab37ee7e87b14976530f3fb11aaa4ba07f4a13a0765baa`
(SHA-256). Only run it with `BEDROCK_PMC_CLOCK=1` on a disposable `boxctl`
box. Kernel modules must be built and loaded inside the box, never on the
workstation.

On `box-a42f86b1` with HWE `7.0.0-38-generic`, the box SVM suite passed and
the exact pinned one-seed Tempo campaign passed in **189 host seconds**
(`schedoptin1`, 2026-10-15). Near virtual second 237 it had 28.27 million
exits, including 1.79 million RDTSC exits. The clocksource-only opt-in runs
passed in 186 and 195 seconds; near virtual second 236 the first had 33.07
million exits, including 5.30 million RDTSC exits. The reduction in exits has
not shown an end-to-end speedup. The earlier sched-clock branch stalled once
at virtual second 113 and passed three subsequent runs. That intermittent
stall remains undiagnosed, so this branch is an experiment, not a merge
candidate. The box was restored to its original guest image and production
module after this trial.
