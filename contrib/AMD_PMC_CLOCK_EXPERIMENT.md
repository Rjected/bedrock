# AMD guest PMC clock experiment

This branch is an isolated performance probe, not a production-ready PMU
interface. It lets the guest execute `RDPMC` without interception, virtualizes
PMC1 as retired-instruction time, and adds the opt-in guest kernel parameter
`bedrock_pmc_clock`. Linux then reads PMC1 in its TSC clocksource and sends
userspace vDSO clock calls through the kernel. PMC0 still controls Bedrock's
exact instruction deadlines.

On disposable `box-a42f86b1` with the HWE `7.0.0-38-generic` host, the pinned
one-seed Tempo campaign passed in 191 host seconds with the option and 206
seconds without it. Both runs used the same patched guest kernel
(`vmlinux` SHA-256 `86bdd21555fb8323b52d74ffa736c0e744107be3f9c55b9c44a27cf68c5cb891`),
module, initrd, seed, and branch-built userspace driver. The 23 SVM and 275
VMX library tests and in-box SVM suite also passed. The guest `RDPMC` probe
matched across two forks resumed from a virtual-time checkpoint.

The `bedrock_pmc_clock` option is required for the guest to use PMC1. An
attempt to use PMC1 in `native_sched_clock()` as well reduced RDTSC exits but
stopped the full campaign at virtual second 113; that change is deliberately
absent here. The current probe still exposes other PMCs to guest ring 0 and
changes guest `RDPMC` behavior even when the kernel option is off. It needs a
counter-access policy, broader replay and interrupt tests, and safe handling
when PMC virtualization is unavailable before it can be merged.

Only build and load the kernel module in a disposable `boxctl` box with
`--disable-job-agent`. Do not build or load it on the workstation. The box
runner accepts `BEDROCK_PMC_CLOCK=1` to add the guest boot option. Stage the
`guestKernel` `vmlinux` output from this branch in the box's
`/home/ubuntu/ci-dst/vmlinux` with checksum verification; Nix outputs can
have identical size and mtime, causing ordinary `rsync` to skip a new image.
Preserve the original box `vmlinux` first and restore it after the trial.
