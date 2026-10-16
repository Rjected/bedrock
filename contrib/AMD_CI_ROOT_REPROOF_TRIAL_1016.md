# AMD conditional execute reproof across CR3 (2026-10-16)

A diagnostic found that the destination guarded root already held a
single-page alias proof for every conditionally executable page in 495 of
576 sampled CR3 switches. This isolated hypervisor-only trial checks those
proofs, retranslates each page's sole virtual continuation under the new
root, verifies that the next physical page is globally trusted, and
rereads the cross-page bytes before retaining the old NPT execute grants.
Any failed check falls back to the existing full revocation. No guest
image or workload rule changed.

The focused new-root, extra-alias, and unsafe-continuation unit test passed,
as did all 283 VMX library tests and the complete SVM suite inside
disposable EPYC 4245P / HWE `7.0.0-38-generic` box `box-2b77e4f7`.
The pinned 60-virtual-second seed passed in 95 host seconds. In that
diagnostic, the first 81,920 eligible reproof attempts had 76,737
successes. The full pinned 180-virtual-second seed
`14373434789495652599` also passed (`failed=0`). Both arms used the same
guest assets and driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`:

| Module, run order | Host wall time |
| --- | ---: |
| Root-reproof candidate | 168 s |
| Unchanged production `46749ac` | 166 s |

Matched 60-virtual-second progress runs each took 96 host seconds and
passed. Their last branch-VM snapshots were slightly offset in guest
virtual time (candidate 109.564 s, control 107.991 s). The candidate
recorded 14.454 million exits and 7.315 million NPT faults; the control
recorded 14.565 million exits and 7.306 million NPT faults. This shows no
large reduction in NPT traffic despite frequent successful reproof. The
candidate is not merged because it has no demonstrated wall-time gain and
does not address the dominant fetch-fault class.

The raw statuses, progress logs, and reproof counters are under ignored
`target/boxctl-evidence/box-2b77e4f7/root-reproof/` in this worktree.
All kernel modules were built and loaded only inside the disposable box.
