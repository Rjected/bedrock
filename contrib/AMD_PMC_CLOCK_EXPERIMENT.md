# AMD guest PMC clock experiment

This branch is an isolated performance probe, not a production-ready PMU
interface. The `bedrock_pmc_clock` boot parameter asks the Tempo runner to
enable a per-VM AMD PMC clock ioctl, then the patched Linux kernel reads PMC1
as retired-instruction time in its TSC clocksource. Userspace vDSO clock
calls enter the kernel. PMC0 still controls Bedrock's exact instruction
deadlines. VMs without the ioctl retain the ordinary RDPMC intercept and
PMC0-only counted entries; forked VMs inherit the opt-in.

With the per-VM opt-in, the exact pinned one-seed Tempo campaign passed in
186 host seconds on `box-a42f86b1` (`optin1015`, 2026-10-15), using the
patched guest kernel and progress driver. The earlier same-kernel experiment
without the per-VM guard passed in 191 seconds, and its matched clock-off
control passed in 206 seconds. These are single runs; the five-second
difference between the clock-on runs is within plausible run variation.
After moving intercept setup from every entry to the first entry of each VM,
the same pinned campaign passed in 195 host seconds (`optincached1015`).
The two passing opt-in timings therefore do not show a reliable speedup
from that small entry-path change. The opt-in box suite, guest PMC1 fork
replay, exact PMC0 deadline during a
guest RDPMC loop, initial CR4.PCE rejection, and a guest CR4.PCE write
rejection passed. Only the disposable box built or loaded the module.

On disposable `box-a42f86b1` with the HWE `7.0.0-38-generic` host, the pinned
one-seed Tempo campaign passed in 191 host seconds with the option and 206
seconds without it. Both runs used the same patched guest kernel
(`vmlinux` SHA-256 `86bdd21555fb8323b52d74ffa736c0e744107be3f9c55b9c44a27cf68c5cb891`),
module, initrd, seed, and branch-built userspace driver. The 23 SVM and 275
VMX library tests and in-box SVM suite also passed. The guest `RDPMC` probe
matched across two forks resumed from a virtual-time checkpoint.

The ioctl fails if SVM PMC virtualization is unavailable. The opt-in VM
intercepts CR4 writes and refuses entry if CR4.PCE is set, so guest CPL3
cannot read PMC0. Guest CPL0 can still address other counters with RDPMC;
the opt-in assumes the patched guest kernel, which reads PMC1 only. The
`bedrock_pmc_clock` option is required for the guest to use PMC1. An
attempt to use PMC1 in `native_sched_clock()` as well reduced RDTSC exits but
stopped the full campaign at virtual second 113; that change is deliberately
absent here. The current probe still exposes other PMCs to the opted-in guest
kernel and needs broader replay and interrupt tests before it can be merged.

Only build and load the kernel module in a disposable `boxctl` box with
`--disable-job-agent`. Do not build or load it on the workstation. The box
runner accepts `BEDROCK_PMC_CLOCK=1` to add the guest boot option. Stage the
`guestKernel` `vmlinux` output from this branch in the box's
`/home/ubuntu/ci-dst/vmlinux` with checksum verification; Nix outputs can
have identical size and mtime, causing ordinary `rsync` to skip a new image.
Preserve the original box `vmlinux` first and restore it after the trial.
