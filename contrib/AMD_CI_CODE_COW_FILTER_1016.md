# Keep AMD table filters through code-only COW

The forked VM used to dirty the guarded guest page-table tree and clear every
cached leaf-table filter whenever COW touched a page that might contain
code. The guest page-table entries are unchanged by a COW remap of a code
GPA. A 60-virtual-second Tempo DST diagnostic counted about 6,500 such
code-page COWs among more than 7,000 whole-filter invalidations, while
about 1,100 COWs actually touched page-table GPAs. Fresh alias walks in
two large roots sometimes re-read hundreds of leaf tables after those
cache clears.

This change keeps the guarded table tree and leaf filters when the remapped
GPA is not a known table, the gate is complete and clean for the current
CR3, no host table write is pending, and NPT mapping generations match.
It revokes trusted-code state before replacing the old NPT leaf, clears the
affected code-byte memo and hints, advances the code-plan epoch, and updates
the unchanged table guards' mapping generations. Other COW cases retain
the conservative full refresh.

All 279 `bedrock-vmx` unit tests passed, including tests of code-memo
revocation and fast-path rejection on dirty, untrusted, empty, stale, or
wrong-root gates. The disposable `boxctl` SVM suite passed. On EPYC 4245P
box `box-b0acd740` with Linux `7.0.0-38-generic`, the pinned one-seed
180-virtual-second Tempo DST workload passed in 180 and 175 wall seconds
with the final guarded code (the latter on the pushed production commit).
Earlier candidate builds passed in 172 and 177 seconds. Production controls
on the same box passed in 184, 194, and 188 seconds. The 180-second final
candidate run was adjacent to the 188-second production control. These
measurements still include host variation; they establish a repeatable
improvement on this pinned workload, not a universal speedup estimate.
The shortened 60-virtual-second candidate run reported
`E4/graceful-stop` at its shortened deadline, so it is not used for the
performance verdict.

The reported Intel CI run of about 65 seconds used different host hardware
and guest images. This change reduces a general AMD software cost but does
not establish parity with Intel.
