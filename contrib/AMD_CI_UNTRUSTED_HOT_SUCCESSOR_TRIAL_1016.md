# AMD untrusted-source successor trial (2026-10-16)

The merged frequent-successor predictor admits a second guarded hazardous
page when the current page is globally trusted. This isolated follow-up also
tries the learned second successor when the current page is hazardous, before
screening four recent pages. It retains the existing guarded translation,
exact-byte hazard, executable-alias, boundary, and four-breakpoint checks.

All 282 VMX library tests passed, including an added case where both learned
successors are absent from the recent-page set. The complete SVM box suite
passed on disposable EPYC 4245P / HWE `7.0.0-38-generic` box
`box-ad3b5fef`. The exact pinned 180-virtual-second Tempo DST seed
`14373434789495652599` and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`
passed with `failed=0` in both adjacent campaigns:

| Module | Host wall time |
| --- | ---: |
| Untrusted-source extension | 175 s |
| Unchanged production `39f7cc3` | 168 s |

The 7-second regression agrees with the earlier 161- and 165-second runs of
the unchanged frequent-successor implementation on this box. Adding a
predicted hazardous page to the untrusted-source plan does not produce a
speed gain here, so this branch is not merged into `dan/amd-tempo-dst`.
Raw status and campaign logs are under ignored
`target/boxctl-evidence/box-ad3b5fef/` in this worktree. All kernel-module
builds and loads occurred only inside the disposable box.
