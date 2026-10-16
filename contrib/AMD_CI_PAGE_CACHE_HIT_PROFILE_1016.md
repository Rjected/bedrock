# AMD CI page-plan cache hit profile (2026-10-16)

This diagnostic adds aggregate lookup and hit counters to the isolated
page-scoped global-plan cache. It logs one snapshot per 1,048,576 eligible
global-planner calls. The pinned one-seed, 180-virtual-second Tempo CI run
passed with `failed=0` on disposable HWE box `box-e675c0c1`. The diagnostic
wall time was 192 seconds, including counter overhead; it is for attribution,
not a candidate speed comparison.

The last retained snapshot recorded **56,623,104 lookups and 1,029,227 hits**
since module load, a **1.82%** hit rate. The module had first run the short
in-box SVM suite, but the 54 snapshots accumulated during the full CI run
dominate these totals. Full-plan reuse is too infrequent to explain a large
speedup even with a page-scoped key. This is consistent with the alternating
candidate/control timing in `AMD_CI_PAGE_PLAN_CACHE_TRIAL_1016.md`, which did
not establish a gain. Improving the full-plan cache further is lower priority
than reducing guarded-page exits and breakpoint-capacity faults.

All 277 VMX library tests and the in-box SVM suite passed before the full
run. The production module was restored afterward and passed the same in-box
suite. The diagnostic source is isolated on
`dan/amd-ci-page-cache-hit-profile-1016`; kernel-log snapshots and campaign
status are under ignored
`target/boxctl-evidence/box-e675c0c1/ci-page-cache-hits/`. No workstation
kernel module was built or loaded.
