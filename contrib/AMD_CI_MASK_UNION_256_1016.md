# AMD CI bounded cross-page breakpoint-mask trial (2026-10-16)

This isolated variant retains the guarded cross-page singleton-proof union
from `dan/amd-ci-hot-mask-union-1016`, but limits any *new* cross-page merge
to a 256-byte AMD breakpoint mask. A page's existing singleton proof can
still contain its own wider mask. The change is hypervisor-only and preserves
the four hardware-breakpoint budget and exact-byte/alias proofs.

The broad union passed the pinned Tempo CI campaign but regressed from
190-192 to 236 host seconds on the same disposable EPYC 4245P / HWE
`7.0.0-38-generic` box. A one-in-256 sample of harmless masked #DB hits in
matched 60-virtual-second runs found 47,145 samples for that broad union
versus 1,281 for production. In the broad union, 33,806 samples hit guest
physical page `0xc662000`, mostly a hot loop at offsets `0x400`-`0x438`
covered by a new 1 KiB mask based at guest RIP `0x40f400`. Production had
351 samples on that same physical page, using a 256-byte mask at `0x40f600`.
The diagnostic code is pushed on `dan/amd-ci-mask-hit-profile-1016` and
`dan/amd-ci-production-mask-hit-profile-1016`.

The 256-byte variant passed 279 `bedrock-vmx` library tests and the complete
in-box SVM suite. In a shorter 60-virtual-second diagnostic, it passed in
117 host seconds with a 48.2-second warm checkpoint; adjacent production
passed in 114 host seconds and the broad union had taken 145 seconds with a
72.7-second warm checkpoint. The shortened diagnostic changes the workload
verdict and cannot be compared to Intel CI timing.

The exact pinned one-seed, 180-virtual-second Tempo CI campaign passed with
`failed=0` in 199 host seconds. The adjacent unchanged production control
also passed in 186 host seconds, using the same guest assets, driver, seed,
and box. Earlier production controls on this box took 190-192 seconds. This
variant has no established speed gain and is not merged into
`dan/amd-tempo-dst`. Production was restored and passed the full in-box SVM
suite before the adjacent control. No kernel module was built or loaded on
the workstation; all module work occurred inside the disposable box.
