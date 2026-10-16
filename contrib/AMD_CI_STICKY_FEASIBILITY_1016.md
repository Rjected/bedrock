# AMD guarded-page successor feasibility (2026-10-16)

This diagnostic-only branch sampled the pinned Tempo DST guest on disposable
EPYC 4245P / HWE `7.0.0-38-generic` box `box-ad3b5fef`. The production code
base was `1d270e1`; the exact guest assets and userspace driver matched the
preceding conditional-boundary run. All module builds and loads were inside
the box. The in-box SVM suite and three 60-virtual-second one-seed diagnostic
campaigns passed with `failed=0` in 97, 99, and 95 host seconds. Diagnostic
timings are not production A/B measurements.

The leading remaining one-hazard page is physical `0x1feb000`, with an
apparent hazardous offset `0xe86` inside an immediate operand preceding hot
`__schedule` code. At the first diagnostic's final checkpoint, 1,277,952
instruction-fetch faults targeted this page. The preceding global plan had
zero, one, two, three, or four occupied breakpoint slots for 548,397, 153,094,
362,273, 145,135, and 68,555 faults respectively; 498 followed a non-global
entry. Thus 94.6% of these faults followed a counted global plan with at least
one free slot. This is only a capacity bound: the page also needs a guarded
alias proof, safe page boundaries, and a predictor that selects it before
entry.

A one-in-4,096 sample of 1.26 million faults in the second campaign associated
137/307 with preceding plan page `0x1fe5000`, and 90/307 with `0x1258000`.
The third campaign counted page transitions from their corresponding virtual
pages. After 1,441,792 transitions from `0xffffffff81fe5000`, 512,213 (35.5%)
went to the scheduler page. After 1,835,008 transitions from
`0xffffffff81258000`, 393,799 (21.5%) went there. In a one-in-4,096 transition
sample from the first source, the two leading destinations were
`0xffffffff81258000` (156/363) and `0xffffffff81feb000` (141/363). The current
single latest-successor cache loses one of those edges whenever the other
occurs. These observations motivate testing a general two-candidate
successor predictor instead of keeping a breakpoint armed across every entry.

An isolated, guest-specific sticky-breakpoint prototype on
`dan/amd-sticky-onehazard-1016` passed the full in-box suite and pinned
60-virtual-second campaign, but took 100 seconds versus 97 seconds for the
adjacent diagnostic control. That does not establish a gain and the prototype
is not merged. It repeatedly revoked and re-promoted its grant on entries
whose breakpoint plan was full or not global. A later counter run was stopped
after a per-entry atomic diagnostic distorted its timing; it is not a
performance result.

Raw logs and status files are under ignored
`target/boxctl-evidence/box-ad3b5fef/` in this worktree. The fixed page
addresses and counters in this branch are for diagnosis only.
