# AMD pinned-CI exits after frequent-successor planning (2026-10-16)

Production `39f7cc3` passed the exact pinned one-seed, 180-virtual-second
Tempo DST workload (`failed=0`) on disposable EPYC 4245P / HWE
`7.0.0-38-generic` box `box-ad3b5fef`. The progress driver logged
ten-virtual-second snapshots and took 173 host seconds; its userspace binary
SHA-256 was `f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
Its last branch-VM snapshot at virtual time 233.192 seconds reported:

| Exit/cost | Value | Share |
| --- | ---: | ---: |
| All VM exits | 42,813,339 | 100% |
| Nested-page faults | 21,837,491 | 51.0% of exits |
| RDTSC + RDTSCP | 13,984,167 | 32.7% of exits |
| Synthetic MTF exits | 5,512,847 | 12.9% of exits |
| VM-entry preparation | 156.93 billion TSC cycles | 37.1% of recorded run cycles |
| Guest/VM-run interval | 226.99 billion TSC cycles | 53.7% of recorded run cycles |
| Recorded run total | 422.85 billion TSC cycles | 100% |

The earlier [production exit profile](AMD_CI_PRODUCTION_EXIT_PROFILE_1016.md)
captured 47.05 million exits, including 26.02 million NPT faults, at virtual
time 233.734 seconds before both guarded boundary trust and the
frequent-successor change. Those profiles used different boxes and snapshot
times, so the roughly 4.2-million-fault reduction is directional and cannot
be assigned to the predictor alone. TSC-read exits remained near 14 million.
NPT instruction-fetch faults and the cost of preparing their next entries
remain the largest target for AMD workload performance.

The separate diagnostic branch `dan/amd-postsuccessor-scalar-1016` sampled
remaining fetch faults and boundary grants. In a short pinned run, 294/775
sampled one-hazard faults had their sole offset at `0xfff`. The 16-page
conditional-grant capacity was never reached in 786,432 attempts. Ordered
revocation counters instead found CR3 changes (33%), pending host writes
(27%), and guarded translation-tree changes (21%) most often. These counters
point toward safe proof reuse across address-space and mapping changes; a
new root must be proved before an NPT execute grant can remain active.

A follow-up that also admitted frequent successors from hazardous source
pages passed but regressed to 175 seconds versus an adjacent 168-second
production control on this box. Its source and evidence are on rejected
branch `dan/amd-hot-successor-untrusted-1016`; that extension is not merged.

The progress campaign log and status are under ignored
`target/boxctl-evidence/box-ad3b5fef/postsuccessor-profile/` in this
worktree. All kernel-module builds and loads were confined to the disposable
box. The production module was restored and passed its full in-box SVM suite
before the box expired.

On the next disposable HWE box, an overlap diagnostic found that all
147,608 root-change revocations at its last checkpoint also changed the
guarded-tree generation. A [same-root generation trial](AMD_CI_STABLE_TREE_TRIAL_1016.md)
passed the pinned workload, but its cheaper variant took 167 seconds
versus a 166-second adjacent unchanged-production control. It remains
isolated on `dan/amd-stable-tree-1016`; the overlap diagnostic is on
`dan/amd-postsuccessor-scalar-1016`. The larger root-switch class still
requires a proof that is valid in the new address space.

A [destination-root proof diagnostic](AMD_CI_ROOT_REPROOF_FEASIBILITY_1016.md)
found cached alias proofs for all conditional pages in 495/576 sampled root
switches. A [full reproof trial](AMD_CI_ROOT_REPROOF_TRIAL_1016.md)
successfully retained many grants, but took 168 seconds versus a 166-second
same-box production control. Matched short progress runs showed little
change in total NPT faults. The trial remains isolated on
`dan/amd-root-conditional-reproof-1016`; the root-switch revocations are
not the main remaining fetch-fault source.

A [one-hazard reachable-region probe](AMD_CI_ONE_HAZARD_REGION_FEASIBILITY_1016.md)
found a zero-breakpoint region for about 2% of sampled guarded entries,
with none of the logged zero regions on the hot scheduler page. Broadly
applying that prover to one-hazard pages would mostly exchange an exact
hazard breakpoint for one or more frontier stops, so no execution change
was merged.

A [post-predictor scheduler diagnostic](AMD_CI_POSTPREDICT_SCHEDULER_1016.md)
found that the hot scheduler page still faulted nearly one million times
in a passing short campaign. It usually followed a global plan with a
free breakpoint slot. A later diagnostic sampled the two leading source
pages and found that nearly all plans naming the scheduler page had an
available alias proof and breakpoint budget; plans that did not name it
dominated the sample. That later campaign failed on a one-instruction PMU
budget overrun, so its counts are hypothesis-generating, not a valid
performance or correctness comparison. Successor selection and the PMU
boundary failure both need follow-up.

A [third guarded successor candidate](AMD_CI_THREE_SUCCESSOR_TRIAL_1016.md)
addresses the observed selection gap when the second-order prediction differs
from both frequent destinations. It passed 283 local library tests and was
pushed on isolated branch `dan/amd-three-successor-1016`. Accelerated hardware
validation is pending because the available disposable boxes provisioned
EPYC 4244P or EPYC 9124 rather than the previously validated EPYC 4245P.
