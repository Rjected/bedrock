# Inactive-root alias-proof filter trial, 2026-10-16

This branch tracks a conservative Bloom filter of the guarded page-table
frames for each recent CR3 root. A guest write to an inactive table retains
a cached executable-alias proof only if the proof's filter is current and
proves the table absent. A missing, evicted, or positive filter invalidates
the proof. Filters are unioned across rebuilds so a retained proof cannot
forget a page-table frame added later.

The 279 local `bedrock-vmx` tests and the SVM box suite passed on an EPYC
4245P box with Linux `7.0.0-38-generic`. The pinned 180-virtual-second DST
seed passed in 187 seconds with the initial implementation and 185 seconds
after caching each root's Bloom lookup within a write. Adjacent production
on the same box passed in 184 seconds. A 60-virtual-second diagnostic run
reported 75,134 retained proof instances among 96,344 inspected at 8,192
inactive-table write events; this counts repeated survival of a proof, not
distinct saved walks. It passed in 108 seconds with instrumentation.

The filter has no demonstrated full-workload speedup and is not included
in the production AMD branch. Another disposable box with an EPYC 4244P
failed the existing control-flow acceleration gate on both candidate and
production and was excluded from the timing comparison.
