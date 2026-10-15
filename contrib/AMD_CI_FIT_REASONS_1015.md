# AMD pinned-CI fetch-fault follow-up (2026-10-15)

The earlier current-CI profile classified about 9.6% of sampled fetch faults
as apparently fitting the four-breakpoint budget and targeting a predicted or
recent page. This was only a necessary-condition check. A series of
diagnostic-only modules on disposable HWE box `box-02f5f816` traced the
actual global-plan branch and the target's recent-page rank. All six pinned
one-seed, 180-virtual-second Tempo DST diagnostic campaigns passed with
`failed=0`; the diagnostic runs took 181–193 host seconds. Each used seed
`14373434789495652599`, guest `vmlinux` SHA-256
`3feabfd845b13320fc499b008747dac1c7f9991a87029879cb10ba0f940b2336`,
and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
The runs sampled one in 256 instruction-fetch NPT faults in the same guest-TSC
window, 300–600 billion, as the preceding profile.

The final diagnostic campaign sampled 53,556 of 13,710,165 fetch faults:

| Classification | Samples | Share |
| --- | ---: | ---: |
| Cached target proof would exceed four breakpoints | 24,177 | 45.1% |
| Proof fits, target is an older recent page (rank 5–8) | 10,849 | 20.3% |
| Proof fits, target is outside the eight-page recent set | 8,111 | 15.1% |
| Proof fits, target is recent, source page is trusted | 4,547 | 8.5% |
| Apparent fit after a reused clock plan | 770 | 1.4% |
| Other (missing/unguarded proof, translation, capacity, etc.) | 5,102 | 9.5% |

The 4,547 trusted-source samples are **not planner rejections**. The current
trusted-source path considers a predicted hazardous successor only; it never
enters the general four-recent-page candidate loop. Of these samples, 1,618
targeted recent rank 1, 1,944 rank 2, and 985 rank 3. The 770 clock-plan
samples used a previously protected plan and likewise were not fresh
candidate-loop rejections. A plan-time trace found **zero sampled
apparent-fit targets rejected inside the normal candidate loop**. The
five-hazard reachable-region early-return path did not account for this group.
The remaining large class is the hardware breakpoint capacity limit, rather
than a missing condition in this planner loop.

To test a cheaper trusted-source policy, isolated branch
`dan/amd-ci-trusted-rank2-1015` admits only recent rank 2 after the existing
prediction. It retains the guarded code, alias, boundary, breakpoint-union,
and four-code-page checks of the earlier bounded widening experiment.
All 277 VMX library tests and the in-box SVM suite passed. On the same box,
the candidate and adjacent unchanged production control both passed the
identical pinned campaign in **190 host seconds**. The earlier four-recent-page
trusted widening also failed to improve its adjacent control (191 versus
189 seconds). These policies are not merged because neither establishes a
speed gain. The production module was restored and passed the in-box SVM
suite after the candidate run.

Diagnostic source is on `dan/amd-ci-fit-reasons-1015`; its raw histograms and
campaign logs are under the ignored
`/tmp/bedrock-amd-ci-fit-reasons-1015/target/boxctl-evidence/box-02f5f816/fit-reasons/`.
Candidate and control status files are under the ignored production-tree
`target/boxctl-evidence/box-02f5f816/ci-fit-and-rank2/`. No kernel module was
built or loaded on the workstation.
