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
candidate that checks the faulting store's next RIP was run subsequently;
its complete-command result is below. The guest states differed across runs,
so the 14.10-versus-36.54-second comparison is directional, not a
controlled speedup ratio.

## Complete 100-transfer command

The 1,024-page indexed build completed `bench send` for 100 transfers in
**127.304 host seconds** between the two fair timing markers. The report
shows 100 sent, 100 successful, 100 included in one block, zero failures,
and zero reverts. The same command took 3.216 seconds in the native AMD
control, so this Bedrock run was about 39.6 times slower. During the command,
Bedrock recorded 28.048 million exits, of which 26.395 million (94.1%)
were monitor-trap exits; VM-entry preparation used 71.2% of measured host
cycles. A later 25-second host `perf` sample placed 24.24% of cycles in
`svm_run_guest` and 17.97% in `collect_page_breakpoints`.

In a concurrent 512-page indexed plus diagnostic guard-refresh run, the same
100-transfer command sent all 100 but had not reached its end marker after
more than three host minutes. It stalled near vt 62.151 s after exceeding
7.59 million guarded-tree capacity failures for one CR3. A 20-second host
`perf` sample there put 62.96% of cycles in
`collect_translation_tree_pages`, 21.56% in `__pi_memcpy`, and 1.30% in
`svm_run_guest`. The earlier 512-page
run that did not hit capacity was not representative of this tail behavior.
The 1,024-page capacity avoids this observed failure in the completed run,
though it does not by itself make execution fast.

A stricter guard-refresh experiment waited for the trapped store's decoded
next RIP and forced one instruction before rearming. It completed the same
100-transfer command in **164.800 host seconds** with 30.944 million exits,
including 29.341 million monitor-trap exits. That is slower than the pushed
1,024-page build's 127.304 seconds and 28.048 million exits, so this guard
change was discarded.

## Exact 10,000-transfer workload

The pushed 1,024-page indexed build reached marker 1 and started the same
10,000-transfer `bench send` command used in the historical Intel comparison.
It ran for **more than seven host minutes after marker 1 without an end
marker**, then was stopped to bound the feedback loop. This is a lower bound,
not a completed timing; historical Intel Bedrock completed in 24.087 seconds.
A 20-second host `perf` sample after `bench send` began put 43.52% of cycles
in `collect_page_breakpoints`, 11.83% in `visit_alias_entry`, and 4.72% in
`svm_run_guest`. Repeated alias and page-breakpoint proof work is now the
dominant measured bottleneck for this exact workload.

## Batch-path sampling

A box-only diagnostic sampled one in every 256 VM entries and printed
cumulative batch-path counts at 4.194-million-entry intervals. Subtracting
the samples nearest the two 100-transfer timing markers yielded 131,071
sampled entries: **73.8% scalar**, 14.1% page batches, 6.0% bounded batches,
5.5% global batches, and 0.6% other paths. No sampled entry failed while
protecting an already prepared batch. This identifies prolonged scalar
execution as the main source of the monitor-trap volume in that run; the
precise mix can vary with guest state. The instrumented complete command
took 133.706 host seconds.

Another local candidate indexed lookups in the global guarded-table set to
avoid a linear search during alias traversal. All 241 library tests and the
box SVM smoke test passed, but its complete 100-transfer command took
**135.555 host seconds** with 30.394 million exits. The pushed build's
127.304 seconds and 28.048 million exits remain the better result. The
global-index candidate was not merged.

## Repeated-RIP sampling

A separate box-only diagnostic sampled guest RIP and CR3 every 262,139
monitor-trap exits during a complete 100-transfer run of the pushed build.
The two fair-timing markers were 139.261 host seconds apart, and `bench send`
reported 100 sent and 100 successful transfers. This instrumented timing is
consistent with, but does not improve on, the uninstrumented 127.304-second
result.

After the first timing marker, 69 of 123 distinct sampled monitor-trap exits
had the same guest RIP (`0x7fce5a47848a`) and CR3 (`0x10296b000`). Two
consecutive 20-sample stretches at that address each spanned 4,980,641
monitor-trap exits. The guest TSC advanced only 16.8–18.6 million ticks in
those stretches. This identifies one repeated instruction as a major source
of scalar exits. A second complete 100-transfer run with opcode sampling
identified the same address offset (`0x48a`) as **`REP STOSB` (`f3 aa`)**:
69 of 126 distinct sampled monitor-trap exits during the transfer phase
had that exact RIP and opcode. Its fair-marker interval was 123.717 host
seconds, and the command again reported 100 successful transfers. The
repeat-batch planner already recognizes this instruction; the problem is
that it often falls back to scalar execution.

A candidate limited each `REP STOSB`/`REP MOVS*` batch to the current
destination page. This avoids rejecting a safe prefix just because the
following destination page is unmapped. The focused mock test used an
unmapped next page, all 241 library tests passed, and the box SVM smoke
test passed. In the complete 100-transfer workload, however, the candidate
took **151.477 host seconds**, with 29.931 million exits including 28.295
million monitor-trap exits. It did not improve the pushed build's 127.304
seconds and 28.048 million exits, so it was not merged. A separate partial
diagnostic run observed `REP STOSB` with a mapped current destination and
unmapped next page, including cases whose remaining count fit within the
current page. The next test is identifying the planner condition that rejects
these batches.

The planner probe found the missing condition: at repeated `REP STOSB`
entries it reported `prepared=false`, despite `can_loop=true`,
`can_guard_page_tables=true`, and budgets above 1.5 million instructions.
Guest RFLAGS was `0x10213` or `0x10217`, so RF (bit 16) was set. The old
planner rejected all RF entries. AMD's [Architecture Programmer's Manual,
Volume 2](https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2) documents that
an interrupted string instruction can retain RF until it successfully
completes. The new planner permits RF only for recognized bounded REP MOVS
and STOS chunks; it keeps the conservative check for other instructions.
When an artificial chunk ends before the original count is exhausted, the
runner restores RF along with RCX and RIP before exposing a timer or
interrupt boundary.

The RF-aware, page-bounded candidate completed the 100-transfer command in
**117.036 host seconds**, with 100 sent and 100 successful transfers. It
made **11.612 million exits, including 9.962 million monitor-trap exits**,
versus 29.488 million exits and 27.840 million monitor-trap exits in the
opcode-logging control. No transfer-phase sample hit `REP STOSB` in the
candidate. The fair-marker wall time improved modestly from 123.717
seconds in that control and 127.304 seconds in the pushed uninstrumented
build. VM-entry preparation still consumed **82.1%** of the candidate's
measured host cycles, so removing this scalar hotspot alone does not solve
the application performance gap.

The same RF-aware build then started the exact 10,000-transfer command. It
reached fair marker 1 at guest virtual time 62.587 s, but had no end marker
or benchmark report **251 host seconds later**; it was stopped to bound the
experiment. Thus it still exceeds the historical Intel Bedrock 24.087-second
result by more than 10.4× without completing. A 20-second, 99 Hz host `perf`
sample during that transfer phase put 32.57% of sampled cycles in
`collect_page_breakpoints`, 12.29% in `visit_alias_entry`, and 7.02% in
`svm_run_guest`. Repeated alias proof work remains the largest measured
source of host work after the REP fix.

When the global page-table guard is dirty, the next candidate tries the
existing reachable-code proof before enumerating every executable alias of
a hazardous code page. It retains the full alias walk as fallback. All 242
library tests, the box SVM smoke test, and the hardware REP/RF deadline
regression passed. The complete 100-transfer command took **96.319 host
seconds**, with 100 successful and included transfers. It made **9.754
million exits**, including 8.176 million monitor-trap exits, and spent
292.55 billion host cycles in VM-entry preparation. That compares with
117.036 seconds, 11.612 million exits, and 367.06 billion preparation
cycles for the RF-aware build before this change. A 20-second transfer-phase
`perf` sample on the new build put 25.00% in `collect_page_breakpoints`,
3.33% in `visit_alias_entry`, and 9.58% in `svm_run_guest`. These are
samples from the 100-transfer run and should not be equated directly with
the separate 10,000-transfer profile above. VM-entry preparation remains
79.5% of this run's measured host cycles.

The region-first build also ran the exact 10,000-transfer command on the
same disposable AMD box. It reached fair marker 1 after 662.451 host
seconds of setup at guest virtual time 62.587 s. Four host minutes after
that marker, it had reached only guest virtual time 62.947 s and had no
second marker or completed transfer report, so the run was stopped. Its
marker-to-marker time is therefore **more than 240 seconds**, already more
than **10×** the historical Intel Bedrock 24.087-second result, without a
completed workload. A 20-second, 99 Hz host sample during this interval put
34.61% of cycles in `collect_page_breakpoints`, 10.04% in
`visit_alias_entry`, and 6.48% in `svm_run_guest`. Disassembly annotated
against the box's module identifies the collector's hottest loop as the
linear `gate_tables` lookup at `svm_batch.rs:1745`. The earlier global
guarded-table hash-index candidate regressed the 100-transfer workload
before the RF and region fixes; the current profile motivates revisiting
that lookup with a focused benchmark, but does not establish a faster
replacement yet.

We retested that guarded-table hash index on top of the RF-aware,
region-first build. All 243 library tests passed, along with box-only SVM
smoke and REP/RF tests. The exact 100-transfer workload finished with 100
successful and included transfers, but whole-guest wall time increased from
**755.092 to 775.458 seconds**. VM-entry preparation rose from 1.956 to
2.031 trillion cycles; monitor-trap exits remained near 62 million. This
run accidentally omitted the `BEDROCK_FAIR_TIMING=1` environment flag
required by the box's CLI binary, so it has no fair transfer markers and
cannot establish the index's transfer-only effect. The full-run regression
and earlier 100-transfer regression do not justify merging the index.

A box-only diagnostic counted alias decisions during a bounded 10,000-transfer
run. In a 20-second interval after fair marker 1, the collector was called
**3,045,595** times: **2,881,353 full alias walks** and **164,242 cached
proof hits**. Its translation-tree proof was valid on 3,045,228 of those
calls; the global gate was ready throughout and its CR3 matched on 2,960,637
calls. Dirty-gate calls were 326,203. This rules out an absent translation
guard as the general explanation for the 95% alias-proof miss rate. Frequent
proof churn, invalidation, and changing hazardous-page sets remain possible;
the counters do not distinguish those causes. A 20-second transfer-phase
`perf` sample from the first diagnostic run put 30.81% in
`collect_page_breakpoints` and 10.81% in `visit_alias_entry`. Both diagnostic
runs were stopped after marker 1 rather than allowed to run indefinitely.
