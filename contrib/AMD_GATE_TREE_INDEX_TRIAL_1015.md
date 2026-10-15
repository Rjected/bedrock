# Guarded translation-tree index trial (2026-10-15)

A stalled run of the experimental PMC scheduler-clock guest spent 36.1% of
799 `perf` samples in `collect_translation_tree_pages`. That collector
linearly searched up to 1,024 guarded table pages for each upper-level table
it encountered. An isolated hypervisor-only trial used the existing
`gate_table_index` to find those guarded pages, retaining the exact linear
fallback while the index is invalid during rebuilds.

All 276 `bedrock-vmx` unit tests and the disposable box's SVM suite passed.
On `box-ad902887` (HWE 7.0.0-38), both variants used the same patched guest,
initrd, images, workload planner, pinned seed `14373434789495652599`, and
userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
The only intended difference was the loaded Bedrock module:

| Workload | Unchanged module | Indexed collector |
| --- | ---: | ---: |
| 60-virtual-second diagnostic | 110s, pass | 108s, pass |
| 180-virtual-second pinned DST | 193s, pass | 193s, pass |

The full result is a tie. The index change is not merged. The stall profile
identifies where CPU time went after guest progress stopped, but does not
show that this lookup causes the stall or materially limits normal DST speed.
The code remains on experimental branch `dan/amd-gate-tree-index-1015` for
inspection. No module was built or loaded on the workstation.
