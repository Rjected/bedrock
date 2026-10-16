# AMD scalar fetches after guarded boundary trust (2026-10-16)

An isolated diagnostic based on production `1d270e1` ran the pinned
60-virtual-second Tempo DST seed on disposable EPYC 4245P / HWE
`7.0.0-38-generic` box `box-5e63aad0`. The 280 local VMX tests, in-box SVM
suite, and seeded run passed (`failed=0`). The guest assets and userspace
driver matched the preceding scalar-cause diagnostic.

At the last million-exit checkpoint, the new module recorded 12.58 million
scalar instruction-fetch NPT faults, including 6.36 million on one-hazard
pages. The earlier production `5fc001a` diagnostic on this same box recorded
13.63 million and 7.51 million, respectively, at its last short-run
checkpoint. These are separate seeded campaigns with checkpoint rounding,
so the roughly one-million-fault reduction is directional, not an exact
paired count.

One in 8,192 scalar fetches were sampled for page and hazard offset. The
sole hazard was at offset `0xfff` in **255/793 (32.2%)** of the new
one-hazard samples, versus **494/1,067 (46.3%)** before conditional boundary
trust. This supports the intended mechanism: more boundary-proven pages
retain execute permission and stop repeated fetch faults. Remaining scalar
faults, including other one-hazard pages, still dominate the AMD execution
cost. The absolute diagnostic runtime is not a production speed comparison.

The leading remaining sampled one-hazard pair was physical page `0x1feb000`,
offset `0xe86` (184 samples). Its hot fetch RIPs resolved against the pinned
guest `vmlinux` to `__schedule` around `0xffffffff81febed0`. The apparent
`REP` bytes at `0xffffffff81febe86` are inside an immediate operand of a
`mov` in the preceding `is_mmconf_reserved` function. This is a cold false
hazard sharing a page with hot scheduler code; dropping its guard still needs
a proof that no guest entry can jump into those bytes. A page-local byte scan
alone cannot establish that.

Raw counter lines are under ignored
`target/boxctl-evidence/box-5e63aad0/postboundary/dmesg.txt` in this
worktree. No kernel module was built or loaded on the workstation.
