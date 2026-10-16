# AMD CI alias-walk and COW diagnostic, 2026-10-16

This branch adds temporary kernel counters and bounded walk samples. It is
diagnostic only. All modules were built and loaded inside disposable
`boxctl` box `box-b0acd740` (EPYC 4245P, Linux `7.0.0-38-generic`). The
same pinned Tempo DST guest assets and seed were used throughout.

A 60-virtual-second run passed in 107 wall seconds. It logged 152 sampled
successful alias walks: median 47,307 TSC cycles; mean 96,077; 90th
percentile 128,115. The two most expensive roots accounted for 61 samples
and 76% of sampled walk cycles. Their guarded trees usually had about 700
table pages. More than 100 leaf-table reads occurred in 12 samples and
accounted for 40% of sampled cycles; three reads of more than 500 leaf
tables accounted for 19%. These samples are for attribution, not a
whole-run cycle breakdown.

A second 60-virtual-second run passed in 109 wall seconds and counted more
than 7,168 whole-cache leaf-filter invalidations. A follow-up classified
COWs on the same workload. At its final sampled point it counted 142,336
COW pages: 134,758 ordinary data, 6,487 possible code pages, and 1,091
page-table pages. The 7,168th leaf-filter invalidation occurred during
that run, so possible-code COW is the dominant avoidable cause of cache
clears. Feedback-buffer COW and I/O-channel COW were rare. The classified
short run reported `E4/graceful-stop` at its shortened deadline; the
subsequent full 180-virtual-second candidate run passed. This diagnostic
branch must not be merged into production.
