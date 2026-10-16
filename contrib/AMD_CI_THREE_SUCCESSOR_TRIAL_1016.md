# AMD third guarded successor candidate (2026-10-16)

The current trusted-source planner offers a second-order successor and one
frequent destination. When the second-order prediction differs from both
frequent destinations, it omits the other frequent edge even if the shared
four-breakpoint budget has room. An isolated hypervisor-only candidate now
offers that third destination. It checks the cached translation, exact-byte
hazard, guarded single-page alias proof, pairwise page boundaries, and the
four hardware breakpoints before adding any page. No guest address or
workload rule is encoded in the production path.

All 283 `bedrock-vmx` library tests pass, including a test that guards two
distinct frequent destinations beside a different second-order prediction.
The SVM-only kernel module built inside disposable HWE boxes; no module
was built or loaded on the workstation.

**Accelerated hardware validation and timing remain pending.** A boxctl
`m4-metal-small` box in FRA provisioned an EPYC 4244P. Its SVM suite failed
the control-flow acceleration exit-count assertion (`20,023` exits), so it
cannot be used for a fair Tempo comparison. A replacement `m4-metal-small`
in FRA and one in AMS also provisioned 4244P; they were deleted before
testing. A `m4-metal-medium` box in FRA provisioned EPYC 9124, whose SVM
features reported `pmu_virtualization=false`; its mixed workload therefore
fell back to instruction stepping and failed its expected-exit assertion.
That box was also deleted. FRA2 and LON returned out-of-stock errors for
`m4-metal-small`. All created boxes from this attempt were deleted.

Next validation needs a disposable EPYC 4245P box with the HWE kernel and
the pinned Tempo guest assets. Run the full in-box SVM suite, then compare
the exact pinned 180-virtual-second seed against unchanged production on
the same box, with a candidate repeat if the first comparison improves.
Do not merge based on the library tests alone.
