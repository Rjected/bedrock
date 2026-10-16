# AMD scalar faults after frequent-successor planning (2026-10-16)

Diagnostic-only counters based on production `39f7cc3` ran on disposable
EPYC 4245P / HWE `7.0.0-38-generic` box `box-ad3b5fef`. Three pinned
60-virtual-second Tempo DST campaigns passed (`failed=0`) in 101, 98, and
100 host seconds, using seed `14373434789495652599`, the same guest assets,
and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
Diagnostic wall times are not production performance comparisons.

At the first campaign's final million-fault checkpoint, the diagnostic
recorded 12,582,912 scalar instruction-fetch NPT faults: 6,210,324 on
one-hazard pages, 2,481,009 on two-hazard pages, 1,912,434 on three-hazard
pages, 1,512,344 on four-hazard pages, and 400,921 on pages with five or
more hazards. In a one-in-8,192 sample, 294/775 one-hazard faults had their
sole hazard at page offset `0xfff`. The leading one-hazard page/offset pairs
were `0x1feb000`/`0xe86` (135 samples), `0x158b000`/`0xfff` (45),
`0x1371000`/`0xfff` (43), and `0x1310000`/`0xfff` (37). These are physical
page addresses in the pinned guest, not production page-specific rules.

The second campaign measured the existing conditional boundary-code grant.
At its last checkpoint, it attempted 786,432 promotions: zero hit the
16-page capacity, zero found the page already granted, 7,223 were blocked
because the next page was also conditional, and 779,209 were otherwise
eligible. Capacity is therefore not the cause of the recurring boundary
faults in this workload.

At its last checkpoint, the third campaign classified 458,752
conditional-grant revocations by the first failing dependency check:

| Reason | Count | Share |
| --- | ---: | ---: |
| Guest CR3 changed | 153,379 | 33.4% |
| Pending host write | 124,180 | 27.1% |
| Guarded translation tree changed | 96,553 | 21.0% |
| Code proof epoch changed | 52,031 | 11.3% |
| Translation gate unavailable/dirty | 25,796 | 5.6% |
| NPT mapping generation changed | 6,172 | 1.3% |
| Trusted-code generation changed | 641 | 0.1% |

Several dependencies can change together, so the table is an ordered
classification, not an independent causal decomposition. It shows that
enlarging the page array or merely ignoring unrelated trusted-code
transitions would miss the dominant invalidations. Any grant retained across
CR3 or translation-tree changes must prove every executable alias and safe
continuation in the new root before allowing NPT execution.

Raw `dmesg` is under ignored
`target/boxctl-evidence/box-ad3b5fef/` in this worktree. The production
module was restored and passed the full in-box SVM suite before box expiry.
No kernel module was built or loaded on the workstation.
