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

Accelerated validation subsequently ran on disposable EPYC 4245P / HWE
`7.0.0-38-generic` box `box-81a4de72`. The candidate and unchanged
production each passed the full in-box SVM suite. The exact pinned one-seed,
180-virtual-second Tempo workload passed in all three A/B/A runs:

| Module | Host wall time | Workload failures |
| --- | ---: | ---: |
| Candidate A | 167 s | 0 |
| Unchanged production B | 169 s | 0 |
| Candidate A repeat | 170 s | 0 |

The same driver SHA-256 was
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
The candidate therefore has **no established performance gain** and remains
isolated, not merged into production.

A diagnostic-only branch `dan/amd-third-successor-profile-1016` counted
offers and successful third-page selections. In a passing shortened
60-virtual-second run, the final logged checkpoint had 66,717 third-page
offers among 12,320,768 trusted-page plans (0.54%); 10,101 offers named the
hot scheduler page. The selection counter crossed 16,384, including 332
scheduler selections at that checkpoint; it was logged at powers of two,
so that is a lower bound rather than the final selection total. This
explains why an additional guarded successor is too infrequent to move the
full workload timing appreciably. The diagnostic contains a guest-specific
address solely for measurement and must not be merged.

Before that box became available, a boxctl
`m4-metal-small` box in FRA provisioned an EPYC 4244P. Its SVM suite failed
the control-flow acceleration exit-count assertion (`20,023` exits), so it
cannot be used for a fair Tempo comparison. A replacement `m4-metal-small`
in FRA and one in AMS also provisioned 4244P; they were deleted before
testing. A `m4-metal-medium` box in FRA provisioned EPYC 9124, whose SVM
features reported `pmu_virtualization=false`; its mixed workload therefore
fell back to instruction stepping and failed its expected-exit assertion.
That box was also deleted. FRA2 and LON returned out-of-stock errors for
`m4-metal-small`. All created boxes from this attempt were deleted.

Raw status files and the diagnostic dmesg excerpt are under ignored
`target/boxctl-evidence/box-81a4de72/third-successor-ab-a/` in the
production worktree. The production module was restored and passed its full
in-box SVM suite. No kernel module was built or loaded on the workstation.
