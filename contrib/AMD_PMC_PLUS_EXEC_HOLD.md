# PMC clock with retained AMD execute grants (2026-10-15)

This isolated branch combines the per-VM PMC1 clock opt-in with the production
single-page NPT execute-grant retention. It remains a trusted patched-guest
experiment: opted-in CPL0 code can select other PMCs, while CR4.PCE rejection
prevents guest applications at CPL3 from reading PMC0. The production branch
does not enable guest RDPMC.

On disposable `box-a42f86b1` (EPYC 4245P, HWE `7.0.0-38-generic`), all
275 VMX and 24 SVM library tests, the SVM hardware suite, and targeted guest
PMC1 fork, exact-deadline spin, initial CR4.PCE, and CR4.PCE-write probes
passed. The exact pinned Tempo one-seed campaign used patched `vmlinux`
SHA-256 `86bdd21555fb8323b52d74ffa736c0e744107be3f9c55b9c44a27cf68c5cb891`,
unchanged initrd and images, and branch-built driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
Alternating full runs with `bedrock_pmc_clock` on passed in **105 and 111
host seconds**; the same module and guest with the option off passed in
**116 and 115 seconds**. All had `failed=0`. The same-box median difference
is **7.5 seconds (~6.5%)**, smaller than the earlier 83-second gain from
retaining NPT execute, and could include run variation. The clock-on run
had only tens of RDTSCP exits by virtual second 140, while the clock-off
run had millions. Remaining wall time is therefore dominated elsewhere.

The reported Intel CI run is about 65 seconds on different hardware and a
different guest kernel; this experiment does not establish equivalence.
Only the disposable box built or loaded a kernel module. Restore the original
box guest image (`vmlinux-before-pmc-1015`) and production module after this
experiment.
