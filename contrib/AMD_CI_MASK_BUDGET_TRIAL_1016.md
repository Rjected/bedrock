# AMD masked-breakpoint budget trial (2026-10-16)

This isolated hypervisor candidate removes two raw-hazard-count admission
checks. A raw count can exceed four even when AMD breakpoint masks compress
the executable aliases into four hardware slots. The existing guarded alias
proof, boundary checks, and final four-slot union still decide admission.

All 276 VMX library tests and the in-box SVM suite passed. An updated unit
case now admits a fifth code page with the same four breakpoint slots, showing
that the old upper-bound check could reject a provably fitting set. Kernel
module builds and loads took place only on disposable HWE box `box-e675c0c1`.

The pinned, one-seed, 180-virtual-second Tempo CI workload passed with
`failed=0` using the unchanged guest initrd and userspace driver. Candidate
wall time was **190 host seconds**, against an adjacent production control
of **188 host seconds** on the same box. This does not show a speedup, so the
change is not merged into `dan/amd-tempo-dst`. The candidate source is on
`dan/amd-ci-mask-budget-1015`; its status file is under ignored
`target/boxctl-evidence/box-e675c0c1/ci-mask-budget/`.
