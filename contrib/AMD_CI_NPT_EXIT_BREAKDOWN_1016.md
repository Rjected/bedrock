# AMD pinned-CI NPT fault breakdown (2026-10-16)

An isolated diagnostic module based on production `5fc001a` counted nested-page
fault paths on disposable EPYC 4245P / HWE `7.0.0-38-generic` box
`box-0cab28bc`. The local 279-test `bedrock-vmx` suite and the in-box SVM
suite passed. Both the shortened 60-virtual-second diagnostic and exact
180-virtual-second pinned Tempo DST seed `14373434789495652599` passed with
`failed=0`. The full run took 178 host seconds with instrumentation. The
guest assets and userspace driver were unchanged; the driver SHA-256 was
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.

The counters are process-global and log every 1,048,576 NPT exits. They do
not reset between campaigns, so the second run's figures below subtract the
first run's last logged checkpoint. Because each checkpoint can precede its
campaign end by up to 1,048,575 exits, these figures are approximate.

| NPT path | Short run, last checkpoint | Full run, additional exits |
| --- | ---: | ---: |
| Scalar execute page | 15,121,180 | ~32,197,110 |
| Trusted execute page | 221,495 | ~245,786 |
| Protected page-table write | 3,962,767 | ~4,214,142 |
| COW | 150,506 | ~179,699 |
| Other, including devices | 464,038 | ~909,656 |
| All NPT exits | 19,922,944 | ~37,748,736 |

Scalar execute pages account for about 76% of short-run NPT exits and 85% of
the additional full-run NPT exits. Protected page-table writes account for
about 20% and 11%, respectively. Recurrent-code write and trusted-code write
faults are each under 0.02%. This identifies the scalar execute path as the
main remaining NPT-exit source, rather than trusted-code invalidation or COW.
It does **not** imply each scalar fetch can safely be removed: earlier
candidate profiling found that many cross-page plans exceed AMD's four
breakpoint slots, and broad address masks produced expensive false hits.

Raw kernel counter lines and campaign statuses are retained under ignored
`target/boxctl-evidence/box-0cab28bc/npt-breakdown/` in this worktree. This
instrumentation is for diagnosis and must not be merged into production. No
kernel module was built or loaded on the workstation.
