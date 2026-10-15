# AMD guest PMC clock experiment

The per-VM clock is available only through an explicit ioctl and the patched
guest's `bedrock_pmc_clock` boot parameter. It is off for every new VM. On
2026-10-15, the final accounting fix and stack-batching baseline passed a
matched one-seed Tempo DST pair on a fresh EPYC 4245P box: **95 host seconds
with the clock on** and **106 seconds with it off**. Both runs used the same
patched guest (`vmlinux` SHA-256
`86bdd21555fb8323b52d74ffa736c0e744107be3f9c55b9c44a27cf68c5cb891`),
module, driver (`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`),
images, initrd, and pinned seed; both had `failed=0`. The box's full SVM
hardware suite, 26 SVM unit tests, and 276 VMX unit tests passed. PMC1 fork
replay, exact PMC0 deadline during a guest RDPMC spin, and CR4.PCE rejection
probes passed on the same module. A two-seed clock-on campaign passed both
seeds in **183 host seconds**, including a second seed with a different
preemption setting. This is evidence for the opt-in path, not a claim that all
guest workloads are covered. The AMD counter remains accessible to trusted
guest CPL0 code, so callers must pair the ioctl with a compatible guest
kernel.

With the clock disabled and the original guest image (`vmlinux` SHA-256
`3feabfd845b13320fc499b008747dac1c7f9991a87029879cb10ba0f940b2336`),
the same module and driver passed the pinned campaign in **103 host seconds**.
The matching pre-feature production runs on other boxes were 103–108 seconds;
this is a regression check, not a same-box code-version comparison. The
matched clock comparison above uses the patched guest in both arms.

Host `perf record -F 199 -g` using a hardware cycle counter coincided with
one clock-enabled progress run stopping at virtual second 173.755. It was
terminated after 250 host seconds. A repeat using software `cpu-clock`
sampling passed in **98 seconds**, and two runs under an isolated IBS-pause
diagnostic module passed in **100 and 102 seconds**. This points to possible
interaction with host hardware-PMU profiling; it does not establish the root
cause. The opt-in stays off by default. The diagnostic and 9,264-sample
per-process IBS profile are pushed on `dan/amd-pmc-ibs-profile-1015`
(`1897a2c`).

This is an experimental opt-in PMU interface. The `bedrock_pmc_clock` boot parameter asks the Tempo runner to
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
absent here. The opt-in still exposes other PMCs to the guest kernel; keep it
off by default pending broader guest and replay coverage.

Only build and load the kernel module in a disposable `boxctl` box with
`--disable-job-agent`. Do not build or load it on the workstation. The box
runner accepts `BEDROCK_PMC_CLOCK=1` to add the guest boot option. Stage the
`guestKernel` `vmlinux` output from this branch in the box's
`/home/ubuntu/ci-dst/vmlinux` with checksum verification; Nix outputs can
have identical size and mtime, causing ordinary `rsync` to skip a new image.
Preserve the original box `vmlinux` first and restore it after the trial.
