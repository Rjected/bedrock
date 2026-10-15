# AMD trusted-page successor widening trial (2026-10-15)

The global counted path already admits several guarded hazardous pages when
the current page is hazardous. When the current page is globally trusted, it
only admits one predicted hazardous successor. An isolated hypervisor-only
candidate also considered recent hazardous pages. It composed their cached
single-page alias proofs, retained exact and masked breakpoint entries, and
kept the four AMD debug-register limit and boundary checks. It did not alter
the guest, workload images, or userspace driver.

The first version could admit five code pages: one trusted page plus four
guarded successors. The VM runner has only three paired NPT execute-grant
slots beyond the current page. Indexing a fourth paired grant would exceed
its fixed array. Unit tests and the SVM hardware suite passed, but the first
full campaign on `box-32a04d2e` lost SSH/Tailscale connectivity after seed
start. The box was still reported `ready` by boxctl; no campaign verdict or
kernel log was recoverable. The five-page bug is a plausible cause, not a
confirmed diagnosis of that box failure. This version must not be used.

The candidate now caps global plans at four code pages and the runner checks
the cap before indexing its grants. A unit test constructs five eligible
pages and verifies that only four are selected. All 277 VMX library tests
and the disposable-box SVM suite passed. The bounded candidate then passed
the exact pinned one-seed Tempo CI campaign on HWE `box-23ce62df`:

| Same-box module | Failed | Host wall time |
| --- | ---: | ---: |
| Bounded successor widening | 0 | 191 s |
| Unchanged production control | 0 | 189 s |

Both used guest `vmlinux` SHA-256
`3feabfd845b13320fc499b008747dac1c7f9991a87029879cb10ba0f940b2336`,
the same initrd, OCI images, compose file, seed `14373434789495652599`,
and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
The two-second difference is within run variation and does not establish a
speedup. The candidate is not merged; its source is on
`dan/amd-trusted-widen-1015`. Raw campaign statuses, logs, and summaries are
under ignored `target/boxctl-evidence/box-23ce62df/trusted-widen/`.
Kernel modules were built and loaded only inside disposable boxes.
