# AMD native sched clock PMC follow-up

This isolated branch combines the pushed PMC1 guest clock and SVM event-stack
batching with the earlier guest `native_sched_clock()` RDPMC patch. It also
tests retaining guarded translation/code proofs across opted-in scalar RDPMC;
that retention change is **not** in the production or main PMC-clock branches.

On disposable `box-0598ea76` (EPYC 4245P, HWE `7.0.0-38-generic`), the
clocksource-only patched guest and final PMC module passed the exact one-seed
Tempo campaign in **98 total / 63 seed host seconds**. With the same module,
driver, images, initrd, and seed, replacing only the guest kernel with the
`native_sched_clock()` variant (SHA-256
`de6267929714add0f4ab37ee7e87b14976530f3fb11aaa4ba07f4a13a0765baa`)
passed in **107 / 70 seconds**. Its sampled progress campaign also passed in
**104 / 67 seconds**. At a late progress marker near virtual second 241,
the sched-clock guest recorded **1.85 million RDTSC** and **4.97 million NPT**
exits among **13.38 million total exits**. The clocksource-only sampled run
on this box had about **5.12 million RDTSC** and **2.68 million NPT** exits at
a nearby late marker. Reducing time-read exits was largely offset by more
NPT exits, with no established wall-time improvement. The old isolated sched
variant had also stalled intermittently, so this patch should not be merged
on the strength of these passing runs.

The scalar guard-retention trial preserves proofs after `RDPMC` only when the
per-VM PMC clock is enabled and no software event delivery can write guest
memory. Its focused safety test, all **277 VMX unit tests**, and the full box
SVM suite passed. The exact campaign then passed in **113 total seconds**;
its progress campaign passed in **112 / 75 seconds** and recorded **5.55
million NPT faults** at its last virtual-time marker. Retaining the proofs
did not remove the added NPT work and did not improve elapsed time. Keep this
change isolated as a negative result. Logs are under ignored
`target/boxctl-evidence/box-0598ea76/sched-clock/`.

Only disposable boxes built or loaded kernel modules. The workstation used
Nix to select a guest Linux image and Rust to build the userspace driver.
