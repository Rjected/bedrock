# Sched-clock PMC after retained execute grants (2026-10-15)

This isolated trial combines the retained one-page NPT execute grant and
per-VM PMC1 clock opt-in with the guest patch that also reads PMC1 in
`native_sched_clock()`. The guest image SHA-256 is
`de6267929714add0f4ab37ee7e87b14976530f3fb11aaa4ba07f4a13a0765baa`.
Only the disposable `boxctl` box built or loaded the kernel module.

The box SVM suite, including `SVM_CLOCK_GUARD_WRITE_PASS`, passed. With the
same image, module, pinned Tempo seed, and userspace driver, the exact
campaign passed in **116 host seconds** with `BEDROCK_PMC_CLOCK=1` and
**109 seconds** with it off. These two runs show no speedup from extending
the PMC clock to `native_sched_clock()` after the execute-grant optimization.
Production runs with the original guest image and no PMC opt-in were already
in the 110–117-second range. A previous version of the sched-clock guest
patch also had one intermittent stall. This candidate remains isolated.
