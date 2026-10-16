# AMD page-scoped global-plan cache trial (2026-10-16)

The earlier exact-RIP global-plan cache was safe but showed no repeatable
CI gain. This isolated candidate changes its key from the exact instruction
pointer to the virtual code page. Reuse requires the same CR3, recent-page
set, predicted successor, guarded translation tree, code and mapping
generations, and live alias proofs for admitted hazardous pages. The current
entry offset must not be a hazardous instruction start. A reused batch gets
the new RIP and instruction deadline. The page and its translations remain
guarded under the same proof as the exact-RIP candidate.

The targeted new entry-offset test and all 277 VMX library tests passed, as
did the complete in-box SVM suite. The pinned one-seed, 180-virtual-second
Tempo CI workload passed (`failed=0`) in all four alternating runs on the
same disposable EPYC 4245P / HWE 7.0.0-38 box. The guest initrd and userspace
driver hashes were unchanged.

| Run order | Module | Host wall time |
| --- | --- | ---: |
| 1 | Page-scoped cache | 191 s |
| 2 | Production control | 202 s |
| 3 | Page-scoped cache | 189 s |
| 4 | Production control | 191 s |

The second control matched the repeat candidate, so the first control's
202-second time does not establish a cache gain. This experiment does not
close the approximate 190-second AMD versus 65-second Intel CI gap. The
candidate is not merged into production `dan/amd-tempo-dst`; source is on
`dan/amd-ci-page-plan-cache-1016`. Raw status files are under ignored
`target/boxctl-evidence/box-e675c0c1/ci-page-plan-cache/`. Kernel modules
were built and loaded only inside the disposable box.
