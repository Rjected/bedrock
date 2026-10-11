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

A focused experiment tried the reachable-code proof for every hazardous page
while the global gate was ready, before adding speculative code pages. All
242 library tests and box-only SVM smoke and REP/RF tests passed, but the
10,000-transfer guest failed about 30 host seconds after fair marker 1.
The kernel reported `SVM PMU exceeded budget: count=14 budget=753403
code=0x41`, then returned a VM-entry error; no second marker or transfer
report exists. Here `count` exceeded the verified batch count, even though
it was below the deadline budget. A partial transfer-phase `perf` sample
still put 34% in breakpoint collection and 9.86% in alias visitation. This
broader region path is **rejected** and was not merged. The known-good module
was restored and passed the box smoke and REP/RF tests again.

A third bounded diagnostic separated alias-cache misses by proof state. In
20 seconds after marker 1, there were **2,210,196** collector calls,
**2,121,528** full walks, and **88,668** cache hits. Only **11,196** of
those misses found *any* valid cached alias proof; all of those had the
same CR3, and only 5,495 covered the primary hazardous page. The tree
proof was valid on 2,210,019 calls. During the same window, the tree path
cleared alias proofs **317,522** times and the code-hazard path cleared them
**292,586** times. This identifies frequent whole-cache invalidation as the
dominant explanation for the miss rate. Enlarging or indexing the alias
cache alone cannot address it. A safe optimization must distinguish writes
that can create a new executable alias of a hazardous page from unrelated
page-table activity before retaining its proof.

A conservative leaf-only prototype kept an alias proof through a released
leaf table only when the resulting executable PTEs did not name any of its
hazardous physical pages. It withheld reuse while the gate was dirty and
retained the existing full invalidation for upper-table changes, host writes,
and failed rearm. All 243 library tests, box SVM smoke, and REP/RF regression
passed. The exact 10,000-transfer run still had no end marker or report more
than two host minutes after marker 1, with guest virtual time only reaching
62.846 s. A 20-second transfer profile put 32.51% in breakpoint collection
and 8.98% in alias visitation, near the baseline. This prototype was not
merged; its limited effect suggests the leaf-only path may be too rare or
another invalidation path may clear proofs before reuse.

A follow-up box-only counter run found why: in a 20-second 10,000-transfer
window, the leaf-only rearm path succeeded just **50** times and the full
global tree rebuilt **200** times. By contrast, the refresh loop returned
early for the selected scalar code page **4,433,410** times. During this
window the collector still made 1,265,473 full alias walks and hit its
proof cache only 3,674 times. The translation-tree and code-hazard paths
cleared proofs 130,344 and 105,989 times, respectively, often repeatedly
while the global gate remained dirty. A useful fix must distinguish the
single trapped page-table store's replay from prolonged scalar execution on
an untrusted code page; the latter currently blocks global refresh even
after the store may have retired. Rearming early requires a precise store
completion proof so an injected interrupt cannot make stale aliases usable.

An experimental combined patch made that distinction for decoded scalar
stores and also retained unaffected alias proofs across leaf-only table
rearms. All 244 library tests passed, and the box SVM smoke and REP/RF
tests passed. In the exact 10,000-transfer guest, fair marker 1 arrived
after 562.730 host seconds of setup at guest virtual time 62.595 s. After
roughly two further host minutes, there was no marker 2 or completed report;
the latest guest log had advanced only to virtual time 62.864 s. We stopped
the run. A 20-second transfer-phase host profile put **51.32%** of cycles
in `collect_page_breakpoints`, **13.76%** in `visit_alias_entry`, and only
**4.83%** in `svm_run_guest`. Thus the combination did not solve the hot
alias walk and is not merged. It also confirms that faster guest setup
does not imply faster transfer execution. The experiment's log, first
marker, and profile are retained under ignored `target/boxctl-evidence`.

A combined-patch diagnostic counted the alias cache during the exact 10,000-
transfer guest. In the first 20 seconds after fair marker 1, the collector
received **1,601,159 calls** and found **254,094 cached proofs**. Only
**52,797** calls that reached the proof lookup found any valid proof but no
matching one. The global tree refreshed fully **614** times, and those
refreshes invalidated alias proofs **614** times. Leaf-only rearm succeeded
**2,196** times. The collector observed **zero** dirty-gate calls, and the
code-hazard path invalidated proofs **zero** times in this interval. These
measurements isolate complete guarded-tree refresh as the main source of
proof invalidation during transfers. The collector-call count includes
hazard-free early returns, so calls minus cache hits is not an exact count
of full alias walks; the next diagnostic instruments walks separately.

A later 20-second transfer-phase run counted the actual full walks:
**876,936** out of **1,530,287** collector calls, with **321,719** cache
hits. The guarded tree rebuilt fully **688** times. Selective retention
rejected all of them: **637** failed the existing global-guard prerequisite,
**44** had a newly executable upper-table link, and **7** had no valid proof
to retain. The selective full-rebuild candidate still put **44.92%** of host
cycles in `collect_page_breakpoints`, **11.04%** in `visit_alias_entry`, and
**4.40%** in `svm_run_guest`; it did not reach marker 2 before the bounded
run was stopped. This candidate is not merged. A follow-up diagnostic split
the guard-prerequisite rejection into RUN-boundary distrust, changed CR3,
and unavailable gate state so the next optimization can address the actual
condition.

An experimental root-VM RUN mode made the CLI's shared guest-RAM mapping
read-only after loading Linux and skipped the blanket host-write distrust
between RUN calls. A box smoke test exercised this mode and restoring a
writable mapping; the REP/RF regression also passed. The exact 10,000-
transfer guest reached marker 1 after **465.979 host seconds** of setup,
faster than the previous diagnostic run's 553.159 seconds. Transfer execution
remained slow: in 20 seconds, the collector made **1,320,157 full alias
walks** and found **221,419 cache hits**. The tree rebuilt fully **444**
times; **397** rejected alias-proof retention because the global gate was
not ready, **34** because an upper table gained an executable link, and
**13** because no valid proof remained. There were **zero** untrusted-link
rejections and **zero** dirty-gate collector calls. A host profile put
**54.85%** of cycles in `collect_page_breakpoints`, **15.56%** in
`visit_alias_entry`, and **4.81%** in `svm_run_guest`. No second marker or
completed report appeared during roughly two host minutes after marker 1,
so this experimental mode has not met the workload target and is not merged.
The next diagnostic counts why the global guarded tree is unavailable.

That diagnostic found no tree failure in the first 20 seconds of the exact
10,000-transfer phase: **zero** workspace-capacity failures, tree-scan
failures, or NPT write-guard installation errors, with a maximum of **518**
reachable table pages against the 1,024-page capacity. All **499** full
tree refreshes succeeded. A more precise split showed **469** refreshes
were clean CR3 switches and **50** followed dirty page-table state; none
started from an uninitialized gate. The alias collector still made about
**1.316 million full walks** and found **265,392** cached proofs in that
window. Thus increasing the table capacity cannot address this particular
10,000-transfer stall. Each clean address-space switch currently replaces
the single-root guarded tree and clears alias proofs, so safe reuse across
CR3 changes is the next performance problem. Any retained proof must remain
protected against writes to its old root's page tables while another root
is active.

## Completed 10,000-transfer AMD comparison

The guarded-root and alias-proof changes above, combined with a 2,048-entry
alias-walk workspace, completed the exact 10,000-transfer Tempo guest on the
AMD EPYC 4245P box. The first `FAIR_TIMING` marker was at 335.115092622 host
seconds and the second at 549.267251625, giving **214.152159003 seconds** for
the matched transfer phase. The guest emitted `TEMPO_TXGEN_PASS: all
transactions admitted and included`. The historical Intel run took **24.087
seconds** for the same workload, so AMD is currently **8.9 times slower** on
this comparison. The box log is preserved in ignored
`target/boxctl-evidence/box-9ecc6ad0/alias2048/alias2048-10k.log` in the
experimental worktree.

The transfer phase produced 64,535,413 VM exits, including 49,382,764
nested-page faults (76.5%) and 11,710,235 monitor-trap exits (18.1%). In a
20-second transfer sample, the alias collector received 4,563,687 calls,
served 4,464,769 from its proof cache, and performed only 12,755 full walks,
all successful. Collector work fell to 4.23% of sampled host cycles. The
2,048-entry walker matters: with a 512-entry walker, over a million full
walks per 20 seconds were repeatedly failing before they could be cached.

Nested-page execute faults now dominate. A bounded transfer-phase sample
counted about 3.92 million execute faults and 9,000 write faults in 20
seconds. About 2.81 million execute faults were on a page other than the
currently selected code page. Global-safe scanning rejected about 3.03
million candidates for code-byte hazards. The next speed improvement needs
to reduce these code-page transition faults while preserving deterministic
delivery at unsafe instruction boundaries. The verified changes here meet
the workload-completion target, but do not yet meet Intel's timing.

## Paired hazardous-page global batches

A follow-up same-box comparison on `amd-hazardguard-1010` tested two changes
against the pushed baseline. First, write-guarding recurrent pages whose
hazard scan was rejected completed the workload but **regressed** from
212.690508815 to 231.712092349 seconds between fair markers. Its transfer
phase had 51,467,969 nested-page faults, versus 48,754,449 for the baseline.
That rejected-page change was discarded.

The retained change lets a counted global batch execute on the current
hazardous code page and one recently visited hazardous page when their
combined executable-alias breakpoints fit the four hardware slots. Both
pages are write-guarded for the batch, and the second page's temporary NPT
execute permission is restored immediately after VM exit. Other pages still
fault on fetch. A box SVM smoke test, REP/RF transition suite, 251 VMX library
tests, and the full guest workload passed.

| Same AMD box, 10,000 transfers | Fair marker interval | VM exits | Nested-page faults |
| --- | ---: | ---: | ---: |
| Pushed baseline | 212.690508815 s | 64,372,078 | 48,754,449 |
| Rejected-page guard (discarded) | 231.712092349 s | 66,989,977 | 51,467,969 |
| Paired-page global batch | **194.128301083 s** | 48,120,595 | 33,455,083 |
| Three recent pages (discarded) | 199.249994797 s | 44,299,836 | 29,582,422 |
| Third page only with a guarded memo (discarded) | 205.272250077 s | 47,901,162 | 33,010,146 |

The paired-page change improved elapsed transfer time by **8.7%** and cut
nested-page faults by **31.4%** against the same-box baseline. The guest again
reported `TEMPO_TXGEN_PASS`. At **8.1 times** the historical Intel Bedrock
24.087-second interval, it is progress toward the application target rather
than parity. Paired and baseline logs plus marker/end JSON snapshots are
retained under ignored `target/boxctl-evidence/amd-hazardguard-1010/`.

The two three-page variants also completed all 10,000 transfers and passed
the SVM smoke and transition suite, but neither beat the paired-page build.
The unrestricted third page cut another 3.87 million nested-page faults;
VM-entry preparation rose from 415.35 to 455.02 billion cycles, more than
offsetting that saving. Requiring a write-guarded memo for the third page
reduced its fault benefit without recovering the entry cost (452.32 billion
preparation cycles). Both variants remain experimental and were not merged.
The next optimization needs to lower entry work per attempted code-page set,
or use a different mechanism to prevent execute faults.

## Reuse code-page reads during hazard scans

The next same-box experiment used `amd-paired-profile-1010` and the same
10,000-transfer guest, timing boundaries, HWE kernel, and EPYC 4245P CPU.
Previously, a hazard scan read a 4 KB guest code page in 512-byte chunks, then
read the entire page again to populate its exact-byte memo. The retained
change copies each chunk into the memo during the scan. A page rejected for
having more than four hazard entries still finishes copying the page, so a
later exact-byte comparison can recognize that rejection. Guest-memory read
failures do not create a valid memo.

| Same box, run order | Fair marker interval | VM exits | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| Paired-page baseline | 203.980055998 s | 49,299,371 | 34,270,456 | 443.08 billion |
| Scan/memo single read | 196.617210180 s | 48,353,466 | 33,624,186 | 424.58 billion |
| Single read plus 16 retained write guards (discarded) | 200.261186882 s | 48,521,369 | 33,754,210 | 434.60 billion |
| Paired-page baseline repeat | 199.208063192 s | 48,372,680 | 33,370,122 | 431.09 billion |
| Scan/memo single-read repeat | 193.645588692 s | 47,884,360 | 33,239,015 | 418.82 billion |

The two baseline intervals average **201.594 seconds**; the two single-read
intervals average **195.131 seconds**, a **3.2%** same-box improvement. All five
runs emitted `TEMPO_TXGEN_PASS`. The candidate also passed the box SVM smoke
and REP/RF transition tests and all 251 VMX and 40 VM library tests. A
30-second transfer-phase `perf` sample placed `page_hazards` at 11.07% of
sampled cycles in the baseline and 9.41% in the single-read candidate. The
16-guard variant did not improve the complete workload and remains unmerged.
Logs, marker/end snapshots, and perf reports are retained in ignored
`target/boxctl-evidence/amd-paired-profile-1010/`.

This change removes repeated host work but does not change the underlying
exit pattern: the retained build still averages about **8.1×** the historical
Intel Bedrock 24.087-second interval, with roughly 33 million nested-page
faults per transfer phase. Substantially closer timing requires reducing the
execute-fault and entry-preparation work further.

## Promote safe zero-hazard code pages

An instrumented 10,000-transfer run on `amd-hazard-diag-1010` classified
15,605,578 rejected global batches. Of those, 8,868,972 (56.8%) were on
pages whose exact-byte scan found no hazardous instruction; only 19,661 of
5,670,181 page scans overflowed the four-breakpoint hazard budget. This
pointed to pages left on the scalar path after the translation gate became
ready, rather than hazard overflow, as a likely source of avoidable work.

When a zero-hazard page is encountered with the gate ready, the retained
change promotes it to NPT write-guarded executable code and starts a global
counter batch. It requires a proven non-store instruction at the current RIP.
Without that condition, a store replaying after a write fault can rearm the
guard before it retires and loop forever; an initial candidate exhibited that
failure during Linux boot. A unit test now covers promotion, invalidation,
and the store-replay guard. The corrected build passed 252 VMX and 40 VM
library tests, the box SVM smoke and REP/RF transition suite, and both full
Tempo runs below.

| Same AMD box, 10,000 transfers | Fair marker interval | VM exits | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| Safe-page promotion | 201.250429609 s | 47,819,010 | 33,967,858 | 413.50 billion |
| Existing branch, after promotion run | 207.501964282 s | 47,747,211 | 33,181,612 | 435.56 billion |

Promotion improved this paired run by **6.25 seconds (3.0%)** and saved
22.06 billion measured VM-entry preparation cycles, despite more nested-page
faults. This is one same-box pair, so the size of the gain is less certain
than the earlier repeated single-read comparison. The promoted run remains
**8.4×** the historical Intel 24.087-second interval, and about 34 million
nested-page faults remain. Guest logs and marker snapshots are retained under
ignored `target/boxctl-evidence/amd-promotion-1010/`; all kernel work ran on
the disposable box with the 7.0 HWE kernel.

## Retain a larger exact-byte hazard working set

On `amd-next-profile-1010`, a transfer-phase `perf` sample of the promoted
build still spent 9.28% of host cycles scanning 4 KB code pages in
`page_hazards` and 6.95% checking the hazard memo. A box-only diagnostic
reset its execute-fault map at fair marker 1, then tracked 32,586,065
execute faults over the transfer phase with only 15 dropped hash entries.
The 32 hottest guest-physical pages caused 22,781,745 faults (69.9%); all
were classified as hazardous with the global translation gate ready. Safe
page promotion alone cannot eliminate those page transitions.

The retained change expands the exact-byte hazard memo from 64 to 256 pages.
A 1,024-entry hint table points to likely memo slots, but every hint is
checked against the physical page and validity before use. Collisions and
evictions fall back to the complete lookup; they cannot reuse another page's
hazard proof. A unit test covers a 96-page working set and a stale hint.
The VMX library's 253 tests, VM library's 40 tests, box SVM smoke, and REP/RF
transition suite passed. All four full guest runs below emitted
`TEMPO_TXGEN_PASS`.

| Same AMD box, run order | Fair transfer interval | VM exits | Nested-page faults |
| --- | ---: | ---: | ---: |
| Existing branch | 186.262105229 s | 45,673,029 | 31,958,002 |
| 256-page memo | 176.206401302 s | 46,557,454 | 32,541,501 |
| Existing branch repeat | 184.731231701 s | — | — |
| 256-page memo repeat | 182.816336905 s | — | — |

The two existing-branch intervals averaged **185.497 seconds**, and the two
expanded-memo intervals averaged **179.511 seconds**, a **3.2%** improvement.
In matched 30-second transfer samples, `page_hazards` fell from 9.28% to
1.21% of sampled host cycles. Measured exit-handler cycles fell from 48.85
to 26.05 billion in the first pair, despite 0.58 million more nested-page
faults. Guest logs, marker snapshots, and perf reports are saved under
ignored `target/boxctl-evidence/amd-next-profile-1010/`. The remaining
~32 million execute faults leave AMD about **7.5×** the historical Intel
24.087-second workload; reducing hazardous-page transitions is the main
remaining performance problem.

### Follow-up: hot-page boundaries and rejected NPT experiments

On the same HWE 7.0.0-38 AMD box, an exact transfer-phase execute-fault
diagnostic recorded 31,782,276 faults across 2,129 physical pages, with no
dropped samples. The hottest 32 pages accounted for 22,212,343 faults. Eight
of those 32 pages had no interior instruction hazards yet failed the global
safe-page rule, accounting for 4,257,788 faults. A second run logged their
page-edge bytes: all seven zero-interior-hazard pages in that run ended in
`F2`, `F3`, or `0F`. Those bytes may begin an instruction that crosses into
another virtual page, so zero interior hazards alone cannot justify globally
enabling execution. The second diagnostic run passed all 10,000 transfers in
169.409371830 seconds between fair markers.

The NPT leaf-cache experiment regressed on a same-box application test. An
early four-page experiment also regressed, but that result was invalid as a
test of the intended design: its planner selected up to four pages while the
VM run loop enabled execution on only the second page.

| Candidate | Fair transfer interval | Same-box stable interval | Result |
| --- | ---: | ---: | --- |
| Cached NPT leaf locations | 176.949484576 s | 168.808642488 s | 8.14 s slower; 372.50 vs 338.29 billion VM-entry preparation cycles |
| Incomplete four-page experiment | 227.320667247 s | 168.808642488 s | Pages three and four remained NX; 43.84 vs 31.48 million nested-page faults |

The multi-page design was corrected to enable and restore execute permission
for every selected page. It passed the SVM smoke and REP/RF transition tests
and completed the full Tempo workload in each variant. On one EPYC 4245P box
with identical guest artifacts, the fair transfer results were:

| Variant | Transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Stable branch | 176.094182025 s | 32,436,093 | 356.19 billion |
| Corrected four-page, incremental proofs | 186.613921084 s | 25,999,264 | 432.53 billion |
| Four-page, deferred alias proof | 179.664074404 s | 26,526,446 | 400.78 billion |
| Three-page, deferred alias proof | 178.657330520 s | 26,173,720 | 400.91 billion |

The additional pages prevent roughly six million execute faults, but their
proof and permission work outweighs that saving. No multi-page variant was
merged into the shared branch.

A diagnostic-only physical-page transition table on the same box counted
31,806,028 transfer-phase pairs across 8,547 entries and dropped 21,531
events (0.07%). The hottest 32 pairs represented 12,760,568 faults; three
self-pairs alone represented 2,282,477. The hottest self-pair was page
`0x194f000` followed by itself 941,091 times. That page's one apparent
`F2 A5` hazard starts in the displacement of a `CALL` at
`0xffffffff8194f829` in `serial8250_tx_empty`. It cannot simply be ignored:
an indirect branch could enter the displacement bytes. A 20-second transfer
phase host sample attributed 8.60% of samples to
`collect_page_breakpoints` and 6.64% to `page_hazards_memo`.

The next experiment should explain why hot self-pair pages repeatedly fall
back to scalar execution and then reduce that fallback with a proof that
covers every executable alias and branch entry. Exact-byte and cross-page
safety checks remain necessary.

### Follow-up: same-page global planning

A diagnostic-only breakdown for the hottest self-pair pages found that
`0x194f000` had 947,200 accepted global plans and 947,152 calls rejected
because the page was untrusted before its scalar execute fault was selected.
Page `0x1000000` had 1,365,980 accepted plans; page `0x12ff000` instead had
1,405,037 zero-hazard promotions rejected by its cross-page boundary rule.
All counts are transfer-phase counts from a 4245P HWE box; the diagnostic run
passed all 10,000 transfers.

The run loop already reselected an untrusted code page after a two-page
global batch. Extending that rule to one-page batches passed local VMX and VM
tests, SVM smoke and REP/RF transitions, and the full Tempo workload. It did
not improve the application test on the same box:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Retain scalar page after one-page global batch | 170.319390307 s | 31,496,323 | 336.59 billion |
| Stable branch | 166.647476747 s | 31,334,765 | 332.29 billion |

This candidate remains isolated. The diagnostic counts show a common
replanning pattern, but retaining the scalar page did not reduce the
application's total fault or cycle count in this A/B run.

### Follow-up: composing independent alias proofs

A three-page global batch can reuse exact singleton alias proofs under the
same guarded CR3. The breakpoint union covers every executable alias of all
selected pages and must still fit four hardware slots. A focused unit test
confirmed composition without a new combined translation-tree walk. Local
EPT, VMX, and VM tests passed; box SVM smoke, REP/RF transitions, the short
`svm_workload`, and all full Tempo runs passed on an EPYC 4245P HWE box.

| Same box, run order | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Stable | 166.647476747 s | 31,334,765 | 332.29 billion |
| Singleton composition | 164.615981879 s | 25,570,486 | 354.83 billion |
| Singleton composition repeat | 170.655158863 s | 26,351,647 | 368.02 billion |
| Stable repeat | 175.857047558 s | 32,528,856 | 353.75 billion |
| Singleton composition third run | 188.958072817 s | 28,716,660 | 411.30 billion |

The two stable intervals averaged **171.252 seconds**. The three candidate
intervals averaged **174.743 seconds**, with a much wider spread. The fault
reduction is real, but extra and variable VM-entry preparation has not shown
a reliable application speedup. A transfer-phase CPU sample from the earlier
three-page variant attributed 13.95% of samples to
`collect_page_breakpoints`, versus 8.60% in the stable diagnostic sample.
The composition candidate remains isolated.

### Follow-up: sticky proven page set

An isolated variant retained the last proven physical code-page set across
global batches under the same CR3 and code epoch. It reused singleton alias
proofs and still revalidated current hazards and breakpoints. Local VMX (253)
and VM (40) tests passed, as did the 4245P box's SVM smoke and transition
checks. Both full Tempo runs passed all transactions on the same box.

| Same-box variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Sticky proven page set | 173.864143326 s | 27,088,437 | 371.73 billion |
| Stable branch | 174.016150900 s | 32,471,902 | 349.88 billion |

The 0.15-second difference is too small to establish a speedup. The variant
removed 5.38 million nested-page faults but added 21.85 billion VM-entry
preparation cycles. It remains isolated rather than merging a costlier
execution path without an application-level gain.

### Follow-up: skip non-candidate bytes in the hazard scan

An isolated scanner variant skips eight code bytes at once when none are
`0x0f`, `0xf2`, or `0xf3`, the only bytes that can start the hazardous
patterns under inspection. The existing VMX (253) and VM (40) tests passed;
an exhaustive marker-position test and box SVM smoke/transition checks also
passed. Both full Tempo runs passed all transactions on the same 4245P box.

| Same-box variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Eight-byte scan | 177.059517248 s | 32,841,818 | 358.57 billion |
| Stable branch | 178.132035466 s | 33,079,366 | 360.09 billion |

The 1.07-second difference is within the observed run-to-run variation. The
scanner remains isolated. This test also bounds its likely impact: optimizing
the byte scan alone does not address the approximately 33 million transfer
phase execute faults or 360 billion VM-entry preparation cycles.

### Guard hazardous page-boundary entries

Several hot code pages have no hazardous opcode wholly inside the page, but
end in a REP prefix or partial `0F` opcode. The global counter path rejected
them because the following virtual page could complete a REP string opcode,
SYSRET, or RDRAND/RDSEED. The new proof adds a hardware execution breakpoint
at every possible entry into that trailing suffix. It rejects the page if
those entries exceed the four hardware slots, and the existing alias walk
still covers every executable virtual mapping. Code writes still revoke the
proof. The global path can now count through these pages while trapping
before an uncertain cross-page instruction executes.

On one EPYC 4245P box with HWE 7.0.0-38 and identical guest artifacts, all
three full Tempo runs passed all transactions:

| Same box, run order | Fair transfer interval | Nested-page faults | Monitor-trap exits | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| Guarded boundary entries | 124.300178238 s | 32,517,450 | 7,532,646 | 226.93 billion |
| Stable branch | 176.986204669 s | 33,121,850 | 10,901,402 | 354.14 billion |
| Guarded boundary entries repeat | 126.668687370 s | 32,307,997 | 7,491,880 | 238.06 billion |

The candidate's two transfer intervals average **125.48 seconds**, about
**29% faster** than the same-box stable run. Nested-page faults changed
little; fewer monitor-trap exits and substantially less VM-entry preparation
explain the measured gain. The candidate also reached the first fair marker
at 270.29 and 270.31 host seconds, versus 316.66 for stable.

Local VMX (255) and VM (40) tests passed. On the disposable box, SVM smoke
and transition checks passed, including new hardware regressions that execute
`REP MOVSB` and intercept `RDRAND` across a code-page boundary in 7 and 8
VM exits respectively. The AMD transfer remains about **5.2×** the
historical Intel 24.087-second interval; the roughly 32 million execute
faults per transfer are the next major performance target.

### Cache the boundary proof result

A 25-second transfer-phase host CPU sample of the guarded-boundary build
attributed 22.15% of samples to `svm_run_guest`, 12.12% to
`collect_page_breakpoints`, 5.75% to `page_hazards_memo`, and 3.69% to
`boundary_start_hazard`. The sample is saved in ignored
`target/boxctl-evidence/amd-boundary-profile-1010/`. The last check was
recomputing the same trailing-entry decision on repeated VM entries even
though the code-page proof had not changed.

The proof now records whether all possible trailing entries are guarded,
and that bit travels with the hazard memo and code cache. It is set only
after the exact-byte scan has added every required breakpoint; stale code
proofs are still invalidated on writes. Local VMX (255) and VM (40) tests,
box SVM smoke and transitions, and the cross-page REP and RDRAND hardware
regressions passed. Both full Tempo runs passed all transactions.

| Same-box variant | Fair transfer interval | Nested-page faults | Monitor-trap exits | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| Guarded boundary entries | 124.300178238 s | 32,517,450 | 7,532,646 | 226.93 billion |
| Guarded boundary entries repeat | 126.668687370 s | 32,307,997 | 7,491,880 | 238.06 billion |
| Cached boundary proof | 115.139916261 s | 32,320,316 | 7,473,493 | 192.42 billion |
| Cached boundary proof repeat | 116.194837839 s | 32,515,813 | 7,495,768 | 195.67 billion |

The cached variant averaged **115.67 seconds**, **7.8% faster** than the
two guarded-boundary runs on this box. It is still about **4.8×** the
historical Intel 24.087-second interval. Alias breakpoint collection and
roughly 32 million nested-page faults per transfer are the largest remaining
opportunities visible in this profile.

### Recheck three-page execution after the boundary changes

The earlier three-page global counter variant was rebased onto the cached
boundary-proof build. It reused independently proved executable aliases of
each page, selected up to three hazardous code pages when their breakpoints
fit the four hardware slots, and enabled/restored NPT execute permission
for every selected page. A focused unit test includes a trailing REP prefix
on the third page. Local VMX (255) and VM (40) tests, box SVM smoke and
transition tests, and both full Tempo runs passed.

| Same-box variant | Fair transfer interval | Nested-page faults | Monitor-trap exits | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| Three-page alias-proof composition | 127.526130457 s | 25,223,935 | 7,596,236 | 279.10 billion |
| Current shared branch | 114.800815318 s | 31,980,690 | 7,474,027 | 194.82 billion |

The additional executable pages removed **6.76 million** execute faults,
but preparation added **84.28 billion cycles** and the transfer regressed by
**12.73 seconds**. The candidate remains isolated. Reducing page faults by
expanding a guarded set does not pay off while its per-entry proof and
permission work is this expensive.

A 25-second transfer-phase host `perf` sample of the three-page build
attributed **21.39%** of samples to `collect_page_breakpoints`, versus
12.12% in the prior guarded-boundary profile. Its report and raw sample
are saved under ignored
`target/boxctl-evidence/amd-boundary-multipage-profile-1010/`.
The sample explains much of the added VM-entry work but does not imply that
alias collection is its only source.

A follow-up cached the combined alias proof after composing its independently
proved pages, allowing an exact set hit on subsequent entries. A unit test
verified the combined proof was stored; local tests, box smoke/transitions,
and the full Tempo workload passed. It did not improve the application run:

| Same box | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Three-page composition | 127.526130457 s | 25,223,935 | 279.10 billion |
| Three-page composition with combined proof cached | 129.837277249 s | 25,180,362 | 287.99 billion |
| Shared branch | 114.800815318 s | 31,980,690 | 194.82 billion |

Both three-page variants remain isolated. The extra proof cache write may
evict useful singleton proofs or add cost itself; this A/B result does not
distinguish those causes.

A further isolated variant held the composed proof in the single scratch slot
instead of inserting it into the set-associative cache. That let repeated
three-page sets hit without evicting singleton proofs. A focused unit test
verified the scratch hit still works after clearing the singleton entries;
the full VMX suite (255 tests), box SVM smoke and transitions, and the Tempo
transfer all passed. Its fair transfer interval was **126.414743403 seconds**,
with **24,965,933** nested-page faults, **7,497,750** monitor-trap exits, and
**276.81 billion** VM-entry overhead cycles. This is modestly better than the
127.53–129.84-second multi-page variants but remains **11.61 seconds slower**
than the same-box shared build at 114.80 seconds. Proof-cache eviction is
therefore part of the cost, but avoiding it alone does not make the wider
execution guard profitable. This variant remains isolated too.

### Profile the current fast branch

The current shared build was reinstalled on the same 4245P box and passed
SVM smoke, transitions, and the full Tempo transfer. With a 25-second
`perf` sample taken during the transfer, its fair interval was
**121.634595021 seconds**; sampling overhead means the earlier unprofiled
**114.800815318-second** run remains the timing baseline. The profiled
interval had 33,309,597 nested-page faults, 7,583,576 monitor-trap exits,
and 211.02 billion VM-entry overhead cycles. No samples were lost.

The host CPU sample attributed **22.89%** to `svm_run_guest`, **12.10%** to
`collect_page_breakpoints`, **6.64%** to `page_hazards_memo`, and **6.50%**
to `__pi_memcpy`. Raw data, `perf report`, logs, and marker statistics are
under ignored `target/boxctl-evidence/amd-stable-profile-1010/`. The
breakpoint collector remains the largest software preparation hotspot in
the fast branch. The three-page tests above show that reducing faults alone
does not help when alias-proof work grows on each entry.

### Reuse singleton proofs when widening the global guard

The earlier three-page experiment tried a combined alias walk when its cache
missed, which made fewer execute faults more expensive overall. The new
candidate first proves the current hazardous page, then admits up to two
recently executed hazardous pages only when each already has an exact,
independently cached alias proof under the guarded CR3. It unions their
virtual breakpoints within the four hardware slots and retains the existing
page-boundary compatibility checks. Missing proofs keep the single-page
guard; they never trigger a multi-page walk. A focused unit test covers
three-page composition, a boundary prefix, and the missing-proof fallback.
Local VMX (255) and VM (40) tests and box SVM smoke and transitions passed.

The exact 10,000-transfer workload passed in an A/B/A sequence on the same
EPYC 4245P box with HWE 7.0.0-38:

| Same box, run order | Fair transfer interval | Nested-page faults | Monitor-trap exits | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| Cached singleton composition | 92.269159825 s | 25,166,260 | 7,541,192 | 142.37 billion |
| Shared branch | 119.303314462 s | 32,966,515 | 7,509,387 | 203.58 billion |
| Cached singleton composition repeat | 91.286394400 s | 24,926,763 | 7,501,610 | 140.19 billion |

The candidate averaged **91.78 seconds**, **23.1% faster** than the same-box
shared-branch run, and about **3.8×** the historical Intel 24.087-second
interval. Both candidate runs reduced nested-page faults by about 7.8–8.0
million and VM-entry preparation by more than 61 billion cycles, without
adding material monitor-trap exits. Logs and timing snapshots are under
ignored `target/boxctl-evidence/amd-cached-multipage-1010/`.

Using the fourth breakpoint slot to admit a fourth hazardous page passed the
focused unit test, box smoke/transitions, and all 10,000 transfers. Its
same-box fair interval was **92.734263188 seconds**, with **24,248,853**
nested-page faults, **7,541,298** monitor-trap exits, and **148.81 billion**
VM-entry preparation cycles. It removed another 0.68–0.92 million faults
relative to the two three-page runs but added 6–9 billion preparation cycles
and was slightly slower. The four-page variant remains isolated; three pages
are the best measured limit for this workload.

A 25-second transfer-phase `perf` sample of the three-page build captured
2,502 samples with none lost. Its complete 10,000-transfer run passed in
**92.055585282 seconds**; the interval had **24,699,672** nested-page
faults and **143.68 billion** VM-entry preparation cycles. The sample put
**24.99%** of cycles in `svm_run_guest`, **9.80%** in `page_hazards_memo`,
**6.21%** in `slice_contains`, **3.36%** in `prepare_global`, and **2.83%**
in `collect_page_breakpoints`. The latter fell from 12.10% in the earlier
fast-branch profile, confirming that cached singleton composition removed
the expensive repeated alias walk. The profile and timing snapshots are
saved under the same ignored evidence directory. Remaining software work is
concentrated in hazard lookup and membership checks; roughly 25 million
nested-page faults still dominate the exit pattern.

### Index the guarded table set

The three-page profile spent 6.21% of host CPU samples in slice membership
checks, chiefly while the global planner checked whether a code page was one
of roughly 1,024 guarded page-table frames. An exact, open-addressed index
of the active guarded tree now replaces that linear scan. It is built once
after a complete guarded-tree installation and invalidated before rebuild or
teardown; while unavailable, lookup uses the original exact scan. A unit
test covers a hash collision and fallback during rebuild. The 256 VMX and
40 VM library tests, box SVM smoke and transition suite, and both full Tempo
runs passed.

| Same EPYC 4245P box | Fair 10,000-transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Three-page build, unprofiled | 92.269159825 s | 25,166,260 | 142.37 billion |
| Three-page build, unprofiled repeat | 91.286394400 s | 24,926,763 | 140.19 billion |
| Indexed guarded tables | 88.547379967 s | 24,992,535 | 129.46 billion |
| Indexed guarded tables repeat | 88.925999722 s | 25,022,687 | 130.07 billion |

The indexed variant averaged **88.74 seconds**, about **3.3% faster** than
the preceding three-page pair. Fault counts were similar, while VM-entry
preparation fell about 11 billion cycles. All four runs completed and passed
the same guest workload on the same box. Logs and marker snapshots are under
ignored `target/boxctl-evidence/amd-cached-multipage-1010/`. The AMD interval
is still about **3.7×** the historical Intel 24.087-second result.

### Hazard guard limit and fault diagnosis

On another EPYC 4245P box, diagnostic builds compared the existing eight-page
recurrent-code guard limit with 16 and 32 pages. Each completed the full
10,000-transfer workload. The fair intervals were **88.262 s** (eight),
**88.330 s** (32), and **86.781 s / 89.029 s** (16, two runs). The 16-page
variation straddled the baseline and 32 pages increased nested-page faults;
there is no repeatable speedup to merge. The hazard lookup hint hit on over
99.8% of lookups, but the eight-page run still did about 334 million page-byte
comparisons. Increasing the limit to 32 reduced that to about 268 million
while adding roughly 750,000 nested-page faults, offsetting the gain.

A separate instrumented NPF run passed in **86.897 s** with **24,906,008**
transfer-phase nested-page faults and **7,516,588** monitor-trap exits. Across
the run, the NPF diagnostic counted 83.9 million faults, of which 82.1 million
(97.8%) were instruction fetches; almost every fetch took the scalar path.
These diagnostic counts include boot and setup, while the transfer-phase counts
above come from the exact fair-timing markers. The remaining bottleneck is
therefore executing across hazardous code pages, not page-table writes or
hazard-hint misses. Logs, marker JSON and diagnostic dmesg are in the ignored
`target/boxctl-evidence/amd-hazard-guard-limit-1010/` and
`target/boxctl-evidence/amd-npf-diag-1010/` directories. The diagnostic
instrumentation and guard-limit variants remain in isolated worktrees.

### Select a likely successor code page

A same-box test of retaining the current hazardous page after three-page
global batches passed local and SVM tests but only reduced the fair interval
from **89.889 s** to **89.136 s** and nested-page faults from **25,181,771**
to **25,013,709**. This is too small against observed run-to-run variation;
the change remains isolated.

An execute-fault diagnostic on the unchanged branch recorded where the faulting
guest RIP appeared in the recent-page set. SVM did not provide a valid guest
linear address for these faults, so the diagnostic used guest RIP plus CS base.
Over a 25,165,824-fault window aligned to the transfer interval, **0.5%** were
on the same page, **12.1%** on the previous page, **10.0%** on the second most
recent page, **44.3%** on older recent pages, and **33.0%** outside the eight-page
set. The full diagnostic run passed in **88.158 s**; the rank percentages use
nearby dmesg snapshots rather than exact timing markers.

The next candidate records the last observed successor for each virtual code
page and guarded CR3. When forming a global batch, it tries that page before
the existing recent-page list. It still requires the exact translation, byte,
cross-page boundary, alias, and breakpoint proofs before enabling execution.
The 256 local VMX tests, box SVM smoke and transition checks, and two complete
10,000-transfer runs passed. The same-box fair results so far are:

| Variant | Transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Unchanged branch | 89.889 s | 25,181,771 | 132.72 billion |
| Successor-guided guard | 86.217 s | 23,698,509 | 128.26 billion |
| Successor-guided guard repeat | 87.102 s | 23,844,310 | 130.55 billion |
| Unchanged branch repeat | 89.088 s | 25,019,706 | 131.28 billion |

The two successor runs average **86.659 s** versus **89.489 s** for the
bracketing unchanged runs, a **3.2%** improvement. They remove roughly
1.3–1.5 million nested-page faults in each matched comparison. Raw logs and
marker snapshots are under ignored `target/boxctl-evidence/amd-successor-1011/`.

An isolated 8,192-entry successor table passed the same SVM checks and full
workload, but its single fair interval was **87.197 s** with **24,076,128**
nested-page faults, more than either 2,048-entry successor run. Increasing
capacity alone has not shown a benefit, so the shared branch retains 2,048
entries. The next useful diagnostic is to separate failed successor predictions
from candidates rejected by the existing alias, boundary, or breakpoint proof:
the predictor removed only about 1.4 million faults despite most remaining
faults targeting pages beyond the two most recent.

### Diagnose successor accuracy

On a fresh EPYC 4245P box, an instrumented one-successor run passed all
10,000 transfers in **86.624 s**. During an 18.9-million-transition window
near the transfer markers, the last observed successor was correct **25.8%**
of the time, wrong **73.7%**, and absent **0.6%**. A larger table cannot fix
mostly wrong predictions. Planner counters also showed that an expected
successor was often admissible, so a second candidate might help.

A two-successor experiment retained the two most recent distinct destinations
per source page and tried both before the general recent-page list. It passed
257 local VMX tests, 40 VM tests, box SVM smoke and transition checks, and two
complete Tempo runs. The same-box fair transfer results were:

| Variant | Transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Two successors | 84.989 s | 22,953,175 | 126.82 billion |
| One successor, uninstrumented | 85.980 s | 23,403,932 | 129.04 billion |
| Two successors repeat | 86.844 s | 23,543,467 | 129.30 billion |

The candidate's first-run improvement did not repeat. Its two-run average was
**85.917 s**, effectively the same as the one-successor control, and its fault
count also varied across the control. The second successor remains isolated;
the shared branch still uses one. Raw logs and marker snapshots are in the
ignored `target/boxctl-evidence/amd-pred-diag-1011/` and
`target/boxctl-evidence/amd-successor-two-1011/` directories.

### Reuse write guards for active hazardous code

A 25-second sample of the shared one-successor build during the transfer phase
had 2,498 samples with none lost. The leading host-cycle symbols were
`svm_run_guest` **25.46%**, `page_hazards_memo` **9.38%**,
`collect_page_breakpoints` **4.85%**, and `protect_recurrent_code` about **2.07%**.
The memo skips a full code-byte comparison only for NPT write-guarded pages,
but the existing eight slots stay with the first pages guarded during boot.

An initial stale-guard replacement reduced faults but regressed to **89.520 s**
on the first box. Its profile showed `protect_recurrent_code` around **6.3%**
of samples because every full-cache attempt scanned all 256 memo entries for a
victim. The corrected version keeps an exact guard count, considers eviction
only every 65,536 hazard lookups, and replaces a page only if it has not been
consulted for at least 1,048,576 lookups. It invalidates the evicted byte proof
before restoring write permission. A focused test confirms that changed code
bytes are rescanned after eviction. The 257 VMX and 40 VM library tests and
box SVM smoke, transition, and workload checks passed.

The corrected build first passed the complete 10,000-transfer test in
**83.524 s** on the original box. A fresh EPYC 4245P box then measured the
same artifacts in candidate/control/candidate order:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles | Total run cycles |
| --- | ---: | ---: | ---: | ---: |
| Adaptive guard | 81.571 s | 23,081,515 | 121.94 billion | 316.51 billion |
| Unchanged one-successor branch | 88.815 s | 24,326,179 | 133.70 billion | 344.60 billion |
| Adaptive guard repeat | 83.122 s | 23,694,563 | 123.08 billion | 322.52 billion |
| Guard count only, no replacement | 86.018 s | 23,982,616 | 132.02 billion | 333.76 billion |

The adaptive pair averaged **82.347 s**, **7.3% faster** than the same-box
control and about **3.4×** the historical Intel 24.087-second interval. The
count-only result shows that avoiding repeated scans helps, but rotating old
guards adds a further measurable gain. The AMD-to-native comparison remains
roughly **6.3×** against the historical 13.109-second AMD native mean. Logs,
marker snapshots and perf reports are in ignored
`target/boxctl-evidence/amd-profile-successor-1011/`,
`target/boxctl-evidence/amd-stale-guard-1011/`, and
`target/boxctl-evidence/amd-stale-guard-ab-1011/`.

### Profile the adaptive guard build

The pushed adaptive build (`a3bb1cb`) passed another complete 10,000-transfer
run on the same EPYC 4245P box in **83.400 s** between fair markers. The
transfer interval recorded **23,451,327 nested-page faults**, **7,479,580
monitor-trap exits**, and **126.66 billion VM-entry preparation cycles**. The
guest emitted `TEMPO_TXGEN_PASS`.

A 25-second, 99 Hz transfer-phase `perf` sample collected 2,494 samples with
none lost. The leading symbols were `svm_run_guest` **27.04%**,
`page_hazards_memo` **9.49%**, and `collect_page_breakpoints` **5.33%**.
`protect_recurrent_code` was only **0.08%**, confirming that the bounded
replacement check removed its earlier scan overhead. These sample shares are
not a cycle decomposition, but they point to the still-high exit rate and
per-exit code-page preparation as the next bottlenecks. The log, marker
snapshots, and perf report are under ignored
`target/boxctl-evidence/amd-profile-adaptive-1011/`.

### Retain sixteen active code guards

With stale guards now replaced, the eight-page write-guard limit can leave
frequently used hazardous pages unguarded. An isolated sixteen-guard build
passed 257 VMX and 40 VM library tests, the EPYC 4245P box SVM suite, and two
complete 10,000-transfer runs. In candidate/control/candidate order on the
same box, using identical guest artifacts:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Sixteen guards | 82.804 s | 23,833,530 | 122.47 billion |
| Eight guards | 84.514 s | 23,842,225 | 127.04 billion |
| Sixteen guards repeat | 82.233 s | 23,911,842 | 120.14 billion |

The sixteen-guard pair averaged **82.519 s**, **2.4% faster** than the
same-box control. Fault counts were nearly unchanged; the lower VM-entry
preparation cycles in both candidate runs are consistent with more code-byte
proofs being retained. This is a small paired gain, not a claim that the
remaining **3.4×** gap to the historical Intel run is solved. Guest logs and
marker snapshots are under ignored `target/boxctl-evidence/amd-guard16-1011/`.

### Use the fourth hardware breakpoint in global batches

The global AMD batch previously stopped at three hazardous code pages even
when each page needed only one breakpoint. A focused test now verifies that a
four-page batch can use all four hardware breakpoint slots while retaining
the exact translation, alias, and boundary checks. The 258 VMX and 40 VM
library tests, box smoke and transition checks, and three full Tempo runs
passed. On the same EPYC 4245P box used above:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Eight guards, three-page global cap | 84.514 s | 23,842,225 | 127.04 billion |
| Eight guards, four-page global cap | 82.333 s | 22,951,250 | 124.47 billion |
| Sixteen guards, three-page global cap, two-run mean | 82.519 s | 23,872,686 | 121.30 billion |
| Sixteen guards, four-page global cap | 81.928 s | 23,244,395 | 122.53 billion |
| Sixteen guards, four-page global cap repeat | 82.330 s | 23,410,724 | 122.46 billion |

The combined pair averaged **82.129 s**, about **0.4 s faster** than the
sixteen-guard pair and **2.4 s faster** than the eight-guard control. This
small timing difference needs more sampling to quantify precisely, but both
combined runs had fewer nested-page faults than either sixteen-guard run.
The guest reported `TEMPO_TXGEN_PASS` every time. Logs and marker snapshots
are under ignored `target/boxctl-evidence/amd-fourpage-1011/` and
`target/boxctl-evidence/amd-combined-1011/`.

### Condition the next-page prediction on the previous page

A diagnostic table keyed by guarded CR3, previous code page, and current code
page compared next-page predictions against the existing current-page-only
table without changing execution. Across 28 million observed transitions in
the dmesg snapshots near the transfer markers, the existing predictor hit
**24.7%** and the contextual predictor hit **59.8%**; contextual lookup was
cold **2.0%** of the time. The snapshots bracket nearby million-transition
logging points rather than the exact marker instants. The diagnostic guest
completed all 10,000 transfers.

The production change uses the contextual destination first, falling back to
the current-page predictor when no contextual entry exists. Every proposed
page still passes the translation, exact-byte, boundary, alias, and hardware
breakpoint proofs before execution is enabled. The 258 VMX and 40 VM library
tests, box SVM checks, and complete Tempo workload passed. On the same EPYC
4245P box, in candidate/control/candidate order:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Contextual predictor | 79.935 s | 22,021,776 | 121.96 billion |
| Current-page predictor | 82.876 s | 23,473,794 | 124.03 billion |
| Contextual predictor repeat | 80.596 s | 22,557,277 | 120.47 billion |

The contextual pair averaged **80.265 s**, **3.1% faster** than the same-box
control, and saved roughly **0.9–1.5 million** nested-page faults per run.
The AMD interval remains about **3.3×** the historical Intel 24.087-second
result. Diagnostic and run logs are under ignored
`target/boxctl-evidence/box-a5046a49/` and
`target/boxctl-evidence/amd-second-order-1011/`.

### Check a third page of prediction history

An isolated diagnostic also keyed the next-page prediction by the two pages
preceding the current page. Across 28 million transitions in snapshots near
the transfer markers, the two-page-context predictor hit **60.0%**, while
three-page context hit **70.8%**; three-page lookup was cold **3.0%** of the
time. The diagnostic completed all 10,000 transfers. The production variant
passed 258 VMX and 40 VM library tests, box smoke and transition checks, and
two complete Tempo runs, but the extra accuracy did not improve elapsed time:

| Same EPYC 4245P box | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Two-page context, two-run mean | 80.265 s | 22,289,527 | 121.21 billion |
| Three-page context | 80.989 s | 21,947,097 | 124.97 billion |
| Three-page context repeat | 80.318 s | 21,920,328 | 123.15 billion |

The three-page pair averaged **80.653 s**, about **0.4 s slower** despite
roughly 0.36 million fewer faults per run. The larger table and lookup path
remain isolated; the shared branch retains two-page context. Diagnostic and
run evidence is under ignored `target/boxctl-evidence/box-a5046a49/` and
`target/boxctl-evidence/amd-third-order-1011/`.

### Diagnose prediction admission and try a fallback candidate

A box-only diagnostic of the shared two-page predictor passed the full Tempo
workload in **79.072 s**. Across 26 million observed transitions in dmesg
snapshots near the transfer markers, the planner had a predicted candidate
22.58 million times and admitted it 15.00 million times (**66.4%**).
Of the remaining attempts, 2.86 million failed hazard or boundary rules,
1.24 million had no free breakpoint slot, 3.00 million targeted an already
selected or otherwise ineligible page, 0.46 million lacked a usable hazard
proof, and only 0.03 million lacked a cached alias proof. These categories
count planner attempts rather than distinct page faults. The primary
prediction matched 8.99 million of the 26 million *remaining observed*
transitions; prediction changes which transitions are observed, so this rate
must not be compared directly with the earlier baseline diagnostic's 59.8%.

An isolated variant tried the current-page predictor after the contextual
prediction on every global plan. A second variant tried that fallback only
when the contextual page could not be admitted. Both passed 258 VMX and 40 VM
library tests, box smoke and transition checks, and all 10,000 transfers.
On the same EPYC 4245P box:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Two candidates unconditionally | 82.415 s | 22,417,832 | 127.44 billion |
| Unchanged shared branch | 80.343 s | 22,259,765 | 122.00 billion |
| Fallback only after admission failure | 80.377 s | 22,164,127 | 122.26 billion |

Unconditionally checking both candidates regressed by **2.07 s**. The
conditional fallback was effectively tied with the control, so neither
variant was merged. Logs and marker snapshots are under ignored
`target/boxctl-evidence/box-91a9ef06/`,
`target/boxctl-evidence/amd-two-pred-1011/`, and
`target/boxctl-evidence/amd-fallback-1011/`.

### Check redundant XCR0 switching

The current profile attributes about **7% of `svm_run_guest` samples** to its
two `XSETBV` instructions. An isolated assembly variant skipped those writes
when the saved host XCR0 equaled the guest XCR0. It passed box SVM smoke and
transition checks and all 10,000 transfers in two full runs. The same-box
candidate/control/candidate intervals were **79.289 / 82.714 / 81.281 s**;
the earlier unchanged run on that box was **80.343 s**. Fault counts differed
by more than a million between runs, so the wall-time variation is not
evidence of an XCR0 improvement.

The box's measured host XCR0 was **`0x2e7`**. Bedrock exposes only x87, SSE,
and AVX to this guest, initializes its XCR0 to **`0x7`**, and recorded zero
guest XSETBV exits in both candidate and control transfer intervals. Thus the
equality branch could not skip the writes on this workload. The variant was
not merged. Evidence is under ignored
`target/boxctl-evidence/amd-skip-xcr0-1011/`.

### Retain exact-byte proofs for very hot rejected pages

An instrumented full Tempo run on a fresh EPYC 4245P box passed in **81.445 s**.
Across nearby diagnostic snapshots around the transfer markers, the global
planner attempted roughly 30 million batches and rejected about 5.4 million
because the current page had no usable hazard scan. A second full run passed
in **81.022 s** and showed that more than 7.2 million hazard-memo rejection
lookups over a similar interval were for pages with more entries than the four
hardware breakpoint slots; only about 0.16 million were for forbidden boundary
bytes. These memo lookup counts include calls outside the global planner.
The hot-page diagnostic then passed in **80.004 s**; every logged hot rejection
during its transfer interval contained `REP` string patterns, with no RDRAND
or SYSRET patterns. The hash-table hint was already effective, so this points
to repeated 4 KB exact-byte comparisons on pages that cannot enter a global
batch, rather than alias-cache misses.

The retained change write-protects a rejected memo only after 4,096 uses,
allowing its exact-byte rejection to be reused until a guest or host write
invalidates it. It raises the total guard ceiling from 16 to 24 only for
these hot rejected pages. It does not make the page globally executable or
bypass the hazard proof or scalar fallback. A focused test confirms that a guest
write revokes the guard and changed bytes are rescanned. The 259 VMX and 40
VM library tests, box SVM smoke and transition checks, and four complete
Tempo workloads passed. On the same box, in candidate/control/candidate/control
order:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Guard hot rejected pages | 80.872 s | 22,586,661 | 123.61 billion |
| Unchanged branch | 82.427 s | 22,623,867 | 126.69 billion |
| Guard hot rejected pages, repeat | 79.376 s | 21,990,253 | 121.87 billion |
| Unchanged branch, repeat | 82.155 s | 22,706,810 | 125.23 billion |

The candidate pair averaged **80.124 s** versus **82.291 s** for the controls,
a **2.6%** improvement with about **3.22 billion** fewer preparation cycles
per run. The AMD interval remains about **3.3×** the historical Intel
24.087-second workload. Logs and marker snapshots are under ignored
`target/boxctl-evidence/amd-global-diag-1011/` and
`target/boxctl-evidence/amd-rejected-guard-1011/`.

A further full candidate run passed in **79.652 s** while attempting a
transfer-phase perf sample. The process-attached sample captured only 41
kernel samples, and the system-wide kernel-only sample was dominated by idle
threads. Neither sample is suitable for attributing the remaining Bedrock
hotspots. The raw data is under ignored
`target/boxctl-evidence/amd-rejected-guard-profile-1011/`.

### Investigate AMD breakpoint-address masks (2026-10-11)

The EPYC 4245P box reports both AMD breakpoint-address extension CPUID bits
(`Fn8000_0001_ECX[26,30]`). All four DR0–DR3 mask MSRs can be read, written
with a low-bit mask, and restored on a disposable box. The
[AMD64 system programming manual](https://docs.amd.com/api/khub/documents/sD1_QL~h4Afq2_tvzxqqSQ/content)
specifies that set mask bits exclude those address bits from instruction
breakpoint comparison. This permits one breakpoint to cover several unsafe
entries in an aligned range.

A short instrumented guest run recorded nine hot rejected-page samples, with
five to seven hazardous instruction entries on most pages. For example, page
`0x184b000` had offsets `2071, 2183, 3575, 3576, 3735`; four aligned ranges
can cover them in only 19 bytes total. Page `0x118ee2000` had six entries
coverable in four ranges spanning 98 bytes. One six-entry page needs a
512-byte range under a minimum-byte-span four-range plan, so extra harmless
debug exits may limit the gain. The raw samples are in ignored
`target/boxctl-evidence/amd-mask-diagnostic-1011/hot-rejected.log`. This
instrumented run reached its 340-second wall-clock timeout before the Tempo
transfer markers, so it supplies address distributions, not a performance
comparison.

An initial implementation expanded the hazard scan to 16 entries, grouped
virtual addresses into at most four masked ranges, and saved/restored host
mask MSRs around VMRUN. It built and passed existing SVM smoke/transition
checks, but seven VMX unit tests then failed because they expected the old
four-entry fallback. A targeted five-entry hardware probe made a box
unresponsive. The same probe passed with the last known-good module in 29
exits, while two candidate controls also locked up with mask use disabled;
one of those additionally restored the original VMRUN assembly and four-entry
scan limit. That first prototype was not pushed. The failure was traced to
kernel-stack usage as described below.

### Resolve stack overflow and validate masked breakpoints

The prototype failure came from expanding a 64-element `PageHazards` array on
the kernel stack from about 3.5 KB to over 5 KB. That planner can select only
the current page plus eight recent pages. Reducing its local array to nine
elements eliminated the box lockups; the same five-entry hardware probe now
passes with four masked breakpoints in 44 exits. The 260 VMX and 40 VM library
tests and box SVM smoke/transition checks pass. Pages with more than 16 unsafe
entries, or ranges too wide for the bounded four-slot plan, retain the scalar
fallback. CPUs without AMD's breakpoint-mask feature retain the four-entry
hazard scan.

Three full 10,000-transfer runs passed on the same EPYC 4245P box, with the
same guest artifacts, command line, and benchmark command, in candidate /
unchanged module / candidate order:

| Variant | Transfer interval | Transfer start after launch |
| --- | ---: | ---: |
| Masked breakpoints | 75.673 s | 182.114 s |
| Unchanged module | 79.175 s | 233.593 s |
| Masked breakpoints, repeat | 75.137 s | 182.631 s |

The candidate pair averaged **75.405 s**, **4.8%** faster than the single
same-box control. The transfer phase also started about **51 s** sooner. In
the first candidate/control full-run comparison, single-step exits fell from
62.98 million to 27.70 million and nested-page faults from 76.63 million to
75.06 million. The confirming candidate's transfer-phase marker snapshots
show 22.40 million nested-page faults, 4.45 million single-step exits, and
99.31 billion VM-entry overhead cycles. The AMD transfer interval remains
about **3.1×** the historical Intel 24.087-second run. Logs and confirming
marker snapshots are under ignored
`target/boxctl-evidence/amd-masked-breakpoints-1011/`.

### Test broader speculative-page admission with masked breakpoints

The planner still rejects an optional code page when the raw hazard count
across selected pages exceeds four. An experiment removed that preliminary
limit on CPUs with breakpoint-address masks, leaving the alias and breakpoint
collector to prove whether the combined pages fit. It passed 260 VMX and 40
VM library tests, box SVM checks, and the full 10,000-transfer workload. A
same-box control, run immediately afterward with the unchanged module, showed
the broader admission was slower:

| Variant | Transfer interval | Nested-page faults | VM-entry overhead cycles |
| --- | ---: | ---: | ---: |
| Broader optional-page admission | 77.513 s | 22,542,328 | 106.83 billion |
| Unchanged masked-breakpoint module | 74.529 s | 22,243,158 | 98.65 billion |

The candidate added 299,170 faults and 8.18 billion preparation cycles, so
it was reverted. Both runs included all transfers without failure or revert.
Logs and marker snapshots are under ignored
`target/boxctl-evidence/amd-mask-optional-pages-1011/`.

The global counted-execution planner has its own four-raw-hazard limit. A
second experiment combined cached masked singleton proofs across pages there.
A focused ten-hazard, two-page unit test, all 261 VMX tests, 40 VM library
tests, and the box smoke/transition suite passed. It was unusably slow in a
full guest: after two minutes of 100% CPU, boot had reached only virtual time
0.279 seconds. The unchanged module had already reached image import by then.
The run was stopped, and the change was reverted. Its boot log is saved beside
the paired optional-page logs above.

A CPU-pinned `perf` cycle sample of the unchanged module during the transfer
captured only five samples in 25 seconds; a concurrent `perf stat` cycle count
hit the 47-bit counter maximum. Bedrock owns the PMU while counting guest
instructions, so this sample cannot attribute its host costs.

Temporary host-TSC probes then measured the unchanged masked-breakpoint
module during a passing full Tempo run. Between the transfer markers, the
global counted-execution planner ran about **30.4 million** times and used
**66.1 billion cycles**. Its nested breakpoint collector used **20.1 billion
cycles** over 25.9 million calls, and cached code-hazard lookup used **20.5
billion cycles** over 86.5 million calls. Translation-tree collection used
only **0.10 billion cycles**. These nested figures must not be added to the
global total. The probes added several seconds to the transfer, so the
unprofiled paired runs above remain the timing baseline. Dmesg snapshots at
both fair markers are saved in ignored
`target/boxctl-evidence/amd-internal-profile-1011/`; the probes were removed
afterward.

A two-entry most-recently-used shortcut for the code-proof lookup was then
tested against the unchanged module on the same box. The shortcut passed 260
VMX and 40 VM library tests, the box suite, and all 10,000 Tempo transfers,
but its transfer interval was **76.310 s** versus the unchanged **74.529 s**.
It also added 416,459 nested-page faults and 3.47 billion VM-entry overhead
cycles. It was reverted; the log and marker snapshots are under ignored
`target/boxctl-evidence/amd-code-mru-1011/`.
