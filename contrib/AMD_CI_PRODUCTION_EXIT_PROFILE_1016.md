# AMD pinned CI production exit profile (2026-10-16)

The unchanged production branch `dan/amd-tempo-dst` ran the pinned one-seed,
180-virtual-second Tempo CI workload with `BEDROCK_SEED_EXIT_PROGRESS=1` on
disposable EPYC 4245P / HWE `7.0.0-38-generic` box `box-e675c0c1`. The
workload passed with `failed=0`; the campaign took 193 host seconds. The
progress driver has the same SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`
as the earlier pinned driver. Its ten-virtual-second snapshots add logging,
so this run is for attribution rather than an exact A/B timing comparison.

The final branch-VM snapshot at virtual time 233.734 s reported:

| Counter | Value | Share |
| --- | ---: | ---: |
| All VM exits | 47,053,389 | 100% |
| Nested-page faults | 26,019,998 | 55.3% of exits |
| RDTSC + RDTSCP | 14,002,336 | 29.8% of exits |
| Synthetic MTF exits | 5,514,453 | 11.7% of exits |
| VM-entry preparation | 208.21 billion TSC cycles | 42.0% of recorded run cycles |
| Guest/VM-run interval | 243.93 billion TSC cycles | 49.2% of recorded run cycles |
| Recorded run total | 495.49 billion TSC cycles | 100% |

The 47 million exits and 208-billion-cycle entry path make full-plan cache
micro-optimizations insufficient on their own: the page-scoped plan cache
hit only 1.82% of eligible lookups in the companion diagnostic. NPT faults
remain the largest exit class. RDTSC/RDTSCP exits are also substantial, but
they return an instruction-count-based virtual TSC; allowing raw hardware
TSC reads would change deterministic time semantics. Any TSC acceleration
must preserve that contract and replay, rather than merely avoid the exit.

The raw campaign log and status are under ignored
`target/boxctl-evidence/box-e675c0c1/ci-production-progress/`. No kernel
module was built or loaded on the workstation.

## Breakpoint-mask follow-up on the same CI workload

On disposable box `box-d267068a`, a general hypervisor-only cross-page
breakpoint-mask union passed the pinned campaign but took 236 host seconds,
versus adjacent unchanged production runs of 192 and 190 seconds. A
one-in-256 diagnostic found 47,145 harmless masked-breakpoint samples for
the union and 1,281 for unchanged production in matched shortened runs. The
union introduced a 1 KiB mask over a hot user loop, accounting for 33,806
samples on one page. A follow-up limited new cross-page merges to 256-byte
masks; it passed the pinned campaign in 199 seconds, versus an adjacent
production control of 186 seconds. Neither variant is merged. Their code,
tests, and detailed evidence are on `dan/amd-ci-hot-mask-union-1016` and
`dan/amd-ci-mask-union-256-1016`; diagnostic samplers are on
`dan/amd-ci-mask-hit-profile-1016` and
`dan/amd-ci-production-mask-hit-profile-1016`. These experiments show that
simply packing more hazardous pages into four debug registers can trade NPT
fetch faults for even more expensive false breakpoint retries.
