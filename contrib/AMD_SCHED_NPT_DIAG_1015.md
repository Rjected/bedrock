# AMD sched-clock NPT fault diagnostic (2026-10-15)

This diagnostic branch samples one in 256 raw SVM exit RIPs and dumps
fetch/data NPT histograms at virtual TSC 600 billion. It also counts
clock exits with a proven one-page or multi-page global code plan. Its
atomics perturb the hot path; timings below establish workload completion,
not a performance comparison.

On disposable `box-01a145bd` (EPYC 4245P, HWE `7.0.0-38-generic`), the
same module, driver, initrd, images, and pinned seed ran the opt-in PMC1
clocksource guest (`vmlinux` SHA-256
`86bdd21555fb8323b52d74ffa736c0e744107be3f9c55b9c44a27cf68c5cb891`)
and the guest additionally using PMC1 in `native_sched_clock()` (`vmlinux`
SHA-256 `de6267929714add0f4ab37ee7e87b14976530f3fb11aaa4ba07f4a13a0765baa`).
The clocksource-only campaign passed in **101 seconds**. At the dump it
counted **1,634,165 fetch NPT faults** and **1,202,062 other NPT faults**.
The sched-clock guest passed in **106 seconds** and, after reloading the
module to reset its one-shot histogram, again in **104 seconds**. The second
run counted **2,659,044 fetch faults** and **1,007,676 other faults** at
the same virtual TSC threshold: the extra NPT work was predominantly
instruction fetch, about **1.02 million more** in this pair. A third
diagnostic sched-clock run with clock-plan counters passed in **112
seconds** and counted **2,966,616 fetch** and **1,319,685 other** faults
at its dump. The path varies across runs, but this independently confirms
fetch faults as the larger class. The data-fault RIPs still cluster at
APIC MMIO and Linux memory-management sites.

At the third sched-clock dump, **1,176,761** clock exits carried a proven
single-page global code plan and **247,506** carried a multi-page plan.
Merely retaining additional execute grants across those multi-page clock
exits can affect at most that smaller group directly; it cannot explain the
roughly million extra fetch faults in the matched comparison. A broader
execute-permission cache would need to keep every executable page covered
by its alias breakpoints and code write guards. This diagnostic does not
establish a safe or worthwhile implementation of that cache.

The 26 SVM and 276 VMX unit tests and full box SVM hardware suite passed.
The first sched-clock run produced no histogram because the diagnostic's
static one-shot dump flag persisted from the prior guest; the module was
reloaded before its repeat. Raw statuses, logs, and histograms are ignored
under `target/boxctl-evidence/box-01a145bd/sched-npt-diag/`. No kernel
module was built or loaded on the workstation.
