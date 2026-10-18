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

All 282 `bedrock-vmx` library tests pass. The diagnostic module built and
passed the full SVM suite inside disposable EPYC 4245P / HWE
`7.0.0-38-generic` box `box-00971fe7`. The exact pinned seed with a shortened
60-virtual-second workload passed (`failed=0`) in 95 host seconds. At the
last kernel counter checkpoint, 20,480 one-in-256 samples had these results:

| Raw hazard count | Samples | Fewer slots with region proof | Zero slots | Proof failed |
| ---: | ---: | ---: | ---: | ---: |
| 2 | 8,229 | 3,894 (47%) | 44 | 2,795 |
| 3 | 6,768 | 5,903 (87%) | 0 | 54 |
| 4 | 5,483 | 4,409 (80%) | 1 | 168 |

The 14,206 slot reductions are a promising *capacity upper bound*, not an
expected speedup. The existing region proof follows code reachable from one
entry point. Bedrock leaves unselected pages NX, so a jump out of that page
normally causes an exit. If a second page is made executable in the same
counted interval, it might jump back to an unvisited offset of the first
page. A safe multi-page implementation needs a joint reachable-code proof
over both pages, or another guard on that return path; simply replacing the
single-page alias proof with these smaller breakpoint sets is unsafe.

A follow-up diagnostic also logged physical page IDs for reduced-slot cases,
but its shortened campaign lost SSH connectivity before returning a verdict.
That run is not counted as passing or as a timing result. No kernel module
was built or loaded on the workstation. Earlier `m4-metal-small` allocations
in FRA and FRA2 provisioned EPYC 4244P, whose SVM tests reported
`pmu_virtualization=false`; LON, LON2, ASH, and SJC2 lacked suitable stock.
