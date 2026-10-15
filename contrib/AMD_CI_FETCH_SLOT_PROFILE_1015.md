# AMD pinned-CI fetch-fault capacity profile (2026-10-15)

On disposable HWE box `box-23ce62df`, diagnostic-only module builds sampled
one in 256 instruction-fetch NPT faults during guest-TSC window 300–600
billion of the pinned 180-virtual-second Tempo DST campaign. Both completed
diagnostic campaigns passed with `failed=0` in about 190–192 host seconds.
They used seed `14373434789495652599`, guest `vmlinux` SHA-256
`3feabfd845b13320fc499b008747dac1c7f9991a87029879cb10ba0f940b2336`,
and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
Diagnostic timings are for correctness, not a production speed comparison.

The first completed profile sampled 54,285 of 13,896,821 fetch faults.
Only 772 samples followed a non-global entry; **21,229/53,513 global samples
(39.7%)** had all four debug-register slots occupied. The slot counts for
the other global samples were 5,927 with zero, 9,116 with one, 6,972 with
two, and 10,269 with three. The leading faulting guest-physical pages were
`0x1feb000` (6,696 samples), `0x1398a2000` (5,390), `0xfd49000` (4,337),
and `0x1fe3000` (3,905). No sampled fault target was already admitted to
its preceding global plan.

A read-only target classifier in the second completed profile sampled
54,480 of 13,946,799 fetch faults. It checked the target's current physical
translation, guarded hazard memo, cached single-page executable-alias proof,
and actual breakpoint union with the preceding plan:

| Classification | Samples | Share |
| --- | ---: | ---: |
| Alias proof would exceed four slots | 25,138 | 46.1% |
| Proof fits, target is an older recent page (rank 5–8) | 10,726 | 19.7% |
| Proof fits, target is outside the eight-page recent set | 8,240 | 15.1% |
| Proof fits, target is predicted or among the four newest pages | 5,222 | 9.6% |
| Target memo not currently guarded/valid | 2,678 | 4.9% |
| Guarded memo exists, cached alias proof absent | 1,352 | 2.5% |
| Other (non-global, translation mismatch, no memo, raw/page cap) | 1,124 | 2.1% |

"Fits" is a necessary condition, not proof of a missed optimization: the
planner also validates code translation and candidate order, and adding a
page can displace a more useful one. The apparent-fit category among the
four newest pages needs finer planner-rejection attribution before changing
admission rules. An initial fit-classifier run incorrectly filtered on exit
reason 37 instead of the decoded NPT reason 48; its zero counts were
discarded and that run was stopped.

To test the older-recent opportunity directly, isolated hypervisor variants
screened five or eight recent pages after the predicted successor; unchanged
production screens four. All variants passed 276 VMX library tests and the
in-box SVM suite. The same box ran exact pinned campaigns with the same
driver, guest, initrd, images, and seed:

| Variant / run order | Failed | Host wall time |
| --- | ---: | ---: |
| Eight recent pages | 0 | 192 s |
| Adjacent four-page control | 0 | 189 s |
| Five recent pages | 0 | 187 s |
| Adjacent four-page control | 0 | 190 s |
| Five recent pages, repeat | 0 | 193 s |

The eight-page scan regressed in its adjacent pair. The five-page trial's
initial three-second gain reversed on repeat. Neither policy establishes a
reproducible improvement and neither is merged. Source is preserved on
`dan/amd-slot-profile-ci-1015`, `dan/amd-ci-recent8-1015`, and
`dan/amd-ci-recent5-1015`; raw logs and histograms are under ignored
`target/boxctl-evidence/box-23ce62df/ci-slot-and-recent-trials/`.
The production module was restored and passed the full in-box SVM suite.
No kernel module was built or loaded on the workstation.
