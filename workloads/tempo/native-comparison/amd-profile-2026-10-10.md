# AMD Tempo transfer profile (2026-10-10)

All kernel-module builds, loads, smoke tests, and profiles in this note ran in
disposable `boxctl` AMD EPYC 4245P boxes with Ubuntu HWE `7.0.0-38-generic`.
The local workstation was used only for source edits, Rust library tests, and
analysis of copied output. The guest is the same 100-transfer Tempo workload
described in `amd-boxctl-2026-10-09.md`, with one Bedrock CPU and five CPUs
reported to the guest.

`BEDROCK_FAIR_PROFILE_PREFIX=/home/ubuntu/profile` writes `profile-marker1.json`
at the transfer start marker; `--exit-stats-json profile-end.json` writes the
final snapshot at `--stop-at-vt`. The analyzer `analyze-amd-profile.py` subtracts
these cumulative counters to isolate the transfer phase. This avoids treating
slow guest boot as transfer execution.

## Baseline findings

- Before marker 1 in box-e7b88952, the guest caused 136,261,986 VM exits:
  77,975,079 monitor-trap exits and 54,752,808 nested-page faults.
  VM-entry preparation accounted for 65.1% of measured host cycles.
- From marker 1 (guest TSC 181,986,903,116) to virtual time 60.960 s in the
  same box, 840,634 additional exits consumed 45,627,343,764 host cycles.
  VM-entry preparation consumed 82.4%; guest execution and the VM runner
  consumed 11.7%; exit handlers consumed 2.0%. The 0.22 s guest interval
  is only an early transfer-phase sample, not a full benchmark result.
- An independent box-9df68301 run from marker 1 (guest TSC 181,935,626,450)
  to virtual time 61.000 s caused 1,377,620 exits and used 94.7% of its
  measured host cycles in VM-entry preparation. The different interval means
  these counts should not be compared as throughput results.
- A 30-second, 99 Hz host `perf record -g -p <bedrock-cli-pid>` sample after
  marker 1 in box-9df68301 put 75.46% of sampled cycles in
  `svm_batch::add_translation_child` and 15.48% in
  `collect_translation_tree_pages`; just 0.63% landed in `svm_run_guest`.
`add_translation_child` used a linear search through as many as 512 page
  table entries for every discovered child. It runs while the VM-entry path
  rebuilds a guarded translation tree. The percentages describe that sampled
  30-second window, not the entire transfer.
- A boot-phase `perf` sample was different: `collect_page_breakpoints` used
  26.61% of sampled cycles, `svm_run_guest` 15.49%, and `__pi_memcpy` 8.48%.
  The transfer-phase hotspot should therefore be validated against the
  transfer phase, rather than inferred from the boot profile.

The host `perf.data` recordings and the JSON snapshot pairs are kept in
ignored `target/boxctl-evidence/` directories. The kernel can report ROGPT
(`CPUID.8000000A:EDX[21]=1`) on these boxes; this bottleneck is not caused by
the hardware lacking ROGPT.

## Indexed child lookup experiment

An unmerged local candidate replaces the linear child search in
`add_translation_child` with a fixed-size, open-addressed page-to-index table
stored in the preallocated guard workspace. It preserves discovery order,
page-table level checks, the 512-table limit, and the existing fallback on
workspace exhaustion. All 240 `bedrock-vmx` library tests pass. The updated
module passes `svm_smoke` in both boxes, including exact deadline, fork CoW,
and replay checks.

Results across two same-box before/after checkpoint comparisons differ:

| Box and interval | Baseline run cycles | Indexed run cycles | Baseline VM exits | Indexed VM exits |
| --- | ---: | ---: | ---: | ---: |
| box-9df68301, marker 1 to vt 61.000 s | 193.82 billion | 48.82 billion | 1.378 million | 0.986 million |
| box-e7b88952, marker 1 to vt 60.960 s | 45.63 billion | 1,599.61 billion | 0.841 million | 22.810 million |

The favorable box improved about 4.0× in host cycles over a nearly equal
guest-TSC interval. The other box regressed about 35× and spent 94.9% of
host cycles in VM-entry preparation, with 22.095 million monitor-trap exits.
The guest states around these arbitrary virtual-time stops were different:
box-e7b88952's indexed run entered a long scalar-execution stretch after
transaction generation. A 20-second `perf` sample in that stretch placed
57.61% of cycles in `collect_translation_tree_pages`, 26.51% in `__pi_memcpy`,
and 6.61% in `add_translation_child`. The indexed lookup removed its targeted
linear-search hotspot but did not make the whole path consistently faster.
The experiment cannot yet be presented as a reliable application speedup.

## Follow-up diagnostics

A repeat of the box-e7 baseline checkpoint used 35.57 billion host cycles
and 816,152 exits from marker 1 to vt 60.960 s. Repeating the indexed run
with a sampled RIP logger used 39.59 billion cycles and 991,886 exits over
that checkpoint. The earlier 35-fold regression therefore did not reproduce;
an arbitrary virtual-time stop can land in a very different guest instruction
sequence. A later host `perf` sample with the index still placed roughly 60%
of cycles in `collect_translation_tree_pages` and 25% in `__pi_memcpy`.

The global guarded page-table tree is frequently unusable while a pending
scalar instruction prevents its refresh. Of 62 spaced tree-scan samples in
one run to vt 61.100 s, 45 had a dirty global guard and 26 had a different
guarded CR3. The maximum guarded tree in that run was 498 of 512 pages, with
no capacity failure. The code then recomputes the translation tree while
preparing individual exits.

A local experiment skipped bounded-batch planning whenever that global guard
was unusable. Against the repeated box-e7 baseline checkpoint, it raised
marker-to-vt-60.960 cycles from 35.57 to 63.13 billion and monitor-trap
exits to 9.907 million. Boot to marker also increased from 651.71 to 720.14
seconds. That approach was discarded because it traded expensive scans for
more single-step exits.

The deeper box-9df run to vt 61.300 s exposed another limit: a send-phase
process's translation tree reached the 512-page workspace capacity. The
diagnostic logger observed more than 140,000 capacity failures. Over the
marker-to-vt-61.300 interval, the indexed plus scalar-guard variant executed
73.728 million monitor-trap exits and used 311.82 billion host cycles. It
reached that virtual-time stop 86.04 host seconds after marker 1; it did not
complete the `bench send` command. A larger workspace is being tested
separately. The capacity failure is a concrete optimization target, but this
combined run cannot isolate its cost from the scalar-guard regression.

In the next two runs to the same vt 61.300 stop, the 512-page build took
650.05 seconds total and the 1,024-page build took 678.65 seconds total.
Both had sent 100 transactions and reached txpool-drain, but neither had
completed the benchmark. The 512-page run had **no** capacity failures, so
the failures are guest-state dependent and the larger workspace cannot yet
be called a speedup. These total times include boot; the timing environment
flag was accidentally omitted, so they do not isolate the application
interval. The following rerun uses marker snapshots.

The timed 1,024-page rerun reached the same vt 61.300 stop **36.54 host
seconds** after marker 1. That interval contained 2.930 million VM exits,
including 2.425 million monitor-trap exits, and 139.21 billion host cycles.
VM-entry preparation used 84.6% of those cycles. A 12-second host `perf`
sample after marker 1 placed 42.06% of sampled cycles in
`collect_page_breakpoints`, 11.02% in `__pi_memcpy`, and 6.84% in
`svm_run_guest`. The 100-transfer command had sent all 100 transactions but
was waiting for the txpool to drain at this stop.

A separate diagnostic variant rearmed the global table guard after the next
VM entry reported at least one retired instruction. In box-9df68301, its
marker-to-vt-61.300 interval used 14.10 host seconds, 1.155 million exits,
and 54.16 billion cycles; total tree scans through that run dropped to about
310,000, versus about 750,000 in the prior 512-page run in the same box.
This is a performance probe, **not a correctness-ready change**: an injected
interrupt could retire an instruction before the trapped store does. A stricter
candidate that checks the faulting store's next RIP passes library tests but
has not yet been run in a box. The guest states in these runs differ, so the
14.10-versus-36.54-second comparison is directional, not a controlled speedup
ratio.
