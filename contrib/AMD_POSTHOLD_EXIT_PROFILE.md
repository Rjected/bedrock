# AMD exit mix after multi-page clock hold

On 2026-10-15, `box-266a001a` ran the production AMD branch after the
token-backed multi-page execute hold was merged. This was a **60-virtual-second
diagnostic interval** of the pinned Tempo DST workload, using the patched
guest, HWE `7.0.0-38-generic`, the progress driver, and seed
`14373434789495652599`. The campaign passed with `failed=0` in 118 host
seconds, but its shorter duration must not be compared with the 180-second
Intel CI workload or the full AMD campaign.

At virtual second 108.251, after the warm checkpoint at virtual second
48.251, the guest had produced **15,082,664 exits**:

| Exit class | Count | Share |
| --- | ---: | ---: |
| NPT violations | 7,491,866 | 49.7% |
| Monitor-trap/single-step | 2,654,358 | 17.6% |
| RDTSC + RDTSCP | 4,400,918 | 29.2% |
| Remaining | 535,522 | 3.5% |

The corresponding host counters reported 80.81 billion entry-preparation
cycles, 78.46 billion guest cycles, and 174.07 billion total RUN-loop cycles.
The NPT and monitor-trap counts show that optimizing only clock-plan reuse
cannot close the remaining gap to the reported Intel timing. Earlier fault
classification found most NPT exits were code fetches from untrusted pages
and guarded page-table writes. The next useful experiment should identify
which current code transitions and scalar replay paths contribute most to
these two categories under the corrected permission-lifetime implementation.
The raw progress log is retained under
`target/boxctl-evidence/box-266a001a/posthold-exits/` in this workspace.
