# AMD stable guarded-tree generation trial (2026-10-16)

A diagnostic of the pinned Tempo DST workload found that conditional NPT
execute grants are often revoked after a tree-generation change. On a
disposable EPYC 4245P / HWE `7.0.0-38-generic` box, an intact global
guarded tree was reused at least 196,608 times in a passing
60-virtual-second run. The bounded-guard reuse path advanced
`tree_generation` even when the same root and write-protected table tree
remained in place.

This isolated hypervisor-only trial preserves that generation when the
root, NPT mapping, and guarded-tree write state are unchanged. A released
table guard marks the global gate dirty, and a host table write marks its
links untrusted; either condition retains the original generation bump.
The focused stable/dirty/host-write unit test and all 282 VMX library tests
passed, as did the full SVM suite inside the box. The exact pinned
180-virtual-second seed `14373434789495652599` passed in all three runs
(`failed=0`) with the same driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`:

| Module, run order | Host wall time |
| --- | ---: |
| Initial candidate with full guard-array scan | 171 s |
| Unchanged production `4c1517f` | 166 s |
| Candidate without per-reuse guard-array scan | 167 s |

The cheaper candidate did not show a speedup against the adjacent control,
so this change remains isolated. The root-switch diagnostic also showed
that every root change coincided with a tree-generation change; this
same-root optimization cannot address that larger class. Status files are
under ignored `target/boxctl-evidence/box-2b77e4f7/stable-tree/` in this
worktree. Kernel modules were built and loaded only on the disposable box.
