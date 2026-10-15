# AMD fetch-fault predecessors (2026-10-15)

On disposable HWE `box-ad902887`, a diagnostic-only module recorded the raw
SVM exit immediately before each instruction-fetch NPT fault in the pinned
Tempo DST seed `14373434789495652599`. A second run recorded whether the two
exit RIPs were on the same virtual 4 KiB page and whether the predecessor
used a guarded global batch. Both 180-virtual-second campaigns passed with
`failed=0` in 191 host seconds. Their overhead makes those times unsuitable
for a production performance comparison. The userspace driver SHA-256 was
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.

In the later-window dump of the second run, 13.78 million fetch faults were
observed. The leading predecessors were:

| Previous raw SVM exit | Fetch faults | Same virtual page | Previous global batch |
| --- | ---: | ---: | ---: |
| NPT violation (`0x400`) | 9,017,592 | 104 | 8,966,557 |
| RDTSCP (`0x87`) | 2,786,064 | 0 | 2,786,064 |
| RDTSC (`0x6e`) | 1,142,363 | 0 | 1,142,362 |
| Debug exception (`0x41`) | 731,619 | 27,167 | 133,517 |

The first predecessor run had the same ordering: 9.07 million NPT,
2.81 million RDTSCP, and 1.16 million RDTSC. The large clock-followed
fetch count does **not** imply a missed same-page clock lease: all those
fetch RIPs were on another virtual page, and the previous clock entry had a
global batch. Repeated NPT-to-NPT transitions were likewise almost always
cross-page. An execute-grant improvement must admit and protect additional
hazardous pages, rather than simply holding the current page longer.

Older fault-weighted admission profiling in
`workloads/tempo/native-comparison/amd-profile-2026-10-10.md` found that
predicted pages were usually rejected because their combined breakpoint
proof exceeded AMD's four debug-register slots; removing the preliminary
raw-hazard gates did not lower faults. A safe next implementation needs an
adaptive multi-page proof that covers **all** executable aliases and fits the
four breakpoint slots, or a different precise-retirement mechanism. NPT
execute permissions without their revocable guard tokens are unsafe; the
earlier apparent 101-second result depended on exactly that token bug.

Raw kernel histograms are retained in the workspace under
`target/boxctl-evidence/box-ad902887/fetch-predecessor/`. All module builds
and loads occurred in the box.
