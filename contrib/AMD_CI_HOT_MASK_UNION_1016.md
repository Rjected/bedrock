# AMD CI cross-page breakpoint-mask union trial (2026-10-16)

This isolated branch tests a hypervisor-only way to admit two code pages whose
raw unsafe-instruction offsets exceed the four AMD instruction-breakpoint
slots. The global planner preserves each page's guarded singleton proof and
merges same-page breakpoint ranges when the combined masks fit four slots.
Its merge preference avoids covering the current RIP. It also removes the
raw-hazard-count precheck that rejected pages before evaluating their actual
masked slot budget. No guest change is involved.

The local `bedrock-vmx` suite passed all 278 tests. The full in-box SVM suite
passed on disposable EPYC 4245P / HWE `7.0.0-38-generic` box
`box-d267068a`. No kernel module was built or loaded on the workstation.

The pinned one-seed, 180-virtual-second Tempo CI workload used the same box,
guest assets, driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`,
and seed `14373434789495652599` for both runs:

| Variant | Verdict | Host wall time |
| --- | --- | ---: |
| Unchanged production, before candidate | `failed=0` | 192 s |
| Cross-page mask-union candidate | `failed=0` | 236 s |
| Unchanged production, after candidate | `failed=0` | 190 s |

The candidate was 44-46 s (23-24%) slower than the adjacent production runs,
so it is not merged into `dan/amd-tempo-dst`. Warm checkpoint wall time was
47.8 s before, 71.1 s for the candidate, and 47.1 s after. The candidate's
added alias-proof evaluation and masking work is a plausible cause, but this
test does not isolate it. The production module was restored and passed the
in-box SVM suite before the final control run.

Raw campaign logs and status files are under ignored
`target/boxctl-evidence/box-d267068a/` in this worktree. The first control's
run name contains `guarded-indirect` from an abandoned prototype, but it used
the unchanged production module; no guarded-indirect code was loaded.
