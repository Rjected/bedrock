# AMD multi-hazard reachable-region feasibility (2026-10-18)

The pinned Tempo workload still incurs about 22 million NPT faults per full
seed after the frequent-successor change. Earlier fault sampling found that
roughly 45% of sampled fetch-fault targets would exceed AMD's four hardware
breakpoint slots when combined with the preceding plan. The existing
reachable-region prover runs only on a current page with five hazards. This
diagnostic asks whether applying that proof to current pages with two, three,
or four hazards could free slots for another guarded page.

The probe samples one in 256 guarded current-page plans after the ordinary
alias proof succeeds. It runs the reachable-region prover on a copy of the
batch, restores its proof-cache entry and cursor, and records whether the
result uses fewer breakpoint slots than the ordinary proof. It does not
change execution or grant extra NPT execute permission. Counts are split by
the page's hazard count, with separate zero-breakpoint and failed-proof
counts. A positive result would only establish opportunity: a region proof
must be checked for safe composition across executable aliases and page
boundaries before it can replace an alias proof in a multi-page plan.

All 282 `bedrock-vmx` library tests pass. A kernel-module build, SVM suite,
and pinned workload measurement remain pending on a disposable box with the
accelerated AMD PMU path. No kernel module was built or loaded on the
workstation. Recent `m4-metal-small` allocations in FRA and FRA2 provisioned
EPYC 4244P, whose earlier in-box SVM tests reported
`pmu_virtualization=false`; LON, LON2, ASH, and SJC2 lacked suitable stock.
