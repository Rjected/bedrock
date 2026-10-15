# AMD PMC-clock host profiling (2026-10-15)

All runs used disposable `box-01a145bd` (EPYC 4245P, HWE
`7.0.0-38-generic`), the patched clocksource guest, pinned Tempo seed, and
the final debug-exit accounting fix. Only the box built or loaded kernel
modules. This branch adds a **diagnostic-only** assembly change: it pauses
host IBS across `VMRUN` and restores it on exit. It must not be merged into
the performance branch because the extra MSR accesses change entry cost.

The production module's clock-enabled campaign under `perf record -F 199 -g`
captured 7,928 host cycle samples, then stopped advancing after virtual
second 173.755 while the VM process remained CPU-bound. It was terminated
after 250 host seconds. A five-second tail profile attributed 61.43% of
samples to `svm_run_guest`, consistent with long guest residency, but it
does not identify the guest instruction or prove why it stopped. Without a
hardware PMU event, `perf record -e cpu-clock -F 199 -g` captured 7,931
samples and the full campaign passed in **98 seconds**. Its timer samples
collapsed onto `bedrock_local_irq_enable`, so they cannot rank host code.
The initial contrast raised a possible hardware-PMU profiling interaction;
one stalled run cannot establish a hardware or hypervisor root cause. The
opt-in remains off by default.

Follow-up controls on the same box make that caveat narrower. With the
patched guest and hardware-cycle profiler, a clock-**off** campaign passed
in **108 seconds** with 7,926 samples, and a clock-**on** repeat passed in
**93 seconds** with 7,940 samples. The sampled stall is intermittent; it
does not reproduce reliably under the clock-enabled setting.

The IBS-pause module passed the full box SVM suite and two clock-enabled
progress campaigns in **100 and 102 seconds**, both with zero workload
failures. A 40-second CPU1 `ibs_op//` sample at period 100 million yielded
331 samples with none lost but included unrelated CPU activity. A cleaner
35-second per-process sample at period 10 million yielded **9,264 samples**.
Its largest flat host symbols were the Rust run loop (**20.17%**),
`collect_page_breakpoints` (**9.69%**), `svm_run_guest` (**9.08%**),
`collect_translation_tree_pages` (**5.95%**), `__pi_memcpy` (**5.71%**),
forked parent-page reads (**5.19%**), and retained-guard lookup (**5.02%**).
These are diagnostic sample shares, not additive whole-campaign wall-time
fractions. The retained-guard lookup has already been removed in a separate
trial without an end-to-end speedup, so breakpoint/translation work and exit
volume remain the useful targets.

Raw `perf.data`, reports, status files, and logs are ignored under
`target/boxctl-evidence/box-01a145bd/pmc-ibs-pause/` here and
`target/boxctl-evidence/box-01a145bd/pmc-profile/` in the production worktree.
