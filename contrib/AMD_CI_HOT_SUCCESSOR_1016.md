# AMD frequent-successor guarded execution (2026-10-16)

The pinned Tempo DST profile found that a hot scheduler page repeatedly
fetched after two other kernel pages even when their counted global plans had
free breakpoint slots. The single latest-successor prediction oscillated
between common destinations. A production hypervisor-only change now retains
two frequent page successors per guarded root and source page, independently
of the latest edge. A bounded two-counter sketch updates on observed page
transitions, decays old observations, and offers a second candidate only when
its frequency is sustained. It does not encode guest page addresses or
modify guest code.

The trusted-source planner validates the second candidate's current code
translation, exact hazardous bytes, cached executable-alias proof, page
boundary, and four-breakpoint budget. It grants NPT execute permission only
for that counted interval, alongside the first predicted page. Any candidate
that fails a proof remains NX and faults before it can execute. The new
breakpoint union retains each alias proof's address mask; one existing range
may replace a new range only when it covers that entire range.

All 282 `bedrock-vmx` library tests passed, including focused tests for
two-way prediction and two hazardous successors. The complete in-box SVM
suite passed before each full candidate run on disposable EPYC 4245P / HWE
`7.0.0-38-generic` box `box-ad3b5fef`. The exact pinned 180-virtual-second
Tempo DST seed `14373434789495652599` passed in an adjacent same-box A/B/A
sequence with the same guest assets and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`:

| Module | Verdict | Host wall time |
| --- | --- | ---: |
| Frequent-successor candidate | `failed=0` | 161 s |
| Unchanged production `1d270e1` | `failed=0` | 173 s |
| Frequent-successor candidate, repeat | `failed=0` | 165 s |

The same seed passed a shortened 60-virtual-second campaign in 97 seconds,
within the 95–99-second range of diagnostic controls; the full campaign
provides the clearer signal because guest startup occupies much of a short
run. The A/B/A sequence supports an approximately 8–12-second gain on this
seed. It does not establish Intel-equivalent performance: the reported Intel
run used different host hardware and guest images.

After cherry-picking the candidate into production as `23f7e79`, the exact
pushed tree passed the complete in-box SVM suite, all 282 VMX and 19 EPT
library tests, and a full-duration two-seed campaign. Both seeds
`14373434789495652599` and `14373434789495652600` passed (`failed=0`) in
335 host seconds total. The second seed uses a different preemption setting;
the aggregate two-seed timing is a correctness result, not an A/B speed
comparison with the one-seed control.

Raw status and campaign logs are under ignored
`target/boxctl-evidence/box-ad3b5fef/` in this worktree and on the box until
its lease expires. All kernel-module builds and loads occurred inside the
disposable `boxctl` box; none occurred on the workstation.
