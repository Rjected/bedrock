# Coherent AMD PMC scheduler clock trial (2026-10-15)

This experimental guest patch extends the opt-in `bedrock_pmc_clock` path to
Linux's scheduler clock. It reads PMC1 for `native_sched_clock()` and for the
boot and cpufreq anchors used by `cyc2ns`. The earlier runtime-only trial left
the anchors in the TSC domain, so its clock conversion mixed two counters.
Without the boot option, all three sites continue to read the TSC.

The patched Linux 6.18 guest was built **inside** disposable
`box-4ed44560` with Clang 18. The comparison guest was built from the same
source, config, compiler, and box after reversing this patch. Both use the
production `80c8c96` Bedrock module, the same initrd, images, workload
planner, pinned seed `14373434789495652599`, and `BEDROCK_PMC_CLOCK=1`.
The two stripped kernel hashes are:

| Guest | SHA-256 |
| --- | --- |
| Control | `81f8ba9aac4b9111ae71f273003edb0282406990396c4898aa1f3a802a7a17f3` |
| Coherent scheduler clock | `ed2879efb1dbe9ff5084beae5188ff1e1f80834fe778593714fcb3bb9e34e50a` |

| Run | Control | Candidate |
| --- | ---: | ---: |
| 60-virtual-second progress DST, host wall | 119s, pass | 110s, pass |
| 180-virtual-second pinned DST, host wall | 198s, pass | 188s, pass |

The short candidate run had 12.68 million exits versus 14.74 million for its
control, with RDTSC exits falling from 1.64 million to 0.55 million. Its NPT
faults also fell from 10.30 million to 9.29 million. The full comparison is a
single paired pass. A second run of the **same** candidate and pinned seed
did not complete: after reaching guest virtual second 226.906 (the target was
roughly 229.2), the guest console stopped advancing and `bedrock-dst` held
one CPU in the VM-run ioctl. We interrupted it after 371 host seconds. The
first full pass therefore does **not** establish a reliable speedup or
resolve the earlier scheduler-clock stall.

An eight-second, 799-sample `perf` capture during the stall lost zero samples.
The largest exclusive samples were `collect_translation_tree_pages` (36.1%),
kernel `__pi_memcpy` (17.1%), forked/root `ParentVm::read_page` (25.3%
combined), and `add_translation_child` (10.8%). This identifies repeated
guest translation-tree collection as the hot path **during** the stall; it
does not yet prove why guest progress stopped. The GDB stack for the
userspace thread was `Vm::run` → `Branch::run_until` → `Driver::run_until`.

Keep the patch off the production branch pending a root-cause fix, repeated
full campaigns, alternate seeds, and review of PMC clock behavior across
supported guest workloads.
