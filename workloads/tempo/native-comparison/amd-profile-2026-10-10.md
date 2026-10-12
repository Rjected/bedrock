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

A subsequent experiment cached a singleton global execution plan for a
write-guarded code page, keyed by linear page and translation root. Reuse
required the current code-hazard memo revision, NPT mapping generation,
translation-tree generation, and alias-breakpoint proof to match. Initial
exact-RIP and page-level probes had no hits because the short-lived code proof
was cleared between calls. Restricting reuse to write-guarded memos yielded
about 1.05 million hits in the first 5 million probes. The guarded version
passed 261 VMX tests, 40 VM library tests, the box suite, and repeated full
10,000-transfer runs. On the same EPYC 4245P box, three candidate runs took
77.112, 76.747, and 75.242 seconds; interleaved unchanged controls took
77.949, 76.603, and 76.853 seconds. The candidate mean was 76.367 seconds
versus 77.135 seconds for the controls, a gain of about 1% within run noise.

The cache-hit path also needed to maintain the hazard memo's use clock and
hit count, which govern write-guard retention. After that correction, all
tests and 10,000 transfers still passed, but the fair interval was **77.502
seconds**, slightly slower than the unchanged control mean. The code added
substantial cache state for no reliable application-level gain, so it was
reverted. Logs, marker snapshots, and the final candidate patch remain in
ignored `target/boxctl-evidence/amd-global-plan-cache-1011/`.

### Check code-hazard memo lookup indexing

The code-hazard lookup uses a direct-mapped hint and scans up to 256 resident
proofs when the hint misses. A four-way hint set passed 260 VMX tests and the
box suite, then passed all 10,000 transfers in **77.397 s**. The unchanged
module took **76.537 s** on that same box. This was a regression, so the
four-way variant was reverted. Its logs and patch are under ignored
`target/boxctl-evidence/amd-hazard-fourway-1011/`.

A second experiment replaced the full scan with linked hash buckets containing
every resident proof. Collision and eviction tests, all 260 VMX tests, 40 VM
library tests, the box suite, and all 10,000 transfers passed. Its fair
interval was **75.898 s**, bracketed by unchanged runs at **76.537 s** and
**74.682 s**. The candidate is slower than the two-control mean of **75.610
s**; the differences are smaller than run-to-run variation. It was reverted.
Logs, marker snapshots, and the patch are under ignored
`target/boxctl-evidence/amd-hazard-index-1011/`.

Temporary kernel counters on the unchanged module explained both negative
results. Of **410 million** code-hazard memo lookups during one complete
10,000-transfer run, **409.20 million (99.81%)** hit the direct hint; about
567,000 found a memo by scanning and 230,000 found none. The diagnostic
workload passed. Logs and dmesg are under ignored
`target/boxctl-evidence/amd-hazard-lookup-diagnostic-1011/`. The extra
diagnostic atomics and logging were removed.

The alias-breakpoint collector had a similar shape. In a passing diagnostic
run, **79.65 million of 80 million calls (99.56%)** reused cached proofs;
about 351,000 walked page tables. A compact hash-tag array was then tested so
set misses could reject unrelated large proof records without reading them.
It passed all 260 VMX tests, the box suite, and all 10,000 transfers. Its fair
interval was **75.552 s** versus **77.617 s** for the immediately following
unchanged control. Earlier unchanged controls on this same box were **76.537
s** and **74.682 s**. The candidate and paired control had nearly identical
VM-entry preparation cycles (103.05 versus 103.23 billion), while guest cycles
and nested-page faults varied substantially. There is no evidence that the
tag lookup caused the timing difference, so it was reverted. Diagnostic logs,
marker snapshots, and the candidate patch are under ignored
`target/boxctl-evidence/amd-alias-tag-1011/`.

### Retain 32 active code write guards

The memo profile above showed that the direct hash hint already works, while
the remaining memo path still checks exact bytes on unguarded pages. Earlier
eight-to-sixteen-guard tests reduced VM-entry preparation. Increasing the
active write-guard limit from 16 to 32, with eight extra slots for very hot
rejected pages, passed 260 VMX tests, 40 VM library tests, the EPYC 4245P box
suite, and two complete Tempo runs. On the same 4245P box, interleaved runs
with identical guest artifacts were:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| 32 guards | 75.631 s | 22,695,514 | 99.02 billion |
| 16 guards | 77.468 s | 22,832,932 | 103.44 billion |
| 32 guards, repeat | 76.312 s | 22,986,531 | 100.49 billion |
| 16 guards, repeat | 76.546 s | 22,343,160 | 103.94 billion |

The 32-guard pair averaged **75.971 s** versus **77.007 s** for the controls,
about **1.3% faster**. Both candidate runs used fewer VM-entry preparation
cycles, by 4.42 and 3.46 billion compared with the following controls.
Nested-page-fault counts varied in both directions, so fewer faults do not
explain the gain. All four runs reported 10,000 successful transfers. Logs,
marker snapshots, and the candidate patch are under ignored
`target/boxctl-evidence/amd-guard32-1011/`.

One disposable box provisioned with an EPYC 4244P, rather than the 4245P used
for those measurements. Both the candidate and unchanged module failed the
existing `svm_bench` control-flow acceleration threshold on that 4244P (about
20,000 exits where the test expects fewer than 100). Its other initial SVM
checks passed. That host was not used for the Tempo comparison; the baseline
failure log is saved with the guard-32 evidence for follow-up.

### Retain 64 active code write guards

Doubling the active guard limit again, from 32 to 64 with eight extra slots
for very hot rejected pages, passed the 260 VMX tests, 40 VM library tests,
the EPYC 4245P box suite, and two full 10,000-transfer Tempo runs. On the
same box used for the 32-guard comparison above, an interleaved A/B/A/B run
measured:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| 64 guards | 73.148 s | 22,487,147 | 93.69 billion |
| 32 guards | 75.524 s | 22,703,628 | 99.29 billion |
| 64 guards, repeat | 74.185 s | 22,316,345 | 98.22 billion |
| 32 guards, repeat | 74.040 s | 22,055,668 | 97.84 billion |

The 64-guard pair averaged **73.667 s** versus **74.782 s** for the 32-guard
controls, about **1.5% faster**. The first pair gained 2.38 s; the second
candidate was 0.15 s slower, so the size of the gain is uncertain. Average
VM-entry preparation fell from 98.57 to 95.96 billion cycles. Both 64-guard
runs passed all transfers, and their startup-to-first-marker intervals were
about two seconds shorter than the adjacent 32-guard runs. One quick native
loop sample showed a transient 50% slowdown; after swapping modules on the
same box, both 32 and 64 guards returned to about 0.3% overhead. Logs,
marker snapshots, and the candidate patch are under ignored
`target/boxctl-evidence/amd-guard64-1011/`. The Tempo interval is still about
**3.1×** the historical Intel 24.087-second result.

### Retain 128 active code write guards

A further increase from 64 to 128 active guards, with eight extra slots for
very hot rejected pages, passed 260 VMX tests, 40 VM library tests, the EPYC
4245P box suite, and two complete 10,000-transfer Tempo runs. The first pair
used the same box as the 64-guard comparison above; the second pair used a
fresh box with the same EPYC 4245P, HWE kernel, and guest artifacts:

| Box / variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| First / 128 guards | 71.893 s | 22,280,600 | 91.60 billion |
| First / 64 guards | 73.813 s | 22,478,708 | 95.95 billion |
| Second / 128 guards | 71.365 s | 22,243,287 | 89.24 billion |
| Second / 64 guards | 74.999 s | 22,956,320 | 96.79 billion |

The 128-guard pair averaged **71.629 s** versus **74.406 s** for the 64-guard
controls, about **3.7% faster**. Both pairs reduced VM-entry preparation
cycles and nested-page faults. All four runs reported 10,000 successful
transfers. A confirmation attempt on the first box was cut short by its
one-hour expiry; it contributed no timing result. Logs, marker snapshots,
and the candidate patch are under ignored
`target/boxctl-evidence/amd-guard128-1011/`. The remaining gap to the
historical Intel 24.087-second interval is about **3×**.

An isolated 192-guard candidate (200 slots for very hot rejected pages)
passed 260 VMX tests, the box suite, and all 10,000 Tempo transfers, but did
not improve the application interval on the same second EPYC 4245P box:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| 192 guards | 71.401 s | 22,329,125 | 89.91 billion |
| 128 guards | 71.390 s | 22,059,410 | 90.50 billion |

The 192-guard cap was reverted. Its patch, logs, and marker snapshots are
under ignored `target/boxctl-evidence/amd-guard192-1011/`.

### Attribute the remaining nested-page faults

A temporary box-only diagnostic counted execution-fault outcomes and the
reason a page could not become globally executable. The EPYC 4245P box passed
the SVM quick suite and a full 10,000-transfer Tempo run. Its instrumented
fair interval was **74.570 s**, with **22,660,292 nested-page faults**; the
added counters make this a diagnostic run rather than a new timing baseline.
At the final 75-million-fault log point, 73,158,342 faults (97.5%) had taken
the scalar execute path. All of those scalar outcomes had a ready, clean
translation gate and were on code pages that failed the global safety proof;
none were attributed to an unready gate, dirty gate, or guarded page table.
The first kernel-log snapshot from the preceding probe was taken after its
first workload marker, which explains why subtracting that log from a later
one disagreed with the marker JSON counters. The diagnostic source was
removed after collecting the evidence. Logs and marker snapshots are under
ignored `target/boxctl-evidence/amd-scalar-reasons-1011/`.

The remaining fast-path target is hazardous-page transition frequency and
the cost of preparing those transitions. In the instrumented fair interval,
VM-entry preparation consumed 95.79 billion aggregate cycles, compared with
10.60 billion cycles in nested-page-fault handlers. The following
transfer-phase host profile shows where the current build spends cycles.

That fresh profile used the uninstrumented 128-guard build and collected 2,478
host `perf` samples at 99 Hz for 25 seconds after marker 1, with zero lost
samples. The full run passed all 10,000 transfers in **72.372 s** between
markers, with **22,560,124 nested-page faults**, **4,448,673 monitor-trap
exits**, and **91.25 billion VM-entry preparation cycles**. Sampling affected
the timing, so the earlier unprofiled pair remains the comparison baseline.
`svm_run_guest` led the sample report at **30.63%**, followed by `__pi_memcpy`
at **4.12%**, `collect_page_breakpoints` at **3.61%**, and
`page_hazards_memo` at **3.23%**. Assembly annotation attributed 27.49% of
samples *within* `svm_run_guest` to the `VMRUN` instruction; XCR0 switching,
debug-register switching, and guest-state save/restore also appeared near
the top. These are sampled shares, not a decomposition of the 72.372 s.
The report, annotation, log, and marker snapshots are under ignored
`target/boxctl-evidence/amd-guard128-perf-1011/`. The profile supports
reducing execute-fault transitions rather than revisiting the small memo
lookup costs.

### Preserve breakpoint masks when admitting a second page

The global planner could reuse a cached single-page alias proof to admit a
predicted or recently visited hazardous page, but it copied only each
breakpoint address. A masked proof then lost its range when combined with the
current page. The planner now carries each proof's address and mask together;
a unit test confirms the resulting batch covers an unsafe entry inside the
mask. This also prevents a combined batch from executing a hazardous entry
with a narrower hardware breakpoint than its cached proof established.

The 260 VMX and 40 VM-library tests, the EPYC 4245P box SVM suite, and four
full Tempo workloads passed. The same box, guest artifacts, and workload were
used in adjacent candidate/control pairs:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Preserve masks | 71.744 s | 22,404,756 | 89.94 billion |
| Unchanged branch | 72.867 s | 22,759,653 | 91.39 billion |
| Preserve masks, repeat | 70.647 s | 21,737,895 | 90.09 billion |
| Unchanged branch, repeat | 70.774 s | 22,151,414 | 88.61 billion |

The candidate had roughly 355,000 and 414,000 fewer nested-page faults in
the two pairs. Its paired intervals were 1.12 s and 0.13 s shorter, so the
timing benefit is small and variable; the mask-preservation correctness fix
is the primary reason to retain it. An attempted wider admission for a
predicted page with five raw hazards was dropped after a focused test showed
its existing singleton proof already occupied all four debug-register slots.
Logs and marker snapshots are under ignored
`target/boxctl-evidence/amd-mask-compose-1011/`.

### Classify current execute-fault transitions by admitted page count

The 128-guard build with preserved breakpoint masks passed three more full
10,000-transfer runs on a disposable EPYC 4245P HWE box. Temporary box-only
probes were removed after collecting their marker snapshots. A first page
sample was discarded for transition analysis because its field labeled
`previous` held the page two steps back. The corrected run sampled every
20,000th scalar execute fault and recorded the immediate predecessor.

Between the corrected run's fair markers, 1,114 sampled scalar faults covered
189 physical pages and 512 `(previous linear page, faulting linear page)`
pairs. The hottest page appeared in 96 samples; the hottest pair in 28.
Only 21 samples returned to the same linear page. Cached singleton alias
proofs required one or two debug-register slots in 942 samples (84.6%),
three in 83, four in 86, and were absent in three. These are periodic samples,
so their frequencies are directional rather than an unbiased distribution.

A separate cumulative counter classified raw nested-page faults by the batch
that had just run. Subtracting the closest logged counters at the transfer
markers gives approximately 22 million faults: 21,947,120 (99.8%) followed
global counted batches. Of those, 4,326,796 followed a one-code-page batch,
4,309,484 followed two pages, and **13,310,840 (60.6%) followed batches
with three or four admitted code pages**. None of the counted transfer-phase
faults targeted a code page already in the batch; they were transitions to
another page. The million-fault logging interval limits the precision of
these marker differences; the independent JSON snapshot recorded 22,414,922
nested-page faults in the same run. The corrected sample, counter snapshots,
and workload logs are under ignored
`target/boxctl-evidence/amd-current-scalar-sample-1011/`.

The current limit is largely page selection under four hardware breakpoint
slots, not failure to enter a multi-page counted batch. A useful next change
must improve which pages are admitted without recreating the expensive alias
proof and permission work that made earlier wider-page variants slower.

### Test smaller global page sets on the current build

The batch classification motivated two isolated planner changes on the same
EPYC 4245P box. A predicted-only candidate omitted the recent-page fallback;
it passed the box SVM suite and all 10,000 transfers, but raised transfer
nested-page faults from **22.39 million to 29.67 million** and the fair
interval from **72.303 s to 83.237 s**. The recent pages are therefore
important to the current acceleration path.

A second candidate retained prediction and the recent-page fallback but
capped a batch at three code pages instead of four. It passed the box suite
and three complete Tempo runs. The local VMX suite had one expected
selection-policy assertion failure until its test was adjusted; with that
assertion updated, all 260 VMX and 40 VM-library tests passed. In run order:

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Four-page control | 72.303 s | 22,390,096 | 92.20 billion |
| Three-page cap | 72.109 s | 22,597,239 | 90.92 billion |
| Four-page control | 72.734 s | 22,569,817 | 92.46 billion |
| Three-page cap | 70.075 s | 22,049,686 | 87.83 billion |
| Four-page control | 71.786 s | 22,277,366 | 90.84 billion |
| Three-page cap | 72.012 s | 22,628,046 | 90.33 billion |

The three paired timing differences were 0.19 s faster, 2.66 s faster, and
0.23 s slower. The large second-run improvement was accompanied by fewer
faults, whereas the other candidate runs had more faults than their controls.
That variation is too large to attribute a reliable gain to the cap, so both
experimental page limits were reverted. Logs and marker snapshots are under
ignored `target/boxctl-evidence/amd-pred-only-1011/`.

### Pair a trusted page with a predicted hazardous successor

A sampled classifier for execute faults after global batches found that 143
of 1,107 samples had a globally trusted current page, no breakpoint slots in
use, and a faulting hazardous target whose cached singleton proof fit the
available slots. This exposed a planner gap: the trusted-page branch never
tried an optional predicted successor. The diagnostic source was removed
after capture; its output is under ignored
`target/boxctl-evidence/amd-fit-headroom-1011/`.

The planner now admits a predicted hazardous successor when its cached code
translation, hazard scan, and single-page alias proof are valid, its boundary
is guarded, and all required breakpoints fit. The globally trusted current
page uses no breakpoint slots; the successor's cached mask supplies the
hardware breakpoints. The combined batch write-guards both pages. A focused
unit test covers the pairing and rejection when the cached alias proof is
invalid. All 261 VMX tests, 40 VM-library tests, and the in-box SVM quick
suite passed. Each full run completed 10,000 transfers with no failures.

| Run order | Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | Paired trusted/successor | 70.751 s | 21,484,965 | 89.25 billion |
| 2 | Unchanged control | 71.131 s | 22,226,493 | 89.70 billion |
| 3 | Paired trusted/successor | 69.137 s | 20,732,642 | 88.24 billion |
| 4 | Unchanged control | 73.676 s | 22,984,012 | 94.03 billion |

The candidate averaged **69.944 s** versus **72.404 s** for the two
same-box controls, a **3.4%** shorter transfer interval and about **1.50
million** fewer nested-page faults per run. The first pair improved by only
0.38 s, so this is a modest gain with run-to-run variation rather than a
precise throughput estimate. It remains about **2.9×** the historical Intel
24.087-second interval. Logs and marker snapshots are under ignored
`target/boxctl-evidence/amd-trusted-pred-1011/`. All module builds, loads,
and application tests ran inside `box-e6354dd8`, not on the local machine.

### Check broader scalar continuation and early breakpoint masking

A fresh 25-second, 99 Hz transfer-phase profile of the pushed trusted-page
candidate collected 2,479 samples with none lost. `svm_run_guest` still led
at **30.97%** of samples; breakpoint collection used **4.19%** and code-hazard
memo lookup **3.92%**. The profiled full transfer took **70.488 s**, with
**21,212,520** nested-page faults and **89.41 billion** VM-entry preparation
cycles. Profiling and full-run evidence are under ignored
`target/boxctl-evidence/amd-multipage-scalar-1011/`.

One candidate extended the existing two-page scalar continuation rule to
global batches with three or four selected pages. It passed 261 VMX tests,
the in-box SVM suite, and all 10,000 transfers. Its **70.530 s** interval
and **21,127,245** nested-page faults compared with an adjacent unprofiled
control at **70.665 s** and **21,275,678** faults. The **0.135 s** difference
is too small relative to run variation to establish a useful speedup; the
change was reverted.

A second candidate eagerly merged unsafe entries within a 16-byte range
into one masked hardware breakpoint and removed the planner's raw-hazard
count cutoff. A focused five-hazard, four-slot test, all 261 VMX tests, 40
VM-library tests, and the in-box SVM suite passed. Both full application
runs completed all 10,000 transfers. Compared with the same adjacent
control, the first candidate run reduced nested-page faults by **1.52
million** but increased the fair interval from **70.665 s to 81.055 s** and
VM-entry preparation from **89.66 to 117.38 billion cycles**. A repeat with
a 25-second host profile took **81.088 s**, with **19,746,998** nested-page
faults and **116.72 billion** preparation cycles. Debug-exception counts
changed little, so extra harmless debug traps do not explain the regression.
The candidate's profile still led with `svm_run_guest` at **33.58%**, followed
by breakpoint collection at **4.71%** and hazard memo lookup at **4.57%**;
it does not isolate a single new function cost. The early masking and raw
count changes were reverted. Logs, marker snapshots, and the profile are
under ignored `target/boxctl-evidence/amd-early-mask-1011/`.

These results show that admitting more pages can reduce execute faults yet
lose more time in preparation and VM running. Future page-selection changes
need full transfer timings and phase-cycle counts, not fault counts alone.

### Check AMD breakpoint-mask MSR swap cost

The SVM entry assembly saves, writes, and restores all four address-mask
MSRs when any page breakpoint has a nonzero mask. A disposable second EPYC
4245P box (`box-af826965`) with the same HWE kernel, guest kernel, initrd,
and Tempo images tested three alternatives. All in-box SVM suites and full
10,000-transfer runs passed.

| Variant | Fair transfer interval | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: |
| Skip zero-mask slots | 69.394 s | 21,353,473 | 89.18 billion |
| Swap enabled DR7 slots | 72.132 s | 21,762,075 | 92.47 billion |
| Swap only masks differing from host | 70.260 s | 21,332,015 | 91.18 billion |
| Unchanged control | 70.471 s | 21,211,125 | 89.48 billion |
| Host/guest comparison plus early masking | 77.276 s | 19,762,837 | 115.38 billion |

Skipping every zero-mask slot is not generally correct: an enabled exact
breakpoint still needs a zero guest mask if the host's MSR is nonzero. The
safe variants either selected enabled slots from guest DR7 or read the host
value and wrote/restored a slot only when it differed. Neither demonstrated
a useful standalone gain. The host/guest comparison recovered part of the
early-masking regression but remained **6.8 s slower** than the same-box
control, despite **1.45 million** fewer nested-page faults. All assembly
and early-masking experiments were reverted.

The saved perf recordings were resolved against reproducibly rebuilt module
build IDs (`e61a9d63` for the unchanged module and `cffac844` for early
masking). In 25-second samples, inclusive `prepare_global` share rose from
**17.77%** to **21.08%**, while `svm_run_guest` self share rose from
**30.97%** to **33.58%**. The samples cover different progress through the
guest workload, so these shares locate candidate costs but cannot apportion
the full-run regression precisely. The dominant measured change is the
**25.9-billion-cycle** increase in VM-entry preparation for the combined
variant. Logs, snapshots, perf recordings, and rebuilt modules are under
ignored `target/boxctl-evidence/amd-mask-msr-1011/`,
`target/boxctl-evidence/amd-multipage-scalar-1011/`, and
`target/boxctl-evidence/amd-early-mask-1011/`.

### Attribute the extra VM-entry preparation

Temporary per-entry RDTSC counters split SVM preparation into instruction
window reading, batch planning, write guarding, paired-page execute setup,
and the remaining pre-entry tail. The counters were exported in the same
marker snapshots used for fair transfer intervals, then removed from the
source. Their extra timestamp reads change timings, so only comparisons
between identically instrumented runs are meaningful.

| Instrumented variant | Batch planning | Full VM-entry preparation | VM runner and guest | Nested-page faults | Fair interval |
| --- | ---: | ---: | ---: | ---: | ---: |
| Pushed baseline | 59.55 billion cycles | 94.71 billion | 160.23 billion | 21,092,240 | 71.512 s |
| Early masking, no slot stop | 82.60 billion | 117.22 billion | 173.45 billion | 19,495,053 | 80.356 s |
| Early masking, stop when four slots full | 61.66 billion | 96.15 billion | 174.51 billion | 19,725,705 | 75.255 s |
| Slot stop plus host/guest mask comparison | 62.68 billion | 97.09 billion | 159.78 billion | 19,411,627 | 71.662 s |

The unnecessary search after all four breakpoint slots filled accounted for
most of the early-mask planning regression. Stopping there removed **20.9
billion** planning cycles. Comparing host and guest mask values before an
MSR write removed most of the remaining VM-run penalty. Yet the combined
approach did not improve the application interval.

The same combined candidate was then built without diagnostic counters and
passed 261 VMX tests, 40 VM-library tests, the in-box SVM suite, and all
10,000 transfers. Against an adjacent unchanged control on the same box it
took **71.329 s versus 70.471 s**. Nested-page faults fell from **21,211,125
to 19,694,026**, but VM-entry preparation rose from **89.48 to 93.33
billion cycles** and VM-run/guest cycles from **160.90 to 161.66 billion**.
The early mask, slot stop, and mask-MSR comparison were all reverted. This
leaves batch planning cost as the next measurable target if widening the
executable page set is attempted again. Snapshots and logs are under ignored
`target/boxctl-evidence/amd-prep-buckets-1011/` and
`target/boxctl-evidence/amd-clean-combined-stop-1011/`. All kernel builds,
loads, and hardware tests ran in the disposable box.

### Test an additional cached successor and the host PMU read

The global batch planner formerly chose the contextual page successor when
available and only fell back to the ordinary successor if the contextual
entry was absent. A candidate tried both distinct predictions before recent
pages, with a focused test showing that the ordinary successor could be
admitted even when absent from the recent-page list. All 261 VMX tests and
the in-box SVM suite passed. Two same-box full Tempo pairs on an EPYC 4245P
completed 10,000 transfers each:

| Run order | Variant | Fair interval | Nested-page faults | VM-entry preparation cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | Two predictions | 70.779 s | 21,350,406 | 92.02 billion |
| 2 | Unchanged | 71.806 s | 21,717,767 | 92.57 billion |
| 3 | Two predictions | 70.823 s | 21,299,652 | 92.09 billion |
| 4 | Unchanged | 69.668 s | 21,043,011 | 89.63 billion |

The first pair favored the candidate and the second favored the control.
Their averages differ by only **0.064 s**, with no consistent fault-count
gain. The predictor change was reverted. An initial box had an EPYC 4244P
and failed the control-flow acceleration assertion on both candidate and
unchanged builds; it was retired. These paired measurements came from a
fresh 4245P box. Logs and snapshots are under ignored
`target/boxctl-evidence/amd-dual-predictor-1011/`.

The pinned host perf event's count is not used for guest retirement, so a
separate candidate removed the hardware `event->pmu->read(event)` call
after counted SVM exits while keeping its active-event check. The box SVM
suite and all 10,000 transfers passed, but the full interval was **71.267 s**
against an adjacent unchanged **70.455 s**. Faults rose from **21,271,457
to 21,739,843**, and preparation rose from **91.20 to 91.86 billion
cycles**. That candidate was also reverted; this run does not support a
PMU-read optimization. Logs and snapshots are under ignored
`target/boxctl-evidence/amd-pmu-read-1011/`.

### Save the host VMCB once per pinned run loop

The SVM assembly issues `VMSAVE` for the host VMCB before every entry. A
candidate cached that save across one VM_RUN loop, which pins the thread to a
single CPU, and reset the cache before each new loop. The in-box SVM suite
passed, including replay and masked-breakpoint cases. Two same-box Tempo
pairs on EPYC 4245P completed all 10,000 transfers:

| Run order | Variant | Fair interval | Nested-page faults | VM exits | VM runner and guest cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Cached host VMSAVE | 70.268 s | 21,194,438 | 28,827,311 | 5,505 |
| 2 | Unchanged | 71.833 s | 21,866,140 | 29,695,406 | 5,495 |
| 3 | Cached host VMSAVE | 70.373 s | 21,416,693 | 29,126,902 | 5,486 |
| 4 | Unchanged | 70.127 s | 21,239,543 | 28,935,062 | 5,489 |

The first pair favored the candidate, but the second favored the control.
The runner/guest cycles per exit show no consistent improvement. Most of the
first pair's interval difference coincided with a lower exit count. The
candidate was reverted. Logs and snapshots are under ignored
`target/boxctl-evidence/amd-hostsave-1011/`; no module was loaded locally.

### Index the hot code-proof and translation caches

The 64-entry code-proof and translation caches scanned linearly even for
repeated pages. A direct, one-based index now points to a likely entry; the
lookup checks the active count and full page key, then falls back to the
existing scan on a miss. Stale indices after eviction or proof invalidation
therefore cannot return an old proof. The candidate passed all 261 VMX unit
tests and the in-box SVM suite. Two alternating same-box Tempo pairs on an
EPYC 4245P completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | Nested-page faults | VM-entry preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Indexed caches | 68.781 s | 28,289,760 | 20,801,078 | 3,052 |
| 2 | Unchanged | 69.916 s | 28,569,353 | 21,004,004 | 3,142 |
| 3 | Indexed caches | 69.496 s | 28,518,609 | 20,982,084 | 3,063 |
| 4 | Unchanged | 69.801 s | 28,578,145 | 20,997,632 | 3,106 |

Both pairs favored the index. Mean fair interval fell from **69.858 to
69.138 s** (about **1.0%**), and preparation cycles per exit fell in both
pairs. The second pair had nearly equal exit counts, supporting a real
planning-cost reduction. This is a modest gain; the remaining ~3x gap to the
earlier Intel result still requires a larger reduction in VM exits or entry
work. Logs and snapshots are under ignored
`target/boxctl-evidence/amd-indexed-cache-1011/`.

### Read only the instruction prefix until the fallback planner needs more

`prepare_instruction_exit` previously read up to 256 guest code bytes before
trying a global counted batch, even though that path checks only the first
instruction. It now reads at most 32 bytes initially and fills the remainder
of the same page only before the fallback batch planner. A focused unit test
checks both the full fill and a fetch near the page edge. All 262 VMX unit
tests and the in-box SVM suite passed. Two more alternating same-box Tempo
pairs on the indexed-cache build completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | Nested-page faults | VM-entry preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Short first read | 69.662 s | 29,000,685 | 21,354,914 | 3,000 |
| 2 | Indexed-cache baseline | 70.723 s | 29,141,536 | 21,453,444 | 3,048 |
| 3 | Short first read | 70.430 s | 29,236,847 | 21,508,571 | 2,994 |
| 4 | Indexed-cache baseline | 70.759 s | 29,083,460 | 21,377,471 | 3,074 |

Both pairs favored the short read. Mean fair interval fell from **70.741 to
70.046 s** (about **1.0%**), while preparation cycles per exit fell in both
pairs. The second candidate had more exits yet still used fewer total entry
preparation cycles. Logs and snapshots are under ignored
`target/boxctl-evidence/amd-short-window-1011/`.

### Profile the current entry path and test a PMU write shortcut

After the indexed caches and short first read, a 25-second, 99 Hz host
`perf` sample captured 2,479 samples from the actual `bedrock-cli` process
during the fair transfer interval, with zero lost samples. That profiled run
completed all 10,000 transfers in **69.419 s**. `svm_run_guest` remained the
largest symbol at **32.54%** of samples, followed by breakpoint collection
at **3.81%**, `__pi_memcpy` at **3.11%**, and code-hazard memo lookup at
**2.78%**. Within the SVM assembly, `VMRUN` held 26.94% of samples,
PMU-control reads/writes were visible, and XCR0, debug-register, and guest
FPU-state switching remained substantial. These are sampled shares, not
separate contributions to wall time. The report, annotation, and snapshots
are under ignored `target/boxctl-evidence/amd-current-hot-1011/`.

A candidate skipped the pre-`VMRUN` `IA32_PERF_GLOBAL_CTRL` write when its
requested value already matched the saved host value, while retaining the
post-exit host restore. It passed the in-box SVM suite and completed three
full Tempo runs, including one on the earlier box. Two alternating runs on a
fresh EPYC 4245P box gave:

| Run order | Variant | Fair interval | VM exits | VM runner and guest cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | Conditional entry write | 69.983 s | 29,141,800 | 5,534 |
| 2 | Unchanged | 69.294 s | 28,668,206 | 5,578 |
| 3 | Conditional entry write | 68.555 s | 28,619,524 | 5,566 |
| 4 | Unchanged | 70.305 s | 29,044,184 | 5,560 |

The candidate averaged **69.269 s** versus **69.799 s**, but each pair's
faster run also had fewer exits, and runner cycles per exit did not improve
consistently. A host `IA32_PERF_GLOBAL_CTRL` read on CPU 1 after the control
test returned `0x3f`; the requested counted-guest mask contains only the one
perf-owned counter bit, so the shortcut cannot fire while the host value is
`0x3f`. The candidate was reverted. Logs and snapshots are under ignored
`target/boxctl-evidence/amd-pmu-entry-1011/`. The assembly path still needs
a larger architectural reduction in per-exit switching or in the number of
execute-fault transitions.

### Let PMC virtualization load the VMCB counter controls

[AMD APM Volume 2, section 15.39](https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2)
defines the guest PMC controls in the VMCB when PMC virtualization is
enabled. The host assembly had additionally read the host global-control MSR,
written a guest enable mask before every entry, and restored the host mask
after every exit. The new path leaves the guest controls to the VMCB. For an
uncounted entry, it clears the previous guest PMC event and global enable.
Assembly still reads the host control before and after `VMRUN` and restores
it if hardware or another component changed it. The fallback when PMC
virtualization is unavailable is unchanged.

The 21 SVM-library tests, including a new guest-counter disarm test, and the
in-box SVM suite passed. Two alternating same-box EPYC 4245P Tempo pairs
completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | VM runner and guest cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | VMCB PMC controls | 68.629 s | 29,074,955 | 5,355 |
| 2 | Unchanged | 69.812 s | 29,025,265 | 5,560 |
| 3 | VMCB PMC controls | 67.633 s | 28,618,546 | 5,361 |
| 4 | Unchanged | 69.747 s | 28,841,826 | 5,569 |

Mean fair interval fell from **69.779 to 68.131 s** (about **2.4%**).
Runner cycles per exit fell by 205–208 in both pairs, including the first
pair where the candidate had slightly more exits. A separate 25-second host
profile with 2,478 samples completed the full workload in **68.025 s**;
its assembly annotation attributed no samples to the conditional host
restore `WRMSR`. The host global-control register read `0x3f` after that
run. Sampling cannot prove the restore never executes, so the conditional
restore remains. Logs, snapshots, perf data, and annotation are under ignored
`target/boxctl-evidence/amd-pmu-autoswap-1011/`.

### Keep PMC virtualization enabled on uncounted entries

The VMCB can hold a disarmed guest PMC state while PMC virtualization stays
enabled. We now enable it on uncounted entries, clear both guest PMC controls,
and restore the previous VMCB virtualization flags after exit. Counted entries
still use `PmcEntry::prepare` and `finish`. This lets SVM handle the host/guest
PMC switch without the assembly's two host-global-control reads and conditional
restore. On the test EPYC 4245P, host `PERF_GLOBAL_CTRL` was `0x3f` before and
after each run. The in-box SVM suite and 21 `bedrock-svm` tests passed.

Two same-box pairs, before removing the now-unreachable assembly branch,
completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | VM runner and guest cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | PMC virtualization throughout | 66.736 s | 29,390,087 | 5,081 |
| 2 | Conditional host check | 67.608 s | 28,669,501 | 5,361 |
| 3 | PMC virtualization throughout | 65.972 s | 28,768,541 | 5,065 |
| 4 | Conditional host check | 67.444 s | 28,742,830 | 5,342 |

Mean fair interval fell from **67.526 to 66.354 s** (**1.7%**). Runner and
guest cycles per exit fell by 277–280 in both pairs; the candidate even had
more exits in the first pair. Full run snapshots are under ignored
`target/boxctl-evidence/amd-pmu-pure-fixed-1011/`.

The final assembly removes that dead PMU-check branch and its unused ABI
argument. Its in-box SVM suite passed, and one full Tempo run completed in
**65.194 s** with **28,355,171** exits and **5,090** runner/guest cycles per
exit. The host global control remained `0x3f` after the run. This single
final-code sample is consistent with the two earlier candidate samples; the
lower wall time also reflects fewer exits, so the two-pair comparison above
is the stronger estimate of the change's effect.

### Profile and test VMCB access costs

A fresh 25-second, 99 Hz profile of the pushed PMC-virtualization build
captured 2,479 samples with none lost. `svm_run_guest` took 28.13% of
self samples, `collect_page_breakpoints` 4.16%, `page_hazards_memo` 3.75%,
`Vmcb::read` 2.51%, and `fields::location` 1.78%. The profiled workload's
fair interval was 67.254 s. This motivated two isolated VMCB-access tests.

First, `#[inline]` on the generic VMCB and field accessors passed the SVM
suite but regressed the otherwise identical profiled interval to 70.082 s.
The sample share moved into `RealVmcs::vmread` and `fields::location`; the
inlining change was reverted. Next, width-specific unaligned VMCB loads and
stores passed the unit and box suites but regressed an unprofiled same-box
full run to **69.045 s versus 65.920 s** for the unchanged code. Exit counts
were nearly equal (28,954,951 versus 28,977,097), while entry-preparation
cycles rose from **85.97 to 95.22 billion**. That change was also reverted.
The comparison markers are under ignored
`target/boxctl-evidence/amd-vmcb-access-1011/`.

A narrower follow-up resolved the five guest fields used by the global-batch
planner directly in the SVM VMCS adapter, leaving the general mapping and VMCB
byte access unchanged. It passed 22 SVM-library tests and the in-box SVM
suite. Two candidates and two unchanged controls on the same EPYC 4245P box
completed the full transfer workload:

| Run order | Variant | Fair interval | VM exits | Entry-preparation cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | Unchanged | 65.920 s | 28,977,097 | 2,967 |
| 2 | Direct fields | 65.591 s | 29,071,818 | 2,902 |
| 3 | Direct fields | 65.538 s | 29,303,952 | 2,795 |
| 4 | Unchanged | 66.498 s | 29,061,873 | 3,014 |

The candidate mean was **65.564 s** versus **66.209 s** unchanged (about
**1.0%** faster). It had more exits in both adjacent comparisons yet lower
preparation cycles per exit. Logs and markers are under ignored
`target/boxctl-evidence/amd-field-fastpath-1011/`.

### Recheck entry assembly and exit-path diagnostics

An updated 25-second host sample of the AMD transfer interval collected
2,479 samples with no loss. The `svm_run_guest` annotation attributed 35.01%
of its samples to `VMRUN`, 7.60% to the two `XSETBV` instructions, and visible
shares to debug-register swaps, `VMLOAD`, and guest `XSAVEOPT`. The annotation
is under ignored `target/boxctl-evidence/amd-vmcb-access-1011/`. Earlier
isolated tests already found that host/guest XCR0 differs on this EPYC host
and that caching the host `VMSAVE` did not speed up the application interval.

The compiled batch planner also still contained `lock inc` instructions for
39 diagnostic counters that nothing reads. Removing them reduced the module's
static `lock` instruction count from 128 to 26 and passed 262 VMX tests and
the in-box SVM suite. It did **not** speed up the application: a baseline run
took **64.991 s** with 29,073,238 exits, while two counter-free runs took
**64.988 and 65.551 s** with 28,549,237 and 29,066,737 exits. Both candidates
spent more VM-entry preparation cycles despite the removed instructions. The
diagnostic-counter removal was reverted. Marker snapshots are under ignored
`target/boxctl-evidence/amd-unread-counters-1011/`.

### Record normalized AMD exits in the software VMCB area directly

Each SVM exit mirrored eight normalized VMCS fields through the generic
VMCS-to-VMCB mapper. Those writes target only the VMCB's software area, but
the generic path also cleared hardware clean bits after every field. The new
path writes the same values at their fixed software offsets without touching
clean bits. A focused test checks all eight field values and unchanged clean
bits; 23 SVM-library tests and the in-box SVM suite passed.

Two same-box EPYC 4245P pairs completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | VM-exit bookkeeping cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | Generic exit recording | 64.991 s | 29,073,238 | 12.08 billion |
| 2 | Direct software writes | 63.698 s | 29,255,646 | 12.04 billion |
| 3 | Generic exit recording | 66.224 s | 29,335,438 | 12.39 billion |
| 4 | Direct software writes | 64.593 s | 29,393,131 | 12.06 billion |

Mean fair interval fell from **65.607 to 64.145 s**, a **2.2%** gain. Both
candidate runs had more exits than their adjacent controls, so the gain is
not explained by a lower exit count. The counters bucket only part of the
exit-recording work; the full interval is the outcome measure. Marker
snapshots are under ignored `target/boxctl-evidence/amd-exit-record-1011/`.

### Build AMD global batches in the caller's slot

The post-exit-recording profile sampled `__pi_memcpy` at 4.85% self time and
showed a large batch copy on return from `prepare_global`. The planner now
constructs its batch directly in the caller's slot. Its compatibility wrapper
still returns a batch for the planner tests. The candidate module's planner
disassembly no longer has that return-path `memcpy`. All 262 `bedrock-vmx`
library tests and the in-box SVM suite passed.

Two same-box EPYC 4245P pairs completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | VM-entry preparation cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | In-place batch | 63.803 s | 29,446,956 | 78.90 billion |
| 2 | Return-and-copy control | 63.475 s | 28,808,143 | 81.72 billion |
| 3 | In-place batch | 62.044 s | 28,459,289 | 77.48 billion |
| 4 | Return-and-copy control | 63.550 s | 29,212,376 | 80.29 billion |

Mean fair interval fell from **63.512 to 62.923 s**, a **0.9%** gain. Mean
VM-entry preparation cycles fell from **81.00 to 78.19 billion**, about
**3.5%**, while total exits across each variant differed by only 0.2%.
The first pair did not improve wall time, so the measured benefit is modest
and subject to workload variation. Marker snapshots are under ignored
`target/boxctl-evidence/amd-batch-into-1011/`.

### Recheck TLB flush and profile the in-place batch build

A fresh 25-second, 99 Hz host profile of the pushed in-place batch build
collected 2,479 samples with none lost while the full 10,000-transfer Tempo
run completed in **64.081 s**. `svm_run_guest` had 30.17% self samples,
breakpoint collection 4.61%, `__pi_memcpy` 4.33%, code-hazard memo lookup
4.12%, and `prepare_global_into` 2.66%. The profile and marker snapshots
are under ignored `target/boxctl-evidence/amd-after-batch-1011/`.

The SVM backend currently requests a TLB flush on every VMRUN.
[AMD's APM Volume 2, section 15.16.1](https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2)
says VMRUN reads but does not clear the VMCB `TLB_CONTROL` field, so this is
a persistent cost. A disposable diagnostic build set that field to zero to
measure whether it could simply be removed. The first mixed-memory box test
failed: the guest computed the expected checksum but a guest memory word
read back as zero. The execution guard changes NPT permissions and mappings
across entries; omitting the invalidation is incorrect. The diagnostic edit
remained in an isolated worktree and was never pushed.

### Borrow the instruction batch at VM entry

The run loop previously copied its `InstructionBatch` into the SVM runner on
every entry, even though the batch remained live on the run-loop stack and
was used again after the exit. `VmRunner::run` now borrows the batch for that
single entry, and the real runner no longer stores a copy. All 262 VMX
library tests and the in-box SVM suite passed, including COW and replay.

Two same-box EPYC 4245P pairs completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | VM-entry preparation cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | Borrowed batch | 60.988 s | 28,378,786 | 74.21 billion |
| 2 | Copied-batch control | 63.765 s | 29,467,016 | 79.31 billion |
| 3 | Borrowed batch | 63.411 s | 29,269,324 | 78.27 billion |
| 4 | Copied-batch control | 62.852 s | 28,919,223 | 78.48 billion |

Mean fair interval fell from **63.308 to 62.199 s** (about **1.75%**).
The candidate had **1.26%** fewer exits overall, which explains part of
the wall-time gain; the second pair favored the control on wall time.
Across both pairs, preparation cycles per exit fell from **2,703 to 2,645**
(about **2.13%**), supporting a real reduction in per-entry work. Marker
snapshots are under ignored `target/boxctl-evidence/amd-borrowed-batch-1011/`.

### Skip guarded leaf tables without selected code aliases

The post-borrowing 25-second host profile completed the workload in
**62.005 s** with 2,479 samples and no loss. `__pi_memcpy` fell from 4.33%
before the borrowed-batch change to 2.87% self samples. Breakpoint
collection remained 4.41%, code-hazard memo lookup 3.66%, and
`svm_run_guest` 31.78%. An isolated diagnostic of the breakpoint collector
showed over 99% proof-cache hits during the transfer phase. The remaining
full alias walks almost all succeeded; the breakpoint-limit failure counter
stayed at 41 across the entire run. Caching negative results would therefore
miss the hot case. The profile and diagnostic counters are under ignored
`target/boxctl-evidence/amd-after-borrow-1011/` and
`target/boxctl-evidence/amd-alias-walk-diag-1011/`.

The alias walker now caches a small Bloom filter of executable guest-physical
pages for each leaf translation table while that table is NPT write-guarded.
If no selected code page's bits are present, the walker skips the leaf's
512-entry scan. A positive filter result still takes the original exact
walk, so collisions only cost time. Full tree rebuilds and rearming after
page-table writes clear the filters; a dirty gate, an NPT mapping-generation
mismatch, or untrusted table links also disable their use. A focused unit test
populates a filter, then verifies host and guest page-table changes are
observed. All 262 VMX library tests and the in-box SVM suite passed.

Two same-box EPYC 4245P pairs completed all 10,000 transfers:

| Run order | Variant | Fair interval | VM exits | VM-entry preparation cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | Guarded leaf filter | 60.169 s | 28,589,856 | 69.62 billion |
| 2 | Unchanged | 63.774 s | 29,226,028 | 80.04 billion |
| 3 | Guarded leaf filter | 61.098 s | 29,213,439 | 69.69 billion |
| 4 | Unchanged | 63.694 s | 29,480,251 | 78.26 billion |

Mean fair interval fell from **63.734 to 60.633 s**, a **4.9%** gain.
Candidate runs had **1.5%** fewer exits overall, but preparation cycles
per exit also fell from **2,696 to 2,410** (about **10.6%**). Both pairs
favored the filter on wall time and normalized preparation cost. Marker
snapshots are under ignored `target/boxctl-evidence/amd-alias-bloom-1011/`.

### Test a hazard-only leaf-filter query

A follow-up queried the leaf filter only for selected pages with hazardous
instruction offsets and precomputed those filter bits once per alias walk.
All 262 VMX tests, the box SVM suite, and the full 10,000-transfer workload
passed. On the same EPYC 4245P box, the candidate took **60.599 s** with
**28,910,267** exits and **69.33 billion** preparation cycles; the unchanged
control took **60.429 s** with **28,896,504** exits and **69.13 billion**
preparation cycles. Exit counts were nearly equal and neither wall time nor
entry cost improved, so the change was reverted. Marker snapshots are under
ignored `target/boxctl-evidence/amd-hazard-bloom-1011/`.

### Widen the guarded leaf filter to 256 bits

A separate candidate used four 64-bit words per guarded leaf table, reducing
false positives at the cost of 24 KiB more guard-state storage. All 262 VMX
tests, the in-box SVM suite, and three full 10,000-transfer Tempo runs passed.
Same-box alternating measurements were:

| Run order | Variant | Fair interval | VM exits | VM-entry preparation cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | 256-bit filter | 60.532 s | 29,153,444 | 68.25 billion |
| 2 | 64-bit control | 61.160 s | 29,124,285 | 69.35 billion |
| 3 | 256-bit filter | 59.330 s | 28,332,279 | 67.60 billion |
| 4 | 64-bit control | 60.742 s | 28,999,410 | 69.26 billion |
| 5 | 256-bit filter | 60.277 s | 28,812,256 | 68.69 billion |
| 6 | 64-bit control | 60.292 s | 28,663,792 | 69.27 billion |

The candidate averaged **60.046 s** against **60.731 s** for the control,
about **1.1%** faster. Preparation cycles per exit were lower in each pair,
but the third wall-time pair was effectively tied. A subsequent review added
a direct-query fallback for unusually large selected-page batches without
changing the measured common path. Marker snapshots are under ignored
`target/boxctl-evidence/amd-bloom256-1011/`.

### Avoid rebuilding a valid guarded leaf filter

An isolated candidate skipped hashing leaf entries when a valid filter
already existed or the leaf was not eligible for caching. All 262 VMX tests,
the in-box SVM suite, and two full Tempo runs passed. The same-box pairs were:

| Run order | Variant | Fair interval | VM exits | VM-entry preparation cycles |
| --- | --- | ---: | ---: | ---: |
| 1 | Skip rebuild | 60.879 s | 29,181,321 | 68.79 billion |
| 2 | Unchanged | 60.225 s | 28,655,296 | 68.92 billion |
| 3 | Skip rebuild | 61.236 s | 29,216,526 | 69.99 billion |
| 4 | Unchanged | 60.587 s | 28,460,876 | 69.16 billion |

The candidate had about **1.7%** fewer preparation cycles per exit in
aggregate but **2.2%** more exits and was **0.65 s slower** in each pair.
Because it did not improve elapsed time, this change remains unmerged.
Marker snapshots are under ignored `target/boxctl-evidence/amd-lazy-bloom-1011/`.

### Classify the remaining nested-page faults

An isolated diagnostic build counted fault outcomes, global-safety proof
reasons, hazard counts, and recent physical fault-page reuse. It passed all
262 VMX tests, the in-box SVM suite, and three full Tempo runs. The diagnostic
code was kept out of the shared branch because its atomic counters and page
history scans perturb timing.

In the third run, subtracting the nearest million-fault log snapshots around
the fair markers gives approximately **21.0 million** nested-page faults:
**20.91 million** scalar execute faults, about **66 thousand** guarded
page-table writes, and **20 thousand** MMIO faults. Every scalar fault in
that window had a ready, clean translation gate. The global code proof found
at least one hazardous entry on **20.89 million** of those pages; only about
**22 thousand** lacked a bounded hazard proof. This confirms the earlier
scalar-path diagnosis on the current 256-bit-filter branch.

Among those scalar faults, roughly **11.05 million (53%)** involved a page
with one hazardous entry and **6.76 million (32%)** involved two; **2.57
million (12%)** involved three or four. The recent-fault history found about
**60%** of scalar faults revisited a physical page seen within the preceding
eight scalar faults, while about **20%** missed a 32-page history. These are
reuse distances among *faulting* pages, not the full instruction stream or a
measure of how many breakpoint slots an executable batch needs. The existing
planner already admits three or four code pages before most transfer-phase
faults, so this observation alone does not justify another recency predictor.
The log and marker snapshots are under ignored
`target/boxctl-evidence/amd-npfdiag-1011/`.

### Test 2 KiB AMD breakpoint masks

An isolated candidate allowed the four debug-register masks to cover up to
2 KiB instead of 1 KiB when merging hazardous entries. All 262 VMX tests,
the in-box SVM suite, and the full Tempo workload passed. One same-box pair
gave **60.583 s** for the wider mask versus **60.419 s** unchanged. The
candidate had **21,497,437** nested-page faults and **49,003** debug
exceptions, versus **21,402,839** faults and **44,767** debug exceptions
for the control. It also spent slightly more VM-entry preparation cycles
(69.19 versus 69.05 billion). With no reduction in either exit class or
elapsed time, the 2 KiB mask was not merged. Marker snapshots are under
ignored `target/boxctl-evidence/amd-mask2k-1011/`.

### Use the guarded-table index in the execute-fault handler

Every execute fault checked whether its physical page belonged to the
write-guarded translation tree, but the fault handler scanned up to 1,024
table pages linearly. The global planner already maintains an exact hash
index for that guarded set. The fault handler now calls the same indexed
membership helper; it retains the helper's linear fallback when the index is
invalid or unexpectedly full. Existing collision and fallback tests plus
all 262 VMX tests and the in-box SVM suite passed.

Two alternating same-box EPYC 4245P pairs completed all 10,000 transfers.
The first unchanged run was also the control for the preceding mask test:

| Run order | Variant | Fair interval | VM exits | Nested-page faults | Fault-handler cycles/fault |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Unchanged | 60.419 s | 29,043,387 | 21,402,839 | 321 |
| 2 | Indexed fault path | 59.337 s | 28,745,087 | 21,154,513 | 169 |
| 3 | Indexed fault path | 60.353 s | 29,328,405 | 21,602,738 | 176 |
| 4 | Unchanged | 61.662 s | 29,113,163 | 21,370,696 | 470 |

Mean fair interval fell from **61.040 to 59.845 s**, about **2.0%**. The
second candidate had more exits than its adjacent control yet finished
faster; its normalized nested-page-fault handling cost remained much lower.
The two controls differed in handler cost, so the table gives a range rather
than a single fixed cycles-per-fault saving. Marker snapshots are under
ignored `target/boxctl-evidence/amd-npf-index-1011/`.

### Profile the indexed fault path

A fresh 25-second, 99 Hz host profile began at the first Tempo transfer
marker on the pushed indexed build. It captured **2,478 samples** with none
lost; the full 10,000-transfer run passed in **60.216 s**, with **21.49
million** nested-page faults and **178 handler cycles per fault**. The
largest self-sample shares were `svm_run_guest` **32.30%**, the kernel run
loop **8.36%**, `page_hazards_memo` **4.44%**, `prepare_global_into`
**3.40%**, `collect_page_breakpoints` **3.23%**, and `__pi_memcpy` **2.01%**.
Assembly annotation attributed 37.44% of the `svm_run_guest` samples to
`VMRUN`; XCR0 changes and debug-register swaps remained visible. These are
samples, not additive wall-time costs. The perf data, reports, annotation,
and marker snapshots are under ignored
`target/boxctl-evidence/amd-after-npf-index-1011/`.

The hazard-memo annotation sampled mostly inside the existing exact 4 KiB
byte comparison. A pinned, native same-CPU microbenchmark of unchanged
pages measured **62 ns** per page for the current eight-word comparison
loop and **292 ns** for `REP CMPSQ`, so that replacement was not pursued.
The global planner annotation also showed repeated `REP MOVSQ` copies;
the cached singleton alias proof was returned by value, motivating a
separate borrowed-proof experiment.

### Borrow the cached singleton alias proof

The global planner used a cached singleton alias proof only to read its
breakpoint addresses and masks, but its lookup returned the entire proof by
value. It now borrows the validated cache entry while composing the batch.
All 262 VMX tests and the in-box SVM suite passed, including masked
breakpoints and fork replay. A same-box EPYC 4245P pair completed all
10,000 Tempo transfers:

| Variant | Fair interval | VM exits | Nested-page faults | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| Borrowed proof | 57.718 s | 28,496,984 | 20,947,001 | 65.68 billion |
| Unchanged control | 59.220 s | 28,743,412 | 21,129,477 | 69.88 billion |

The candidate was **1.50 s faster** with **0.9%** fewer exits. Preparation
cycles per exit also fell from about **2,431 to 2,305**, a **5.2%** reduction
that supports less per-entry copying. Marker snapshots are under ignored
`target/boxctl-evidence/amd-borrow-alias-1011/`.

Two more alternating candidate/control runs on a fresh EPYC 4245P box also
completed all 10,000 transfers. Both variants passed the in-box SVM suite
before their full runs:

| Run order | Variant | Fair interval | VM exits | Nested-page faults | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Borrowed proof | 59.027 s | 28,744,089 | 21,132,908 | 2,363 |
| 2 | Unchanged control | 58.358 s | 28,047,210 | 20,609,370 | 2,463 |
| 3 | Borrowed proof | 60.511 s | 29,425,971 | 21,682,505 | 2,360 |
| 4 | Unchanged control | 62.391 s | 30,018,006 | 22,118,038 | 2,451 |

The two candidate intervals averaged **59.769 s** versus **60.374 s** for
the controls, about **1.0%** faster. One pair was slower because the candidate
had 2.5% more exits; the other was 1.88 s faster. The preparation cost per
exit fell consistently by about **3.7–4.1%** on the fresh box, so the local
copy reduction is repeatable while its end-to-end benefit is modest relative
to run-to-run variation. Marker snapshots are under ignored
`target/boxctl-evidence/amd-borrow-alias-repeat-1011/`.

### Classify the remaining hazardous execute faults

An isolated diagnostic on the same disposable EPYC 4245P box classified the
page hazards responsible for scalar execute faults, then measured the
faulting fetch address against the page's recorded hazardous entries. Both
diagnostic builds passed all 262 VMX tests and the in-box SVM suite, and both
completed the full 10,000-transfer Tempo command. The counters below subtract
the nearest million-fault kernel-log snapshots to the fair markers, so they
approximate rather than exactly match the marker-bounded exit statistics.

The hazard-type run recorded about **20.88 million** classified scalar faults
between snapshots around its **21.09 million** marker-bounded nested-page
faults. About **14.01 million (67%)** were on pages with REP-string entries,
**1.74 million (8%)** on pages with SYSRET entries, and **0.76 million (4%)**
on pages with RDRAND/RDSEED/RDPID entries. Another **0.10 million** had both
REP and SYSRET entries, and **4.28 million (20%)** had entries the lightweight
classifier did not identify, including cross-page suffix hazards. These
categories describe *pages containing* opcodes, not instructions executed.

The second run compared the faulting guest-physical fetch offset with all
recorded hazardous offsets on that page. Between snapshots near the fair
markers, **0 of 20,897,854** scalar faults matched an exact hazardous offset,
**0** fell within 15 bytes, and all **20,897,854** were elsewhere on the
page. The marker-bounded run had **21,361,326** nested-page faults and a
**63.309 s** fair interval; diagnostic reads and logging make its elapsed
time unsuitable as a performance baseline. This confirms that most execute
faults occur when entering a page *containing* a possible hazard rather than
when reaching that entry. It does not prove those hazards unreachable: the
hardware breakpoint still has to stop an actual REP, SYSRET, or RNG entry.
The principal remaining optimization is a safe way to keep more such pages
executable with effective instruction-entry traps; four AMD debug-register
slots currently limit the admitted set. Diagnostic source is isolated in
`/tmp/bedrock-amd-npfdiag-1011`; copied logs and markers are under ignored
`target/boxctl-evidence/amd-hazard-types-1011/` and
`target/boxctl-evidence/amd-hazard-rip-1011/`.

### Retain more recurrent code write guards

The hazard memo has 256 slots, but the recurrent-code path formerly stopped
retaining NPT write guards after 128 ordinary hazardous pages (136 for
rejected pages). Raising those limits to **192/200** leaves at least 56 memo
slots replaceable while allowing more hot pages to reuse their exact-byte
proof without another 4 KiB comparison. The stale-guard replacement test was
expanded to the new limit. All 262 VMX tests and the in-box SVM suite passed,
including replay, masked breakpoints, and stale-code rechecking.

Three alternating same-box EPYC 4245P pairs completed all 10,000 transfers:

| Run order | Guard cap | Fair interval | VM exits | Nested-page faults | Preparation cycles/exit |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 192 | 60.480 s | 29,824,758 | 21,985,832 | 2,278 |
| 2 | 128 | 58.760 s | 28,641,135 | 21,052,332 | 2,344 |
| 3 | 192 | 58.881 s | 29,193,902 | 21,493,189 | 2,259 |
| 4 | 128 | 59.962 s | 29,197,930 | 21,472,434 | 2,356 |
| 5 | 192 | 58.599 s | 28,908,893 | 21,273,546 | 2,278 |
| 6 | 128 | 61.475 s | 29,686,793 | 21,843,482 | 2,411 |

The larger cap averaged **59.320 s** versus **60.066 s** for the control,
about **1.2%** faster. Preparation cost per exit fell in every pair, by
roughly **3–6%**. The first candidate had 1.18 million more exits and was
1.72 s slower; the second had nearly identical exit counts and was 1.08 s
faster. This is a repeatable per-entry reduction with a modest, noisy
end-to-end benefit. Marker snapshots are under ignored
`target/boxctl-evidence/amd-guard192-1011/`.

### Verify that AMD PMU counting cannot replace REP entry traps

A focused hardware probe starts with 100 NOPs, then a 5,000-iteration
`REP STOSB`, and requests an exact stop at virtual instruction count 1,000.
With the normal code-page guard, the HWE 7.0.0-38 EPYC 4245P box stopped
at the REP RIP with RCX 4,100 and RDI advanced by exactly 900 bytes; the
full in-box SVM suite passed. The probe is now part of `svm_transitions`.

In an **isolated, unmerged** variant, the page scanner omitted only interior
REP entries from its hazard list, allowing the same code page to run under
the global retired-instruction counter. The probe instead reached shutdown
with all 5,000 iterations complete at virtual count **103** and failed its
deadline assertion. The broader `svm_bench` suite also failed an instruction
count assertion, reporting 18 where its reference expected 16. The safe
module was reloaded afterward and passed the complete in-box SVM suite,
including the new probe. This rules out merely trusting REP-containing pages
under the existing AMD PMU path: the counter accounts for the whole REP as
one retired instruction in this experiment, so it cannot stop a long REP
at the requested iteration. The unsafe source remains isolated in
`/tmp/bedrock-amd-rep-pmu-probe-1011`; it was not merged.

An additional isolated 4245P probe logged the VMCB counters as the unguarded
5,000-iteration `REP STOSB` finished. The exit after REP had `IRPERF=3`,
`PERF_GLOBAL_STATUS=0`, and a non-overflowed virtual PMC; the guest reached
shutdown at virtual count **103** rather than the requested 1,000 deadline.
The unchanged module was reloaded afterward and passed the box suite. A
native user-only `perf` comparison with and without a 10-million-byte REP
also showed the `0xC0` retired-instruction event did not rise by the byte
count. Event `0xC1` retired macro-ops rose substantially but not one-for-one
with bytes. These checks reinforce that changing the retired event alone
cannot supply an exact stop inside REP on this host.

### Sample the reachable-code alternative

An isolated diagnostic called the existing bounded single-page reachable-code
proof for one in every 2,048 hazardous global-plan entries. It restored the
batch breakpoint fields and proof-cache slot afterward, leaving the selected
execution plan unchanged. All 262 VMX tests, the in-box SVM suite, and the
complete 10,000-transfer workload passed. The fair interval was **59.229 s**
with **21.60 million** nested-page faults.

Subtracting the nearest diagnostic log snapshots around the fair markers
gave **11,264** sampled plans: **101 (0.9%)** needed no breakpoints, **5,475
(48.6%)** needed one, **1,573 (14.0%)** needed two, **819 (7.3%)** needed
three, **742 (6.6%)** needed four, and **2,554 (22.7%)** could not be proved
within the bounded analysis. The snapshots cover slightly less than the
marker interval, and this diagnostic evaluates a single page from its current
entry rather than a multi-page graph. Still, the tiny zero-breakpoint share
does not support a general strategy of making hazardous pages free to execute
by replacing the whole-page scan with this proof. The diagnostic remains
isolated in `/tmp/bedrock-amd-reachdiag-1011`; logs and markers are under
ignored `target/boxctl-evidence/amd-reachdiag-1012/`.

### Rotate AMD ASIDs instead of flushing the TLB on every VM entry

The VMCB previously kept `TLB_CONTROL=3`, requesting an ASID flush on every
entry. The HWE 7.0.0-38 EPYC 4245P box now assigns a fresh per-CPU guest ASID
for each entry and requests a full flush only when the ASID range wraps. This
preserves isolation from stale nested-page permissions without flushing on
every guarded instruction fetch. An invalid VMRUN resets the allocator so a
possibly unperformed wrap flush is retried. The full in-box SVM suite passed
with the candidate, including exact deadlines, replay, and masked breakpoints.

Two alternating same-box Tempo comparisons completed all 10,000 transfers:

| Run order | TLB policy | Fair interval | VM exits | VM runner and guest cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | Rotating ASID | 55.815 s | 27,846,220 | 4,830 |
| 2 | Per-entry flush | 58.645 s | 28,987,356 | 4,875 |
| 3 | Rotating ASID | 57.981 s | 28,854,285 | 4,825 |
| 4 | Per-entry flush | 57.850 s | 28,619,841 | 4,881 |

The candidate averaged **56.898 s** versus **58.247 s** for the control,
about **2.3%** faster, but the second pair is essentially tied in elapsed
time. The measured VM-runner cycles per exit fell by roughly **1%** in both
pairs. The larger first-pair elapsed-time gain mostly coincided with fewer
guarded fetches, so the expected Tempo benefit is modest and variable. The
focused `svm_bench` also passed; it measured about 19.1 million instructions/s
with rotating ASIDs versus 13.2 million with per-entry flushing in one suite
pass. Marker snapshots are under ignored
`target/boxctl-evidence/amd-asidrotate-1012/`.

### Locate the remaining VM-entry preparation cost

An isolated planner variant tried both the contextual and plain successor
predictions rather than discarding the plain prediction whenever contextual
data existed. All 262 VMX tests, the in-box SVM suite, and the complete Tempo
workload passed. Its first fair interval was **58.303 s** with **21.19 million**
nested-page faults, effectively unchanged from the nearby ASID-rotation
controls. It remains isolated in `/tmp/bedrock-amd-dualpred-1012`.

Kernel `perf` sampling of a full control run captured mostly VM creation,
so it did not characterize the transfer phase. Isolated cycle counters around
the VM-entry stages then measured two complete Tempo runs on EPYC 4245P boxes.
Subtracting the closest million-entry log snapshots to the fair markers gave
about **32 million** entries per interval. Preparation cost per entry was
roughly **215 cycles** refreshing the guarded translation tree, **390**
preparing instruction exits, **1,200–1,320** planning a batch, and **170–200**
setting execution/write guards. Timing instrumentation adds some overhead;
these values locate costs rather than predict uninstrumented wall time.

One split showed about **85%** of entries obtained a global counted plan.
The global-plan attempt cost **1,083–1,167 cycles per entry**, while fallback
decoding cost about **96–97 cycles per entry**. On successful plans, the
current-page hazard lookup took about **95 cycles**, breakpoint-proof
collection **348–352**, and widening to additional pages **539–618**. In a
finer widening run, each successful plan examined **2.6** candidate pages
and admitted **1.4**; translation lookup accounted for about **85 cycles**,
other-page hazard lookup **204**, and singleton alias-proof lookup **54** per
successful plan. The candidate-page checks are frequently useful, so dropping
recent pages is unlikely to help; the proof hit path is a better local target.
The diagnostics remain isolated in `/tmp/bedrock-amd-prepdiag-1012`; marker
snapshots and filtered kernel logs are under ignored
`target/boxctl-evidence/amd-prepdiag-1012/`.

A newly provisioned EPYC **4244P** box with the same HWE kernel failed the
existing `svm_bench` control-flow acceleration assertion on both the unchanged
branch and the diagnostic module, each using **20,029 exits** where the test
requires fewer than 100. Its transition suite passed. Its SVM CPUID leaf
`0x8000000a` reports virtual NMI support but **no PMC virtualization**
(`EDX=0x1ebfbcff`, bit 8 clear), and the module logs `SVM PMU unavailable;
using instruction stepping`. The 4245P reports both capabilities
(`EDX=0xfebfbdff`, bits 8 and 25 set) and passes the accelerated suite. This
is a hardware feature gate, not a diagnostic-module regression: the present
fast path requires PMC virtualization, so AMD-V alone does not provide it.
All transfer-phase comparisons above used 4245P boxes.

### Specialize the cached single-page alias-proof hit

The common global plan contains one hazardous current page. Its cached
breakpoint proof can be checked directly against that page and its hazard
offsets instead of using the generic multi-page search. Root, cache validity,
page, hazard count, and exact offsets are still checked; multi-page plans keep
the general matcher. All 262 VMX tests and the in-box SVM suite passed. Two
alternating same-box EPYC 4245P comparisons completed all 10,000 transfers:

| Run order | Proof match | Fair interval | VM exits | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | Direct singleton | 59.405 s | 29,591,377 | 2,283 |
| 2 | General control | 59.973 s | 29,687,108 | 2,334 |
| 3 | Direct singleton | 57.876 s | 28,795,495 | 2,278 |
| 4 | General control | 58.417 s | 28,926,639 | 2,316 |

The candidate averaged **58.641 s** versus **59.195 s**, about **0.9%**
faster. Preparation cost per exit fell in both pairs by **38–51 cycles**.
The candidate also had slightly fewer exits, so the exact wall-clock benefit
remains noisy; the normalized preparation reduction supports the local change.
Marker snapshots are under ignored
`target/boxctl-evidence/amd-singletonproof-1012/`.

### Check whether masked ranges can admit more code pages

An isolated candidate removed the planner's raw-hazard-count admission limit
while retaining the four actual breakpoint slots, boundary checks, and cached
alias proofs. All 262 VMX tests, the in-box SVM suite, and the 10,000-transfer
Tempo workload passed. Its fair interval was **57.511 s**, but it had only
**28.24 million** exits, fewer than nearby runs; preparation rose to **2,374
cycles/exit**, compared with **2,278–2,334** for recent control and proof-fast-
path runs. A focused unit case with five REP hazards showed why: the existing
masking algorithm reduces five raw entries to exactly four hardware slots,
leaving no slot for the next page. The isolated raw-count change was not
merged.

A second isolated variant tried to merge already-proved masked ranges again
when adding a successor page. Its focused unit case admitted the five-hazard
page together with another hazardous page, and all 263 VMX tests and the
in-box SVM suite passed. In the full Tempo boot, however, it reached only
about **4.45 seconds of guest time after 4.5 host minutes** and had not
reached the first transfer marker. The broader masks can trap harmless
instruction entries, making this policy far too expensive. The run was
stopped, and the pushed module was reloaded and passed the complete box
suite. This variant also remains isolated in
`/tmp/bedrock-amd-maskedcapacity-1012`; its log and the raw-count marker
snapshots are under ignored `target/boxctl-evidence/amd-maskedcapacity-1012/`.

### Hint recently used AMD alias proofs

The breakpoint-proof collector already hits its cache on more than 99% of
transfer-phase calls, but alternating code pages still search a four-way
proof set. A one-based direct hint now points to a recent proof for each
hash bucket. Every hinted hit still validates the guarded root, proof key,
physical page, hazard count, and exact offsets; collisions and evictions
fall through to the original search. A focused VMX test poisons the hint
with a different page's proof and verifies that the correct proof is found.
All 262 VMX tests and the in-box SVM suite passed.

On the same EPYC 4245P box, two candidate runs and an intervening unchanged
control completed all 10,000 transfers:

| Run order | Proof lookup | Fair interval | VM exits | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | Direct hint | 58.942 s | 29,257,691 | 2,309 |
| 2 | Unchanged | 59.344 s | 29,268,518 | 2,321 |
| 3 | Direct hint | 58.059 s | 28,785,223 | 2,303 |

The first adjacent pair improved by **0.402 s** with nearly identical exit
counts. Preparation was **12 cycles/exit** lower in that pair; the repeat
candidate was **18 cycles/exit** below its intervening control. This is a
small local gain, and the repeat's lower elapsed time also reflects fewer
exits. Marker snapshots are under ignored
`target/boxctl-evidence/amd-aliaslookup-1012/`.

### Bound extra breakpoint masks to 32 bytes

An isolated follow-up allowed a full four-slot plan to merge two existing
breakpoint ranges only when the combined range covered at most 32 bytes.
Its focused test admitted a five-REP page together with another hazardous
page; all 263 VMX tests and the in-box SVM suite passed. Unlike the earlier
wide-mask variant, it reached the first Tempo fair marker near normal startup
time and completed all 10,000 transfers. The same-box comparison showed a
clear throughput regression:

| Policy | Fair interval | VM exits | Nested-page faults | Preparation cycles/exit |
| --- | ---: | ---: | ---: | ---: |
| Extra 32-byte merges | 64.834 s | 27,242,973 | 19,568,082 | 3,077 |
| Unchanged control | 58.134 s | 29,200,650 | 21,505,404 | 2,267 |

The extra merges prevented **1.94 million** page faults, but preparation
rose by about **810 cycles per exit**, and VM-runner cycles rose from
**139.34** to **150.04 billion** despite fewer exits. The net cost was
**6.70 seconds**. The variant remains isolated in
`/tmp/bedrock-amd-maskedcapacity-1012`; snapshots are under ignored
`target/boxctl-evidence/amd-mask32-1012/`.

An independent isolated candidate removed seven redundant-looking VMCB PMC
stores on counted entries, retaining disabled controls for counters 1–5 and
programming only counter 0. The 23 SVM-core tests and box suite passed, and
all transfers completed. It took **57.492 s** versus the same **58.134 s**
control, but had **442 thousand fewer exits**; preparation was **2,289**
versus **2,267 cycles/exit**. There is no measured per-entry gain, so that
variant also remains unmerged in `/tmp/bedrock-amd-pmcstores-1012`.
Snapshots are under ignored `target/boxctl-evidence/amd-pmcstores-1012/`.

An isolated NPT write-guard variant cached the leaf-table location for each
recent guest physical page, keyed by EPT mapping generation. It avoids the
four-level software NPT walk when repeatedly guarding the same page. Its
focused EPT test and the in-box SVM suite passed, and the full Tempo run
completed in **57.027 s** with **28,637,174** exits, **21,051,826** nested-page
faults, and **65.42 billion** preparation cycles (**2,284 cycles/exit**).
The earlier same-box control took **58.134 s** with **29,200,650** exits and
**2,267 preparation cycles/exit**. The elapsed-time difference tracks the
lower exit count; normalized preparation did not improve. The cache is
unmerged in `/tmp/bedrock-amd-nptleaf-1012`; snapshots are under ignored
`target/boxctl-evidence/amd-nptleaf-1012/`.

A narrower follow-up to the 32-byte mask test precomputed just one cheap
merge pair from an already-full four-slot plan and admitted only a successor
with one cached breakpoint. Its focused case, all 263 VMX tests, and the box
suite passed. The complete Tempo run took **57.929 s** with **29,209,354**
exits, **21,534,280** nested-page faults, and **2,264 preparation cycles/exit**.
The same-box unchanged control had **58.134 s**, **29,200,650** exits,
**21,505,404** nested-page faults, and **2,267 preparation cycles/exit**.
The bounded merge did not reduce faults or meaningful preparation cost, so
it remains unmerged in `/tmp/bedrock-amd-maskpair-1012`. Snapshots are under
ignored `target/boxctl-evidence/amd-maskpair-1012/`.

An isolated SVM assembly candidate skipped the guest and host `XSETBV`
instructions when the already-read host XCR0 exactly equaled the guest XCR0
mask. It retained the existing XSAVE/XRSTOR handling and passed the in-box
SVM suite. Two candidate runs around an unchanged same-box control completed
all 10,000 transfers:

| Run order | XCR0 switch | Fair interval | VM exits | Runner and guest cycles/exit | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Skip equal writes | 55.492 s | 28,083,824 | 4,764 | 2,239 |
| 2 | Unchanged control | 57.875 s | 29,127,177 | 4,769 | 2,271 |
| 3 | Skip equal writes | 57.993 s | 29,194,534 | 4,745 | 2,290 |

The fast first candidate had about a million fewer exits; the second candidate
closely tracked its control in both exits and wall time. The small normalized
runner reduction was offset by preparation variation and did not establish
an end-to-end gain. The change remains unmerged in
`/tmp/bedrock-amd-xcr0equal-1012`; snapshots are under ignored
`target/boxctl-evidence/amd-xcr0equal-1012/`.
The guest VM state initializes its XCR0 mask to `0x7`, while this EPYC box's
host XCR0 reads `0x2e7`; the equality shortcut therefore cannot help before
the guest changes XCR0. This is consistent with its negligible measured gain.

### Probe global-plan reuse and a direct guarded-code proof

An isolated diagnostic counted direct-mapped repeats of the global planner's
current page, predicted successor, tree/code generations, mapping generation,
and recent-page context. During the Tempo transfer phase, an exact eight-page
context repeated on roughly **30%** of eligible calls, while the looser
page-plus-prediction context repeated on roughly **42%**. The diagnostic
hashing and counter logs perturb timing, so its **59.447 s** fair interval is
not a performance baseline. Source is isolated in
`/tmp/bedrock-amd-planprobe-1012`; logs and marker snapshots are under
ignored `target/boxctl-evidence/amd-planprobe-1012/`.

A second isolated candidate attached a direct singleton alias-breakpoint
proof to each recurrent hazardous-code memo that already held an NPT write
guard. Reuse requires the same CR3 root, guarded mapping, exact hazard
revision, alias invalidation epoch, and translation-tree generation. A
focused test covers tree invalidation and host writes; all 263 VMX tests,
the in-box SVM suite, and three complete Tempo runs passed. A diagnostic
version served about **77–80%** of transfer-phase breakpoint-collection
calls directly. Same-box timing did not show a reproducible gain:

| Run order | Proof path | Fair interval | VM exits | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: |
| 1 | Direct memo, diagnostic counter | 57.678 s | 28,847,349 | 2,254 |
| 2 | Unchanged control | 55.879 s | 27,892,624 | 2,298 |
| 3 | Direct memo, safety checks, no counter | 57.459 s | 28,946,980 | 2,257 |
| 4 | Unchanged control | 58.421 s | 29,436,626 | 2,233 |

The second control had lower normalized preparation cost than either
candidate. The candidate remains unmerged in `/tmp/bedrock-amd-memoalias-1012`;
snapshots are under ignored `target/boxctl-evidence/amd-memoalias-1012/`.

### Test precomputed breakpoint packing across code pages

An isolated candidate precomputed a 32-byte, one-merge breakpoint mask when
constructing a guarded singleton alias proof. It could then admit a predicted
hazardous page whose breakpoints fit alongside the current page's without
repeating the expensive alias walk. A focused test, all 263 VMX tests, the
in-box SVM suite, and a complete Tempo run passed. That run took **57.072 s**
for **28,069,821 exits**, including **20,471,512 nested-page faults**, with
about **2,315 preparation cycles/exit**. The adjacent low-exit unchanged
control had **20,483,686 nested-page faults** and about **2,298 preparation
cycles/exit**; the extra admission did not reduce faults meaningfully.

A diagnostic run counted breakpoint-slot combinations during the transfer
phase. Roughly 7.58 million `3+2` page pairs were candidates, but the initial
one-merge rule admitted only about 0.83 million of them; roughly 3.99 million
combinations needed more than five slots. We expanded the isolated candidate
to permit the precomputed incoming proof to join an existing two-page plan.
The new focused test and all 264 VMX tests and the in-box SVM suite passed.
Diagnostic counts showed roughly 14 million packed admissions, up from about
6 million, yet the full Tempo interval took **60.765 s** with **28,848,472
exits**, **21,142,856 nested-page faults**, and about **2,337 preparation
cycles/exit**. Debug exits stayed near 47,000. More admitted pages did not
translate into fewer faults or a faster run. An immediately following unchanged
control completed in **56.617 s**, with **28,314,144 exits**, **20,791,390
nested-page faults**, and about **2,265 preparation cycles/exit**. The
broader candidate had about 351,000 additional nested-page faults and higher
normalized preparation cost than that control; its diagnostic counters may
also contribute to the wall-time gap. These candidates remain unmerged
in `/tmp/bedrock-amd-packfive-1012`; logs and snapshots are under ignored
`target/boxctl-evidence/amd-packfive-1012/` and
`target/boxctl-evidence/amd-packmulti-1012/`.

### Measure actual page-transition prediction accuracy

An isolated probe measured the planner's last-successor, two-page-context,
effective (two-page-context with last-successor fallback), and eight-page
recency predictors on every observed virtual code-page transition. It passed
all 262 VMX tests, the in-box SVM suite, and the full 10,000-transfer Tempo
run on an EPYC 4245P box. In the transfer phase, subtracting the nearest
million-transition kernel-log snapshots at the fair markers gives roughly
**23.07 million transitions**. The two-page-context predictor hit **5.70
million (24.7%)**; the last-successor predictor hit **6.30 million (27.3%)**;
the planner's effective prediction hit **5.75 million (24.9%)**. The
eight-page recent set contained the actual destination **14.40 million times
(62.4%)**. Snapshot timing is within about one second of each fair marker,
so these are approximate phase counts. The run completed in **57.591 s**
with **20,782,861 nested-page faults**. These measurements suggest that
prioritizing a weak prediction over recent pages may consume breakpoint slots
that could prevent more execute faults. The probe remains isolated in
`/tmp/bedrock-amd-predprobe-1012`; snapshots and kernel logs are under
ignored `target/boxctl-evidence/amd-predprobe-1012/`.

An isolated selection test then tried the eight recent pages before the
predicted successor when widening a hazardous current-page batch. All 262
VMX tests and the in-box SVM suite passed; the single ordering-sensitive
unit assertion was updated to reflect the new page order. The complete Tempo
run was worse: **61.639 s**, **31,615,253 exits**, and **23,851,526
nested-page faults**, versus the preceding same-box predictor probe's
**57.591 s**, **28,325,838 exits**, and **20,782,861 faults**. The probe
has counter overhead, so this comparison is conservative: recent-first
caused roughly **3.07 million** more faults. It remains unmerged in
`/tmp/bedrock-amd-recentfirst-1012`; marker snapshots are under ignored
`target/boxctl-evidence/amd-recentfirst-1012/`. The predicted destination
may have particularly high value when it is absent from the recent set;
aggregate prediction accuracy alone does not capture that value.

An expanded version of the isolated predictor probe recorded recent-set
positions and prediction hits outside that set. It again passed all 262 VMX
tests, the in-box SVM suite, and the full Tempo workload. The transfer-phase
snapshots (nearest million-transition logs to the fair markers) covered about
**26.21 million transitions**: the effective predictor hit **6.58 million
(25.1%)**, the last-successor predictor hit **7.26 million (27.7%)**, and
the recent set contained the destination **16.60 million (63.3%)** times.
The effective predictor found a destination absent from that set **1.92
million times (7.3% of all transitions)**. Destinations at recent ranks one
through seven appeared about 4.28, 2.83, 2.61, 2.78, 2.15, 1.40, and 0.56
million times respectively. The run took **60.346 s**, with **21,849,694
nested-page faults**; extra counters perturb timing. Logs and snapshots are
under ignored `target/boxctl-evidence/amd-predrank-1012/`.

Giving the last-successor predictor priority over the two-page-context
predictor in the hazardous-page planner passed all 262 VMX tests and the
in-box SVM suite. Its full run took **60.254 s** with **22,527,378
nested-page faults** and **30,211,679 exits**. An adjacent unchanged control
on the same box took **58.272 s** with **21,311,004 faults** and **28,980,417
exits**. Higher aggregate last-successor hit rate did not improve page
selection under the four breakpoint slots. This candidate remains unmerged
in `/tmp/bedrock-amd-firstsucc-1012`; marker snapshots are under ignored
`target/boxctl-evidence/amd-firstsucc-1012/`.

An isolated, fault-weighted diagnostic stored the predicted physical page
and its admission reason in unused batch slots, then classified execute
faults after the guest run. It passed all 262 VMX tests, the in-box SVM
suite, and the full Tempo workload. Between kernel-log snapshots nearest the
fair markers, about **20.97 million** global-batch execute faults occurred;
**3.27 million (15.6%)** targeted the planner's predicted page. Of those
predicted-page faults, about **2.14 million (65.3%)** were rejected by the
combined hazard-count/boundary precheck and **1.12 million (34.2%)** by the
earlier hazard-count/page-count cutoff. Missing translations or hazard data
accounted for about 10 thousand, and missing cached alias proofs for about
8 thousand. No fault targeted a predicted page marked as admitted. The
diagnostic run took **59.241 s**, with **21,465,942 marker-bounded nested-page
faults**; its per-entry checks perturb timing. Source remains isolated in
`/tmp/bedrock-amd-missreason-1012`; logs and snapshots are under ignored
`target/boxctl-evidence/amd-missreason-1012/`. The combined buckets need
splitting before changing admission rules.

The refined diagnostic split those buckets on a second complete run. The
nearest marker-bounded kernel snapshots covered about **20.97 million**
global execute faults and **3.22 million** predicted-page misses. About
**1.12 million** of those misses hit the early raw hazard-count cutoff, and
**2.08 million** hit the raw combined hazard-count precheck. No misses in
this sample were attributed to the four-page batch limit, unguarded page
boundaries, an unsafe cross-page boundary, or a predicted page marked
admitted. Missing alias proofs accounted for about **7.6 thousand**. The
second full run completed in **59.175 s**. The diagnostic indicates that
raw hazard counts, rather than actual cached breakpoint slots, are excluding
predicted pages; any admission change must still preserve the exact alias
proof and four-register limit. Its logs and snapshots share the ignored
`target/boxctl-evidence/amd-missreason-1012/` directory.

An isolated candidate then removed both raw hazard-count gates and let the
existing cached singleton alias proofs' actual breakpoint lists decide whether
another page fits in four slots. It did not add runtime mask merging. All
262 VMX tests, the in-box SVM suite, and the full Tempo workload passed.
The run took **58.792 s**, with **21,350,691 nested-page faults** and
**29,021,511 exits**, effectively the same fault count as the nearby
unchanged control's **21,311,004 faults**. It also spent **67.67 billion**
VM-entry preparation cycles, above that control's **66.42 billion**. This
candidate remains unmerged in `/tmp/bedrock-amd-proofslots-1012`;
snapshots are under ignored `target/boxctl-evidence/amd-proofslots-1012/`.
The result suggests the actual four-slot constraint still blocks the
predicted faulting pages, despite the earlier raw-count rejections.

A follow-up isolated diagnostic removed the raw gates while recording the
actual breakpoint shortfall only when a global execute fault targeted the
predicted page. All 262 VMX tests, the in-box SVM suite, and the full Tempo
workload passed. Nearest marker snapshots cover about **20.97 million**
global execute faults and **3.30 million** predicted-page misses. Of these,
**1.12 million** occurred with all four breakpoint registers already used;
**1.84 million** required a combined five slots, **206 thousand** six, and
**108 thousand** seven. Missing cached alias proofs accounted for only about
**10.6 thousand**; no admitted predicted page faulted. The diagnostic run
took **58.956 s**. Its counters and extra lookup perturb timing, and it
remains unmerged. Logs and snapshots are under ignored
`target/boxctl-evidence/amd-proofslots-1012/`. The single-slot shortfall is
the largest packable class, but precomputing one narrow merge in the prior
packfive experiment did not reduce faults; merge feasibility among *faulting*
predictions needs measurement before another packing change.

The final isolated diagnostic checked whether any pair in each five-slot
predicted-page combination could share a 32-byte AMD breakpoint mask. It
passed all 262 VMX tests, the in-box SVM suite, and the complete Tempo
workload. Between kernel logs closest to the transfer markers, about
**862 thousand** faulting predicted-page combinations had such a pair.
This is about **40%** of the **2.12 million** proof-overflow misses in that
run, but only about **4%** of its **21,389,285** marker-bounded nested-page
faults. It is an upper bound: extra masked #DB exits and planner work may
outweigh saved fetch faults. The run took **59.342 s** with diagnostic
overhead. The earlier precomputed one-merge variant did not lower fault count,
so a new implementation would need to target the *actual faulting* five-slot
combinations and demonstrate an end-to-end gain. Source remains isolated in
`/tmp/bedrock-amd-proofslots-1012`; logs and snapshots are under ignored
`target/boxctl-evidence/amd-proofslots-1012/`.

### Test a narrow merge across the combined predicted-page breakpoint set

An isolated candidate removed the raw hazard-count gates and, only when
adding the predicted page produced exactly five distinct breakpoint ranges,
searched for one 32-byte mask covering two ranges on the same virtual code
page. This can merge a pair in an already-admitted page, which the earlier
precomputed singleton alternative did not cover. A focused coverage test,
all 263 VMX tests, the in-box SVM suite, and a full Tempo run passed on a
fresh EPYC 4245P box. The candidate was worse than an adjacent unchanged
control:

| Variant | Fair interval | Nested-page faults | Preparation cycles |
| --- | ---: | ---: | ---: |
| Combined five-slot merge | 58.356 s | 20,823,570 | 67.99 billion |
| Unchanged control | **56.028 s** | **20,610,934** | **63.32 billion** |

The candidate increased preparation cycles per exit by roughly 5.8% and did
not reduce faults. It remains unmerged in `/tmp/bedrock-amd-combine5-1012`;
marker snapshots are under ignored
`target/boxctl-evidence/amd-combine5-1012/`. This closes the narrow
five-slot packing line of experiments without a demonstrated workload gain.
