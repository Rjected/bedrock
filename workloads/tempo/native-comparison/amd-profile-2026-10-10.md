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

### Cache exact guarded global page plans

An isolated candidate cached the selected pages and breakpoint masks for an
exact current-page, predicted-successor, and eight-page recent context. A hit
required matching code, translation-tree, alias-invalidation, and NPT mapping
generations, plus persistent write guards on every hazardous admitted page.
The lookup also rejected a current RIP at a recorded hazardous offset. A
focused epoch-invalidation test, all 263 VMX tests, the in-box SVM suite, and
the complete Tempo workload passed on an EPYC 4245P box.

During the transfer interval, closest kernel-log snapshots show about
**25.17 million** cache lookups, **4.48 million hits (17.8%)**, and **13.62
million insertions**. The fair interval was **57.302 s** with **20,657,772
nested-page faults** and **67.26 billion** VM-entry preparation cycles. The
earlier unchanged same-box control took **56.028 s** with **20,610,934
faults** and **63.32 billion** preparation cycles. The validation and
insertion costs exceeded the work saved by hits; the candidate remains
unmerged in `/tmp/bedrock-amd-globalcache-1012`. Logs and snapshots are
under ignored `target/boxctl-evidence/amd-globalcache-1012/`.

### Measure execute-fault privilege and test address-half selection

An isolated diagnostic counted global execute faults by guest CS privilege
level during a full Tempo run. All 262 VMX tests, the in-box SVM suite, and
the workload passed. Nearest kernel-log snapshots to the transfer markers
cover about **20.97 million** execute faults: **14.95 million (71.3%)** at
CPL3 and **6.02 million (28.7%)** at CPL0. The diagnostic adds logging and
remains unmerged in `/tmp/bedrock-amd-cplprobe-1012`; evidence is under
ignored `target/boxctl-evidence/amd-cplprobe-1012/`.

A separate candidate admitted predicted and recent hazardous code pages
only when their linear address had the same high bit as the current page.
Other pages remained non-executable and faulted normally. All 262 VMX tests,
the in-box SVM suite, and the full Tempo workload passed, but the candidate
was slower than the unchanged same-box control:

| Variant | Fair interval | Nested-page faults | Preparation cycles |
| --- | ---: | ---: | ---: |
| Same-half selection | 58.324 s | 21,555,208 | 66.15 billion |
| Unchanged control | **56.028 s** | **20,610,934** | **63.32 billion** |

The filter increased faults by about 944 thousand, so preserving
cross-half predictions is valuable for this workload. The candidate remains
unmerged in `/tmp/bedrock-amd-samehalf-1012`; snapshots are under ignored
`target/boxctl-evidence/amd-samehalf-1012/`.

### Attribute VM-entry preparation with direct phase timers

On the same EPYC 4245P box, Linux `perf` hardware-cycle sampling during a
full transfer recorded only 16 samples, almost all during VM teardown.
Software `cpu-clock:k` sampling recorded 15,600 samples, but 89.5% landed
at `bedrock_local_irq_enable`, where deferred timer interrupts resume after
guest execution. Neither sampled profile can reliably attribute the
interrupt-disabled VM-entry preparation work. Reports are under ignored
`target/boxctl-evidence/amd-phaseprobe-1012/`.

An isolated run-loop probe instead measured TSC deltas around translation-tree
refresh, instruction-exit preparation, global page planning, NPT protection,
and remaining pre-entry work. It passed all 262 VMX tests, the in-box SVM
suite, and the full Tempo workload. The fair transfer interval was **59.486
s**, with **21,417,396 nested-page faults** and **66.73 billion** total
VM-entry preparation cycles. Kernel-log snapshots spanning 58.82 seconds near
the transfer markers measured approximately:

| Phase | TSC cycles |
| --- | ---: |
| Global page planning and scalar/replay decisions | **38.11 billion** |
| Instruction-exit preparation | 12.72 billion |
| Translation-tree refresh and host-write checks | 6.93 billion |
| NPT protection, temporary execution permission, translation retention | 6.51 billion |
| Other pre-entry work | 0.91 billion |

The phase timers and atomic counters perturb timing, and the kernel-log
snapshots do not align exactly with fair markers. Their roughly 65.2-billion
cycle sum is nevertheless close to the independent 66.7-billion preparation
counter. The planner is the largest directly measured preparation component.
Probe source remains unmerged in `/tmp/bedrock-amd-phaseprobe-1012`; marker
snapshots and phase logs are under ignored
`target/boxctl-evidence/amd-phaseprobe-1012/`. The next diagnostic should
split the planner into global-plan proof, scalar fallback, and window decode
costs before changing admission policy again.

The follow-up isolated split probe passed all 262 VMX tests, the in-box SVM
suite, and the full Tempo workload. Its fair transfer interval was **58.267
s**, with **21,149,579 nested-page faults** and **67.03 billion** VM-entry
preparation cycles. Kernel-log snapshots 56.85 seconds apart near the fair
markers show **28.31 million** global-plan attempts, of which **27.05
million (95.5%)** succeeded. The attempted global plans cost **34.61
billion** TSC cycles; full-window decode after failures cost only **0.036
billion**, and fallback batch planning cost **2.97 billion**. These buckets
account for nearly all of the earlier measured planning cost. Optimizing
successful `prepare_global_into` calls is the immediate target; removing
scalar fallback or speeding decode cannot account for a large workload gain.
The probe remains unmerged in `/tmp/bedrock-amd-plansplit-1012`; snapshots
and kernel logs are under ignored `target/boxctl-evidence/amd-plansplit-1012/`.

On a fresh EPYC 4245P box, the follow-up diagnostic timed substeps in
`prepare_global_into` while running the exact native-comparison compose file,
10,000-transfer image archive, and guest kernel matching the initrd's module
build ID. All 262 VMX tests, the box SVM suite, and the full workload passed.
The fair interval was **54.093 s**, with **18,935,772 nested-page faults**.
Nearest marker logs cover about **27.26 million** global-plan calls:
**13.87 billion** cycles in optional candidate-page selection,
**9.83 billion** in breakpoint-proof collection, **2.04 billion** in current
page hazard lookup, and **0.92 billion** in the trusted-page prediction path.
The remaining global-plan cost includes eligibility checks and batch setup.
Diagnostic source remains isolated in `/tmp/bedrock-amd-plansplit-1012`;
evidence is under ignored `target/boxctl-evidence/amd-plandetail-1012/`.

A second full run sampled one in 64 candidate loops to apportion selection
cost without adding a timer to every candidate. It passed the same checks
and finished the fair interval in **54.579 s**, with **19,266,164 faults**.
Marker-nearest snapshots contain **901,808 sampled candidate visits** (roughly
58 million estimated visits) and only **261 translation misses**. Within those
sampled visits, current/optional-page hazard lookup took **60.9 million**
cycles, translation lookup **29.7 million**, screening **29.9 million**,
boundary checks **25.1 million**, and alias-proof lookup **18.1 million**.
The whole candidate loop took **13.87 billion** cycles across all calls.
There is no single dominant lookup or miss path: reducing candidate-page
visits or improving selection quality is more promising than only optimizing
one lookup. Evidence is under ignored
`target/boxctl-evidence/amd-canddetail-1012/`; the probe remains unmerged.

### Check whether execute faults concentrate on recent hot pages

An isolated 1-in-64 execute-fault sampler tracked physical code pages on a
fresh EPYC 4245P box. Its in-box SVM suite, 262 VMX tests, and full fair Tempo
run passed. The fair interval was **54.078 s**, with **19,181,274 nested-page
faults**. In 18 complete one-million-fault windows spanning almost all of the
transfer interval, the hottest one, two, four, and eight pages accounted for
**11.2%**, **17.0%**, **25.9%**, and **37.5%** of sampled execute faults. Top
pages changed between startup and transfer, so a cumulative ranking alone
would have been misleading. Evidence is under ignored
`target/boxctl-evidence/amd-faultwindow-1012/`.

A follow-up sampled the guest RIP at each selected execute fault and compared
its virtual page with the planner's eight recent pages. The exact EPT-style
guest-linear-valid flag is not set on these AMD faults, so an initial run using
that flag was stopped and discarded. The corrected RIP-based run passed the
same checks and took **53.012 s**, with **18,788,640 faults**. Across 18 full
transfer windows, the hottest one, two, four, and eight pages covered
**11.0%**, **17.1%**, **26.0%**, and **37.6%** of samples. Among those hot-page
faults, respectively **92.2%**, **90.1%**, **88.8%**, and **86.9%** had their
virtual RIP page already in the eight-page recent set. Split-page fetches can
make RIP differ from the faulting fetch page, so these recency rates are
approximate. Evidence is under ignored
`target/boxctl-evidence/amd-faultrecent-1012/`; probe source remains isolated
in `/tmp/bedrock-amd-faulthot-1012`.

An isolated heuristic counted recent-page execute faults with periodic decay,
then tried the highest-scoring recent page after the existing successor
prediction and before the normal recent order. Omitted pages remained NX.
All 262 VMX tests, the in-box suite, and two full Tempo runs passed on the
same box, with an unchanged control between them:

| Variant | Fair interval | Nested-page faults | Preparation cycles |
| --- | ---: | ---: | ---: |
| Hot-page priority | 54.917 s | 19,164,592 | 64.87 billion |
| Unchanged control | 54.611 s | 19,500,541 | 61.91 billion |
| Hot-page priority repeat | 53.637 s | 18,711,324 | 63.74 billion |

The two candidate intervals average **54.277 s**, only 0.334 s below the
single control, while their preparation costs are higher. Fault counts vary
substantially across runs on this box, so this does not establish a reliable
end-to-end gain. The candidate remains unmerged in
`/tmp/bedrock-amd-hotpriority-1012`; snapshots are under ignored
`target/boxctl-evidence/amd-hotpriority-1012/`. A stronger version would
need to select hot pages with less work on every VM entry and demonstrate a
repeatable reduction against alternating controls.

A cached-choice variant avoided rescanning all eight recent pages on every
entry. It passed the in-box SVM suite and full Tempo workload but took
**54.939 s**, with **19,135,192 nested-page faults** and **64.37 billion**
preparation cycles. The adjacent unchanged control took **53.313 s**, with
**18,901,008 faults** and **59.93 billion** preparation cycles. This variant
regressed and remains isolated in `/tmp/bedrock-amd-hotcached-1012`; evidence
is under ignored `target/boxctl-evidence/amd-hotcached-1012/`.

A separate singleton alias-proof shortcut bypassed the general breakpoint
collector for the current hazardous page when its exact cached singleton proof
was valid. All 262 VMX tests, the in-box suite, and full Tempo workload passed.
Its fair interval was **54.426 s**, with **19,243,915 faults** and **61.34
billion** preparation cycles over **26,905,894 exits** (about **2,279 cycles
per exit**). The adjacent unchanged control took **55.104 s**, with
**19,463,112 faults** and **62.29 billion** preparation cycles over
**27,205,078 exits** (about **2,290 cycles per exit**). An earlier unchanged
control on the same box was **2,265 preparation cycles per exit**. The shortcut
has at most a small signal against variable fault counts and no reproducible
workload gain; it remains unmerged in `/tmp/bedrock-amd-singleproof-fast-1012`.
Evidence is under ignored `target/boxctl-evidence/amd-singleproof-fast-1012/`.

### Test SVM ASID reuse when nested translations are unchanged

The kernel backend normally assigns a fresh ASID on every VM entry. An
isolated candidate tracked every software NPT mapping or permission mutation
and reused the previous ASID only while the VMCB identity and NPT mutation
generation matched, resetting reuse at each `VM_RUN` boundary. A focused EPT
generation test, all 262 VMX tests, the in-box SVM suite, and three complete
Tempo runs passed. On one EPYC 4245P box, the corrected candidate took
**53.288 s** and **53.547 s** for the fair transfer interval, with
**18,955,012** and **19,006,203** nested-page faults. The unchanged run
between them took **54.975 s** with **19,406,864** faults. The candidate's
lower wall times mostly follow its lower fault totals: VM runner and guest
cycles per exit were about **4,839** and **4,856**, versus **4,873** for the
control. That sub-1% normalized difference is too small to establish a
substantial Tempo gain. An initial candidate run before the VMCB identity was
added took **54.404 s** with **19,187,931** faults. The short `svm_bench`
improved in repeated candidate runs, but an adjacent control varied enough
to make its gain uncertain. The candidate remains unmerged in
`/tmp/bedrock-amd-asid-reuse-1012.patch`; marker snapshots are under ignored
`target/boxctl-evidence/amd-asid-reuse-1012/`. This path cannot close the
roughly twofold Tempo gap while page faults dominate VM exits.

### Cut AMD breakpoint-mask MSR writes and retest bounded page packing

On another EPYC 4245P box, the SVM entry assembly now compares each saved
host breakpoint-address mask with the requested guest mask. It writes only
different MSRs and restores only those it changed. The in-box SVM suite,
including masked breakpoints and exact deadlines, passed. Two complete
unchanged-planner Tempo runs around an unchanged-module control yielded:

| Run order | Mask MSR handling | Fair interval | VM exits | Execute faults | VM runner plus guest cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Conditional writes | 53.102 s | 26,778,456 | 19,151,298 | 4,713 |
| 2 | Unchanged control | 53.278 s | 26,506,343 | 18,915,013 | 4,832 |
| 3 | Conditional writes | 53.427 s | 26,814,367 | 19,140,424 | 4,722 |

The conditional path consistently reduced normalized runner cost by about
**110 cycles/exit (2.3%)**, although fault-count variation masks it in wall
time. It is small but useful for already-masked pages and is retained.

We also repeated the previously regressing 32-byte breakpoint-range packing
policy with the cheaper MSR switching. Unrestricted candidate search cut
faults to **17,554,514** but raised VM-entry preparation to **81.36 billion
cycles** and took **58.745 s**. Limiting each full-breakpoint plan to two
likely next pages kept nearly all the fault reduction (**17,615,394** faults),
reduced preparation to **64.94 billion cycles**, and finished in **53.666 s**.
Allowing just one likely next page when the current page already has four
raw hazards produced **17,449,050** faults, **63.37 billion** preparation
cycles, and **52.904 s**. The adjacent unchanged-planner control took
**53.278 s**, with **18,915,013** faults and **60.50 billion** preparation
cycles. The packing variants passed all 263 VMX tests; every variant passed
the in-box suite and full Tempo workload. The one-page variant saves about 1.47 million
faults but increases preparation by roughly 240 cycles per remaining exit;
its 0.37-second apparent win is within run-to-run variation. It remains
unmerged in `/tmp/bedrock-amd-mask32-one-1012.patch`. Marker snapshots for
all runs are under ignored `target/boxctl-evidence/amd-maskwrite-1012/`.
The next optimization must reduce the work of deciding whether a faulting
page can fit, while keeping the four-register and exact alias-proof checks.

An isolated 32-entry cache then memoized the pure five-to-four breakpoint
packing result for the bounded one-page policy. Hits compared all five input
addresses and masks; collisions could only cause a recomputation. All 263 VMX
tests, the in-box suite, and the full Tempo workload passed, but the cache
was slower: **54.315 s**, **17,744,552** faults, and **66.06 billion**
preparation cycles over **25,494,367** exits, versus the preceding uncached
candidate's **52.904 s**, **17,449,050** faults, and **63.37 billion**
preparation cycles over **25,081,985** exits. Normalized preparation rose
from about **2,526 to 2,591 cycles/exit**. It remains unmerged in
`/tmp/bedrock-amd-maskpack-cache-1012.patch`; marker snapshots are under
ignored `target/boxctl-evidence/amd-maskpack-cache-1012/`.

Finally, a sampled diagnostic split the bounded policy's candidate loop.
The nearest kernel-log snapshots to its fair markers (each within about
0.6 seconds) cover **26.21 million** global-plan calls and **1.10 million**
sampled optional-page visits. Within the sampled visits, translation lookup
used **33.9 million** cycles, page-hazard lookup **92.6 million**, singleton
alias-proof lookup **41.7 million**, and breakpoint packing **58.0 million**.
The sampled loop total was **366.6 million** cycles; the unlisted remainder
includes screening, boundary checks, and measurement overhead. Packing is
roughly **16%** of the sampled loop, so even a free packing operation would
not close the workload gap. This diagnostic completed in **53.322 s** with
**17,651,994** faults; its source remains isolated in
`/tmp/bedrock-amd-maskpack-phase-1012.patch`, and logs and marker snapshots
are under ignored `target/boxctl-evidence/amd-maskpack-phase-1012/`. Further
work should attack the repeated fault/entry path or change which code pages
need traps, rather than only caching breakpoint arithmetic.

### Profile the complete transfer with AMD IBS

On an EPYC 4245P HWE box, `perf record -e ibs_op// -c 1000000 -C 1 -a`
collected 224,957 samples during boot, then Bedrock rejected an SVM PMU
deadline (`count=10 budget=16`); the host also reported unexpected NMIs.
That run is not a valid workload result. Reducing the IBS sample rate to
`-c 100000000` allowed the exact 10,000-transfer workload to pass, with
fair markers **54.294 s** apart and no additional PMU budget failure. It
collected 10,167 samples with none lost, about 2,736 in the approximate
transfer time slice. The transfer sample had 30.6% in `svm_run_guest`, 9.5%
in `handle_run`, 3.6% in `collect_page_breakpoints`, 2.6% in
`prepare_global_into`, and 2.5% in `page_hazards_memo`. IBS attribution at
`VMRUN` may include guest residency, so its 32.6% share within
`svm_run_guest` must not be interpreted as VM transition overhead.

A second complete run used the software `cpu-clock` event at 99 Hz and
finished in **52.359 s**, with 10,000 transfers admitted and included.
Its call-chain samples collapsed onto `bedrock_local_irq_enable`, where
deferred timer interrupts were delivered, so that sampler did not locate
the code consuming the interval. Bedrock's marker snapshots provide the
usable cycle breakdown for this run: **26,168,249 exits**, including
**19,419,225 nested-page faults**; **61.5%** of measured cycles in the VM
runner plus guest, **29.7%** in VM-entry preparation, **5.6%** in exit
bookkeeping, and **1.6%** in exit handlers. Reducing planner arithmetic
alone cannot close the roughly twofold gap to the historical Intel interval;
the fault and repeated VM entry/exit volume remain the main target. Full
logs, marker snapshots, and `perf.data` files are under ignored
`target/boxctl-evidence/amd-ibs-1012/`.

### Defer guest XSAVE while the FPU reservation is held

The SVM runner previously issued `XSAVEOPT` after every VM exit, even though
the kernel FPU reservation kept the guest registers resident for up to 64
entries. The new path marks the memory image dirty during that reservation
and flushes it before the reservation rotates or ends. It also flushes before
an intercepted XSETBV changes the component mask. A dedicated in-box test
checks XMM contents after more than 64 intercepted instructions, then forks
at a userspace RNG exit and checks both the child and parent after resuming.
All 262 VMX library tests, the in-box SVM suite, and two complete Tempo runs
passed on the same EPYC 4245P HWE box.

| Same-box order | Fair transfer interval | VM exits | Nested-page faults | Runner plus guest cycles/exit |
| --- | ---: | ---: | ---: | ---: |
| Deferred XSAVE | 51.758 s | 25,926,638 | 19,226,293 | 4,700 |
| Unchanged control | 51.857 s | 25,769,978 | 19,071,485 | 4,773 |
| Deferred XSAVE repeat | 51.646 s | 25,915,841 | 19,195,199 | 4,696 |

The two candidate runs saved about **75 normalized runner cycles per exit**
against the adjacent control. Their wall times averaged only 0.155 s lower;
fault-count variation prevents claiming an end-to-end gain of that exact
size. The change is retained because it removes repeated FPU saves while
preserving snapshot and fork state. Marker snapshots and full logs are under
ignored `target/boxctl-evidence/amd-fpudefer-1012/`.

### Recheck VMCB clean-field caching on the current fast path

The earlier width-specific unaligned VMCB access candidate had already
regressed on a matched Tempo run (see "Profile and test VMCB access costs").
A duplicate of that candidate passed local and in-box tests on this branch,
but was stopped during boot and reverted after locating the prior result.

An isolated [VMCB clean-field](https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2)
candidate instead set the AMD hardware's clean bits after each successful
VMRUN and invalidated only the field group written
before the next entry. Unrecognized writes invalidated every group; forks,
new VM_RUN calls, and failed VMRUNs forced a full reload. The 4245P host
advertised `vmcb_clean`. All 24 SVM and 262 VMX library tests, the in-box SVM
suite, and the complete 10,000-transfer workload passed. An adjacent
unchanged-module control on the same box was:

| Variant | Fair transfer interval | VM exits | Nested-page faults | Runner plus guest cycles/exit | Entry-preparation cycles/exit |
| --- | ---: | ---: | ---: | ---: | ---: |
| Clean-field candidate | 51.655 s | 25,637,367 | 18,988,028 | 4,754 | 2,364 |
| Unchanged control | 53.155 s | 26,601,607 | 19,738,396 | 4,743 | 2,318 |

The candidate's lower wall time follows its roughly 964,000 fewer exits.
Its normalized runner and preparation costs were both higher, so there is
no evidence that clean-field tracking speeds this workload. The candidate
was reverted. Logs and marker snapshots are under ignored
`target/boxctl-evidence/amd-vmcb-clean-1012/`.

### Sample the remaining trap-flag exits

An isolated diagnostic build sampled every 2,048th SVM trap-flag exit on the
same 4245P HWE box, recording the next guest RIP, opcode prefix, CR3, and
synthetic-exit qualification. The in-box SVM suite and full 10,000-transfer
workload passed. The measured transfer had **25,690,594 exits**, including
**19,049,572 NPT faults** and **4,424,251 trap-flag exits**. Its fair interval
was **75.226 s**, versus roughly 52 s without instrumentation: guest-memory
reads and kernel logging make this run unsuitable as a speed comparison.

The kernel ring retained 1,798 samples from the final 31 seconds of the
75-second transfer interval, around 83% of the 2,160 samples expected from
its total trap-flag count. Of the retained samples, 974 had ordinary
qualification and 786 had the synthetic page-execution plus scalar-replay
qualification. The largest single RIP was the guest kernel's `SYSRETQ`
(`ffffffff810001c9`), at 120 samples (6.7%). The next two RIPs were userspace
`CMP RAX, ...` sequences at 79 and 25 samples. No one instruction dominates
the remaining trap exits, so optimizing only one opcode is unlikely to close
the roughly twofold gap to Intel. The repeated NPT fault/entry path remains
the higher-value target. Diagnostic source is isolated in
`/tmp/bedrock-amd-mtf-profile-1012`; marker snapshots and kernel log are
under ignored `target/boxctl-evidence/amd-mtf-sample-1012/`.

### Check repeated secondary PMC clearing

An isolated candidate stopped clearing the ten secondary programmable-counter
fields in the VMCB on every counted entry. The VMCB starts with zeroes,
guest writes to these MSRs are intercepted, and SVM saves their guest values
on exit. All 23 SVM and 262 VMX library tests, the in-box SVM suite, and two
complete 10,000-transfer runs passed. On the same 4245P HWE box, the adjacent
unchanged control was:

| Run order | Variant | Fair interval | VM exits | NPT faults | Runner plus guest cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Skip redundant clears | 53.317 s | 26,545,707 | 19,698,494 | 4,736 |
| 2 | Unchanged control | 51.306 s | 25,774,714 | 19,105,418 | 4,741 |
| 3 | Skip redundant clears | 52.232 s | 26,142,953 | 19,394,007 | 4,736 |

The candidate's apparent five-cycle runner improvement is negligible, and
both candidate runs had more exits and longer wall intervals. The change
remains isolated in `/tmp/bedrock-amd-pmu-zero-1012` and is not retained.
Marker snapshots and full logs are under ignored
`target/boxctl-evidence/amd-pmuzero-1012/`.

### Bound wider AMD breakpoint-mask opportunities

An isolated diagnostic extended the earlier predicted-page shortfall probe to
record the smallest AMD breakpoint-address mask that could merge any pair in
each five-slot combination. All 262 VMX tests, the in-box SVM suite, and the
exact 10,000-transfer workload passed on a fresh 4245P HWE box. Its fair
interval was **53.190 s**, with **19,180,932 NPT faults**. Kernel-log samples
nearest the markers cover **16.78 million** global execute faults and **1.54
million** predicted-page proof-overflow misses; they do not align exactly with
the marker-bounded interval.

| Maximum mask span | Faulting five-slot combinations that could merge a pair |
| --- | ---: |
| 32 bytes | 669,892 |
| 64 bytes | 717,459 |
| 256 bytes | 859,734 |
| 1 KiB | 1,268,070 |

These are upper bounds on avoidable fetch faults, not expected speedups.
Moving from 32 to 256 bytes adds only about **190 thousand** packable
combinations, around 1% of the run's NPT faults, while wider masks can
cause false #DB exits across many ordinary instructions. The previously
tested 32-byte combined merge already increased preparation time without
reducing faults, so this measurement does not justify a wider merge policy.
Diagnostic source is isolated in `/tmp/bedrock-amd-maskwidth-1012`; full
logs and marker snapshots are under ignored
`target/boxctl-evidence/amd-maskwidth-1012/`.

### Probe IBS interrupts during REP execution

AMD [IBS operation sampling](https://docs.amd.com/r/en-US/68658-uProf-getting-started-guide/Introduction-to-IBS-Instruction-Based-Sampling)
counts sampled micro-ops rather than the programmable retired-instruction
event used by the SVM deadline path. On the same 4245P box, a native program
repeatedly executed 64 MiB `REP STOSB`. `perf record -e ibs_op// -c 1000000`
collected about 1,000 samples, with 96.9% attributed to the REP function and
none lost. A separate perf-event SIGIO probe observed **678** interruptions
with the interrupted RIP at `REP STOSB` and RCX partway through the operation.
Thus IBS can interrupt a long REP before it completes on this CPU.

An **unsafe diagnostic-only** SVM build then omitted REP from the page hazard
scanner and ran a 16 MiB guest `REP STOSB` under host IBS sampling at period
100,000. The VMCB reported repeated NMI exits (`code=0x61`) with RIP still
at REP and RCX decreasing. The guest eventually shut down at virtual count
**191** despite a requested stop at **1,000**; the same unsafe probe without
IBS shut down at count **111**. With the normal module restored, the identical
guest stopped exactly at count **1,000**, with RCX reduced by 900 and only
**seven** exits. The full in-box SVM suite passed again after restoration.

IBS can supply an asynchronous mid-REP exit, but it does not reveal the RCX
value when execution *entered* a REP reached later in an unguarded code page.
Without that starting value, Bedrock cannot account for its iterations or
enforce an exact deadline. Sampling also adds exits to a REP that the current
guarded bounded-chunk path executes efficiently. It is not a drop-in
replacement for REP entry traps. The unsafe module was not retained. Probe
source is isolated in `/tmp/bedrock-amd-ibs-guest-probe-1012`, and native
SIGIO/perf and guest VMCB logs are under ignored
`target/boxctl-evidence/amd-ibs-rep-1012/`.

### Run SYSRET pages in global AMD counter batches

The page hazard scanner previously treated every `SYSRET` opcode as unsafe
for global execution. That was conservative because `SYSRET` can restore TF
from R11. On AMD, a `SYSRET` that sets TF produces #DB **before** the first
return-target instruction, as specified in the
[AMD64 APM volume 2](https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2).
The global gate now distinguishes that guest #DB from its own breakpoint
boundary, preserves the guest TF and DR6 state, and lets these code pages use
retired-instruction counter batches. A dedicated hardware transition test
requires #DB at the target RIP with the target NOP still unexecuted.

The scalar fallback software-emulates SYSRET. It previously delayed this #DB
until after the target NOP. The emulator now reports #DB at the architectural
boundary and accounts for the emulated SYSRET as one retired instruction. The
transition test also passed with global batching temporarily disabled inside
the test box. All 262 VMX unit tests and the full in-box SVM suite passed with
the final module.

Same-box EPYC 4245P HWE A/B/A runs of the exact 10,000-transfer workload all
completed with 10,000 successful and zero failed transfers:

| Run order | Variant | Fair transfer interval | VM exits | NPT faults |
| --- | --- | ---: | ---: | ---: |
| 1 | SYSRET global candidate | 46.774 s | 22,919,577 | 16,865,323 |
| 2 | Unchanged branch | 53.388 s | 26,489,127 | 19,646,023 |
| 3 | Final SYSRET change | 46.014 s | 22,658,539 | 16,648,651 |

The final run is **7.374 s (13.8%) faster** than the same-box control, while
still well behind the historical Intel Bedrock **24.087 s** comparison. Full
logs and marker snapshots are under ignored
`target/boxctl-evidence/amd-sysret-1012/`.

### Locate the remaining AMD trap exits after SYSRET acceleration

A temporary diagnostic sampled every 2,048th trap-flag exit, including its
next RIP, four code bytes, CR3, and scalar-replay qualification. The in-box
SVM suite and complete 10,000-transfer workload passed. Its fair interval
was **63.387 s**, versus **46.014 s** without sampling; guest-memory reads
and kernel logging make this run unsuitable as a speed comparison. The fair
interval recorded **3,769,318** trap-flag exits and **16,658,135** NPT faults.

All **1,840** expected transfer-phase trap samples were retained. **1,230
(66.8%)** had a RIP in one userspace 4 KiB page (`0x556f5ecd2000`, CR3
`0x102a15000` in this seeded run). The sampled RIPs span offsets `0x3e7` to
`0x6f8` rather than one instruction; the same page has sampled `REP MOVSQ`
entries at offsets `0x3e7` and `0x5f4`. Of its samples, **633** carried the
synthetic scalar-replay qualification and **595** had ordinary qualification.
This points to repeated scalar traversal of a REP-containing hot code page as
the next high-value target. The REP entries still require exact accounting;
simply removing their guards was already shown to undercount. The diagnostic
module was removed and the tested module restored. Source is isolated in
`/tmp/bedrock-amd-mtf-after-sysret-1012`; logs and marker snapshots are under
ignored `target/boxctl-evidence/amd-mtf-after-sysret-1012/`.

A follow-up diagnostic tried to log the page's hazard proof at the exact
virtual address observed in that sampled run. The address did not recur in
the next guest: no targeted log was emitted by the first transfer marker.
The probe was stopped, and the tested module was restored and passed the full
in-box suite. This attempt yielded no hazard-proof conclusion. The next probe
must identify the page by its instruction bytes or resolve its mapping in
each run instead of relying on the prior run's user-space address. The
aborted run's log and first marker are under ignored
`target/boxctl-evidence/amd-rep-hotdiag-1012/`; isolated source is in
`/tmp/bedrock-amd-rep-hotdiag-1012`.

### Check cross-page prediction and guarded global-plan reuse

On the same EPYC 4245P HWE box, the current `66cb502` path completed the
10,000-transfer workload in **44.354–44.712 s** (two runs, zero failed
transfers), versus a **45.418 s** same-box control before the user-mode
masked-breakpoint retry change. Kernel-module builds, loads, and tests ran
only inside the disposable box.

An isolated NPT diagnostic sampled the transfer interval's execute faults.
Among 360 samples, 299 were from user mode, 61 from kernel mode, and 85% of
faulting pages had only one or two true hazard offsets. A separate exact
counter found that just **4.4%** of timed execute faults returned to the same
physical page as the immediately preceding fault. The remaining faults are
mostly transitions among many code pages. The context-specific successor
matched 80 of 360 sampled destinations, while the first-order successor
matched 85, including 36 that the contextual prediction missed. Adding both
predictions passed the in-box suite but regressed the uninstrumented fair
interval to **51.636 s**; its extra per-entry selection cost outweighed the
faults avoided. The candidate was discarded.

Sampled cycle counters on the baseline attributed **63.0%** of timed
VM-entry preparation to plan selection, **21.6%** to instruction-exit
preparation, **9.5%** to NPT protection, **3.8%** to temporary execute
permissions, and **2.1%** to retained translation work. This agrees with the
earlier global-planner profile. A guarded global-plan cache then passed the
262 VMX tests, in-box SVM suite, and full workload, but took **50.497 s** and
raised cumulative entry-preparation cycles without reducing NPT faults. In a
diagnostic repeat, only **22.4%** of eligible transfer-phase lookups hit while
**50.3%** copied a full plan into the cache. Requiring a context to recur
before storing cut copies to **5.7%** of lookups, but hits fell to **18.3%**
and the diagnostic fair interval remained **51.588 s**. Both cache variants
were discarded. The diagnostic source remains isolated in
`/tmp/bedrock-amd-npt-profile-1012`, `/tmp/bedrock-amd-entrytime-1012`,
`/tmp/bedrock-amd-dual-predict-1012`, and
`/tmp/bedrock-amd-globalreuse-1012`; logs and counters are under ignored
`target/boxctl-evidence/amd-npt-sample-1012/`.

### Recheck alias-proof hits and page-transition hysteresis

A fresh EPYC 4245P HWE box ran the exact 10,000-transfer workload on the
current fast branch. Three uninstrumented controls took **50.297 s**,
**50.148 s**, and **49.957 s** between fair markers. This box was slower
than the previous box's 44-second runs, so all comparisons below use its
own controls. Each completed 10,000 successful and zero failed transfers;
all kernel-module work remained inside `boxctl` boxes.

Two low-rate collector diagnostics also completed at **50.264 s** and
**50.224 s**. The transfer-phase counter snapshots span about 20 million
alias-proof collection calls: **99.7%** hit a valid proof, only **0.3%**
needed an alias walk, and none saw a dirty or unguarded tree. Of the hits,
about **80%** used the keyed hint, **17.6%** the last-used slot, and **1.9%**
searched the cache set. Earlier findings that alias walks and proof churn
dominated this workload no longer describe the current branch. A validated
single-page hint bypassed the offset hash on some hits, but its full run took
**50.147 s**; the immediately following unchanged control took **50.148 s**.
The change was discarded.

An isolated two-destination transition predictor reduced cumulative NPT
faults from roughly 66.2 million to **65.8 million**, but took **50.182 s**
and increased normalized VM-entry preparation from about 2,461 to 2,533
cycles per exit. A smaller saturating-confidence predictor ran twice at
**49.013 s** and **50.552 s**; the apparent first-run gain did not repeat.
It reduced faults to 65.3–65.8 million but raised preparation to about
2,486–2,492 cycles per exit. Restricting confidence tracking to the
context-specific predictor took **50.140 s** with 65.8 million faults,
again matching the controls. These variants passed 263 VMX library tests
and the in-box SVM suite, but none established an end-to-end speedup, so
they remain unmerged. Their source is isolated in
`/tmp/bedrock-amd-aliasprofile-1012`, `/tmp/bedrock-amd-aliasfast-1012`,
`/tmp/bedrock-amd-majority-1012`, `/tmp/bedrock-amd-sticky-predict-1012`,
and `/tmp/bedrock-amd-sticky-second-1012`; full logs and stats are under
ignored `target/boxctl-evidence/amd-aliasprofile-1012/`.

### Test an NPT leaf cache on the entry path

An isolated direct-mapped cache stored the NPT leaf table and index by guest
physical page and mapping generation. The first 64-entry inline version passed
the EPT and VMX library tests, but the disposable box stopped answering SSH
during `svm_smoke` after the module was loaded. No host panic log could be
recovered before that box expired, so the cause is unproven. That variant was
not retained. An eight-entry version reduced the inline footprint by about
1.75 KiB and passed the library tests and full in-box SVM suite on a fresh
EPYC 4245P HWE box. Kernel-module builds, loads, and tests stayed in boxes.

The eight-entry version cached leaves used for permission changes. Its two
10,000-transfer intervals were **50.599 s** and **50.559 s**, versus same-box
unchanged controls of **51.120 s** and **48.731 s** in alternating order. All
runs completed 10,000 successful and zero failed transfers. The unchanged
runs varied by 2.39 s and roughly 1.0 million NPT faults, so the apparent
first-pair gain is not evidence of an improvement. VM-entry preparation was
about 2,767–2,833 cycles per exit across these runs, without a stable
cache-specific reduction.

A separate eight-entry variant also cached the read-only NPT leaf lookup used
by the global planner. It passed the library tests and in-box suite, then ran
the full workload in **50.478 s**, with 10,000 successful transfers and about
2,798 preparation cycles per exit. This remains within the observed run
variation. Both variants were discarded, and the unchanged module was
restored and passed the in-box suite. Sources remain isolated in
`/tmp/bedrock-amd-nptleafcache-1012` and
`/tmp/bedrock-amd-nptleafcache-hot-1012`; logs and marker snapshots are under
ignored `target/boxctl-evidence/amd-nptleaf-1012/`.

### Identify the hot REP page's breakpoint cost

A low-rate, byte-identified REP diagnostic completed the full workload with
10,000 successful and zero failed transfers. Logging raised the fair interval
to **53.668 s**, so this is profiling evidence rather than a speed result.
The fair-marker timestamps delimit 69 sampled synthetic `MonitorTrapFlag`
exits; **28** selected the same user code page. That page contained both
`REP MOVSQ` sites at offsets `0x3e7` and `0x5f4`. Its guarded hazard proof
reported **five** unsafe offsets: `0x3e7`, `0x4c5`, `0x5f4`, `0x7ae`, and
`0x81b`. The gate was ready and clean at sampled REP entries, with a generous
instruction budget. Thus the repeated scalar behavior does not come from a
dirty translation gate or an unavailable AMD retired-instruction counter.

Many sampled exits on that page had qualification `0x5000...`, the synthetic
page-execution boundary plus breakpoint-replay bits. They are **not** all
hardware trap-flag steps: the global plan packs five hazards into four AMD
breakpoints by masking a range, and ordinary instructions in that range can
exit as harmless breakpoint hits. The earlier trap-exit sample described the
symptom but conflated these synthetic exits with actual single-stepping.
This is a concrete target for reducing exit volume, although simply lowering
the maximum breakpoint-mask width made two library safety tests fail; that
unvalidated candidate was never loaded. Diagnostic source remains isolated
in `/tmp/bedrock-amd-repdiag2-1012`, and logs are under ignored
`target/boxctl-evidence/amd-repdiag2-1012/`.

### Guard only reachable instructions on five-hazard pages

A follow-up low-rate diagnostic applied the existing bounded control-flow
decoder to 68 sampled entries on that five-hazard page. All 68 produced a
proof needing at most four exact breakpoints: 52 needed one, 3 needed two,
6 needed three, and 7 needed four. The diagnostic completed 10,000/10,000
transfers, but its 53.491 s fair interval includes probe overhead.

For a five-hazard page, the global SVM plan now tries that bounded proof
before packing all hazards into a wide masked breakpoint. It falls back to
the existing page-wide proof if the reachable region cannot be proven. A
cached region proof records each visited instruction start; entries at those
starts can reuse the same guarded successors under the same code revision,
while an unvisited entry requires a fresh proof. Outgoing aliases are still
rechecked. The 263 `bedrock-vmx` library tests and the in-box SVM suite pass.

On the same `box-688c6d68` EPYC 4245P HWE host, alternating candidate,
control, candidate runs of the pinned 10,000-transfer Tempo workload gave
**47.316 s, 50.761 s, 47.437 s** for the fair marker interval. All three
had 10,000 successful transfers and zero failures. Synthetic `mtf` exits
were 1,564,267, 1,587,002, and 1,557,053 respectively. Total VM exits
were 22,294,456, 22,469,287, and 22,444,449, so whole-workload exit
counts also vary. This is an observed ~6.6–6.8% improvement relative to
the adjacent control, not a claim of general AMD virtualization overhead.
The historical Intel workload was 24.087 s on different hardware, leaving
this AMD run about 1.97x slower. Logs and fair-marker exit snapshots are
under ignored `target/boxctl-evidence/amd-regionproof-1012/`.

### Check region-proof widening and host perf sampling

An isolated candidate let a five-hazard page's reachable-region proof use
its spare breakpoint slots to admit a predicted second page. It passed the
then-existing 263 VMX library tests and in-box SVM suite, and the complete
Tempo run had
10,000 successful transfers. The candidate took **47.510 s** with
18,137,503 NPT faults; the adjacent unchanged control took **47.132 s** with
17,902,462 faults. More importantly, a targeted unit test found a real
proof gap: a direct branch from the second page can re-enter the first at an
unvisited hazard offset. The candidate admits that page without guarding
the target. It was discarded; the pushed single-page region proof remains
safe, and the re-entry case is retained as a regression test. Candidate
source is isolated in `/tmp/bedrock-amd-regionwiden-1012`; logs and marker
snapshots are under ignored `target/boxctl-evidence/amd-regionwiden-1012/`.

Host `perf` hardware-cycle sampling during a complete pinned run collected
only 306 samples, mostly during guest-memory allocation, because Bedrock
uses the CPU performance counters while the guest runs. Software-clock
sampling collected about 36,000 samples and completed 10,000/10,000
transfers, but 99% landed at `bedrock_local_irq_enable` as guest exits
re-enabled interrupts. Neither profile resolves the interrupt-disabled
VM-entry preparation path. The next profile needs low-rate cycle probes
inside that path. The software-clock diagnostic and report share the ignored
evidence directory above; the hardware-cycle report is under
`target/boxctl-evidence/amd-regionproof-1012/`.

### Reuse guarded hazard memos directly on AMD global entries

Low-rate in-kernel cycle probes on a complete 10,000-transfer run found that
plan selection consumed **63.1%** of sampled VM-entry preparation cycles,
about **1,550 cycles per exit**. Instruction-exit preparation took 22.0% and
NPT protection 9.2%. A finer probe attributed **87.4%** of plan-selection
cycles to the global planner and 5.0% to scalar fallback. Within the global
planner, hazardous-page widening cost about 648 cycles per call, breakpoint
collection 485, and current-page hazard lookup 104. The widening scan
admitted about **33.2 million** successor pages across 24 million timed
planner calls, so dropping it would likely raise NPT faults. Its largest
measured subtask was another page's hazard lookup, about 223 cycles per call.

A hit classifier showed **99.95%** of timed hazard lookups reached the larger
memo path, since global entries clear the short code cache. A new direct
64-slot hint now points to a recurrent memo only after that memo is
write-guarded. Every hit rechecks the memo's page, revision, valid and guarded
bits, and NPT mapping generation. It maintains the memo's usage clock and
falls back to the existing byte-checking path on any mismatch. A regression
test changes code through a host write and replaces its NPT mapping; both
changes invalidate the fast result. All 265 VMX library tests and the full
in-box SVM suite pass.

On the same EPYC 4245P HWE box, two uninstrumented runs of the pinned
10,000-transfer workload took **46.170 s** and **47.134 s** with the hint;
nearby unchanged runs took **47.132 s** and **48.404 s**. Every run had
10,000 successful transfers and zero failures. The second hint run used
about **2,646 VM-entry preparation cycles per exit**, versus **2,774** for
the adjacent control. Different exit totals still account for some wall-time
variation, so this establishes a modest same-box gain, not an Intel parity
claim. A simpler variant using the existing 1,024-entry memo hint took
47.223 s and did not show a stable advantage; it remains isolated in
`/tmp/bedrock-amd-hazard-direct-1012`. The retained variant was tested in
`/tmp/bedrock-amd-hot-hazard-1012`. Logs and fair-marker snapshots are under
ignored `target/boxctl-evidence/amd-hazard-fast-1012/`; diagnostic sources
are isolated in `/tmp/bedrock-amd-entryfine-1012`.

A follow-up safety test found that a closed reachable loop on an untrusted
five-hazard page could produce a global plan with zero breakpoints. That plan
did not write-protect the code page, so its decoded-region proof could become
stale. The global path now uses the page-wide breakpoint proof when the
reachable proof has no breakpoints. The targeted regression test, all 266
VMX library tests, and the full in-box SVM suite pass. This is a correctness
guard for the accelerated path; it does not explain the large Tempo timing
gap.

### Test cached global entry plans

On the same EPYC 4245P box, the guarded baseline at `6cbb3e3` completed the
pinned 10,000-transfer interval in **46.742 s**, with 17,821,678 NPT faults
and about 2,680 VM-entry preparation cycles per exit. Caching the complete
current-plus-successor-page plan passed 267 local VMX library tests, the
in-box SVM suite, and 10,000/10,000 transfers, but took **50.494 s** and
21,948,155 NPT faults. Reusing its stale-but-valid successor-page set saved
preparation cycles while causing 4.13 million more NPT faults; this variant
was discarded.

A narrower cache retained only the current page's guarded breakpoint proof
and recomputed successor pages on every entry. It hit on 71.74% of sampled
lookups, passed the same correctness checks, and completed three full
10,000-transfer runs in **47.019 s**, **46.756 s**, and **46.676 s** (the last
using code-epoch validation). Nearby unchanged controls took **46.742 s**
and **48.322 s**. The code-epoch run had 17,804,958 NPT faults and about
2,720 preparation cycles per exit, versus 17,821,678 and about 2,680 for the
46.742 s control. This cache has no reproducible whole-workload gain, so its
source stays in isolated worktrees rather than the shared branch. Logs and
fair-marker snapshots are under ignored
`target/boxctl-evidence/amd-globalcompact-1012/`.

An exact-context full-plan cache then reused a plan only when the current
virtual page, all eight recent virtual pages, and the predicted successor
matched its saved context. It also rechecked guarded code, tree generation,
and NPT mapping generation. All 267 VMX library tests, the in-box SVM suite,
and 10,000/10,000 transfers passed. NPT faults returned near baseline at
**17,943,598**, but preparation rose to about **2,810 cycles per exit** and
the fair interval took **47.622 s**, versus the same-box baseline's 2,680
cycles per exit and 46.742 s. Context checking costs more than the plans it
can reuse. The candidate remains isolated in
`/tmp/bedrock-amd-globalcompact-1012`; its logs and fair-marker snapshots
share the ignored evidence directory above. The pushed module was restored
on the box and passed the full in-box SVM suite.

For the next exit-count reduction, an execute-only shadow code mapping is not
available on this NPT path. AMD NPT uses a present bit for readable mappings
and an independent NX bit to deny execution; it does not provide an EPT-like
execute-only permission. Bedrock's `AmdNpt` permission encoder reflects this:
any nonempty mapping sets present, and execute permission clears NX. Thus an
executable patched shadow page would also expose patched bytes to guest data
reads. This rules out that shortcut without additional read virtualization.
See the [AMD64 APM volume 2, nested-paging section](https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2)
and `crates/bedrock-ept/src/entry.rs`.

### Check counted-entry VMCB PMU writes

An isolated candidate stopped clearing virtual PMU controls and counters 1–5
on every counted entry. Bedrock arms only counter 0, starts from a zeroed
VMCB, and intercepts guest PMU MSRs. The 23 `bedrock-svm` and 266
`bedrock-vmx` library tests, full in-box SVM suite, and 10,000/10,000 Tempo
transfers passed. On the same EPYC box, the candidate interval took
**47.282 s**, with 22,144,408 exits, 17,828,536 NPT faults, and **60.86
billion** VM-entry preparation cycles. The adjacent unchanged control took
**47.674 s**, with 22,477,753 exits, 18,099,573 NPT faults, and **60.80
billion** preparation cycles. The wall-time difference tracks different
exit counts; there is no measured preparation saving. The candidate stays
isolated in `/tmp/bedrock-amd-pmu-lean-1012`; logs and fair-marker snapshots
are under ignored `target/boxctl-evidence/amd-globalcompact-1012/`. The
unchanged module is restored in the box and passed its SVM suite.

### Cache only a global plan's relevant recent-page prefix

A full-plan cache that checked only the recent-page prefix visited before the
four-slot planner stopped passed 267 VMX library tests, the in-box SVM suite,
and 10,000/10,000 transfers. On the same EPYC 4245P box, it took **42.662
s**, with 16,831,930 NPT faults and about 2,619 VM-entry preparation cycles
per exit; the unchanged control took **41.810 s**, with 16,658,500 faults and
about 2,525 cycles per exit. A narrower variant stored only plans whose
breakpoint capacity filled before scanning all eight recent pages. It also
passed the checks and transfers, taking **42.075 s**, with 16,630,066 faults
and about 2,609 preparation cycles per exit. Both cache variants added entry
work and neither improved elapsed time. They remain isolated in
`/tmp/bedrock-amd-globalcompact-1012`; the first variant is preserved as
`/tmp/bedrock-amd-prefix-context-1012.patch`. Logs and marker snapshots are
under ignored `target/boxctl-evidence/amd-prefixcache-1012/`.

The first replacement `m4-metal-small` box had an EPYC 4244P whose SVM PMU
initialization failed, so it could not run the accelerated path or provide a
valid comparison. It was deleted. The runs above used a new EPYC 4245P box
with HWE 7.0.0-38 and a passing SVM suite. CPU model and PMU availability
must be checked on each provisioned box before interpreting performance.

### Classify missed recent pages at global execute faults

A one-in-64 diagnostic sampled global NPT execute faults during a complete
10,000-transfer run. Nearest million-fault snapshots around the fair markers
span about **16 million** global execute faults and **250,000** samples. About
**172,000 (69%)** faulted at a virtual page in the eight-page recent set;
roughly **148,000 (86% of those recent misses)** had all four breakpoint
registers occupied. About **17,000 (10%)** had apparent space when comparing
the target page's raw hazard count to free slots; this is only an upper bound
because aliases and page-boundary rules can need more breakpoints. The
remaining recent misses had a raw-count overflow or no guarded hazard memo.

A follow-up run split the apparent-fit bucket. Only **309 of 17,178** sampled
apparent-fit misses were stopped by the planner's raw-hazard cutoff, and only
**1,348** had zero breakpoints on the current plan (the trusted-current-page
path). Thus removing the raw cutoff or widening only trusted-page plans has
limited headroom on this workload. The diagnostic sources stay isolated in
`/tmp/bedrock-amd-faultalloc-1012`, with logs under ignored
`target/boxctl-evidence/amd-faultalloc-1012/`.

A third complete diagnostic run checked cached singleton alias proofs for
the apparent-fit faults. Across the nearest fair-marker snapshots it took
**41.806 s** and sampled 250,000 of 16 million global execute faults. Of
**17,181** apparent-fit samples, **17,152** had a cached alias proof whose
breakpoints would fit the remaining slots, **29** had no matching proof, and
**zero** exceeded the slot count. These figures rule out alias slot overflow
as the main explanation for the missed pages, but they do not establish that
the pages could safely be admitted: the planner also checks code translation,
page-boundary compatibility, and other guarded-tree conditions. The next
diagnostic compares the faulting physical page with the translation of the
faulting guest RIP before interpreting recent virtual-page rank.

That physical-page check completed all 10,000 transfers in **41.780 s**. Of
**16,779** sampled apparent-fit faults, **10,385** had a fault GPA matching
the current RIP's physical page, while **6,394** did not. The virtual-page
rank alone therefore overstates candidates for admitting the faulted page
into the current global guard. Even among the matching cases, the planner's
boundary and translation checks still need to be classified before making
a change. This was another one-in-64 diagnostic, not an optimized build.

### Count alias-proof work in the complete transfer phase

On a fresh EPYC 4245P box, a diagnostic counter build completed all 10,000
transfers in **42.200 s**. The nearest million-call snapshots around the fair
markers span 17 million `collect_page_breakpoints` calls: **16,950,670
cache hits (99.71%)** and **49,330 full alias walks (0.29%)**. All walks in
that interval succeeded, with no breakpoint, workspace, or read failures.
The cache is therefore doing its job; the repeated lookup and global-plan
work, along with roughly 17 million NPT execute faults, are the remaining
plausible costs. A follow-up samples host TSC cycles inside these entry-path
functions to quantify their share before changing them. The diagnostic
source is isolated in `/tmp/bedrock-amd-collectstats-1012`; copied logs are
under ignored `target/boxctl-evidence/amd-collectstats-1012/`.

Two production-module `perf` attempts on the previous box did not yield a
usable function profile. Hardware `cycles:k` produced only four samples
while the guest PMU was active. Software `cpu-clock` produced about 2,000
samples, but almost all landed at the module's interrupt-enable boundary,
where the software timer becomes deliverable. Both runs completed the
10,000-transfer workload in **41.376 s** and **42.483 s** respectively;
neither sampled distribution is evidence of the hot function.

A one-in-256 host-TSC timer on the same replacement box completed the full
workload in **42.055 s**. Nearest fair-marker snapshots show **20.98 million**
global-plan preparation calls, averaging about **1,279 host cycles** in
81,948 samples. That is an estimated **26.8 billion cycles** in the preparation
function, including alias-proof collection, versus **51.1 billion** measured
VM-entry preparation cycles for the interval. Within collection, 66,213
sampled cache hits averaged **103 cycles**; 194 sampled misses averaged
**127,082 cycles**. The snapshots include **16.95 million hits** and **52,398
walks**. The expensive walks are rare, but their estimated aggregate cost is
around **6.7 billion cycles**; speeding them up could save a couple of host
seconds. These are sampled estimates, not a controlled speedup measurement.

An isolated candidate used the existing guarded-table hash index in the
upper levels of those alias walks. All 266 VMX library tests, including an
indexed collision/fallback check, and the in-box SVM suite passed. A full
same-box candidate run took **41.643 s**; the adjacent unchanged control took
**42.328 s**. The candidate had **16.417 million** NPT faults versus the
control's **16.770 million**, so the elapsed difference tracks guest-path
variation. Measured VM-entry preparation was **51.69 billion cycles** for
the candidate and **51.77 billion** for the control, showing no meaningful
reduction in the intended cost. The change remains isolated in
`/tmp/bedrock-amd-alias-upperindex-1012`, with logs under ignored
`target/boxctl-evidence/amd-upperindex-1012/`.

A separate isolated candidate grew the set-associative alias-proof cache
from 2,048 to 8,192 entries and its direct lookup from 1,024 to 4,096.
It passed all 266 VMX library tests, the in-box SVM suite, and two complete
10,000-transfer runs on the same EPYC 4245P box. Fair intervals were
**41.640 s** and **42.966 s**, versus **42.328 s** for the adjacent unchanged
control. The candidate's NPT fault counts were **16.625 million** and
**16.759 million**, versus the control's **16.770 million**. Its VM-entry
preparation cycles moved from **50.73 billion** to **54.21 billion** across
those two runs, versus **51.77 billion** for the control. A larger cache
did not yield a repeatable application gain, so this 1.6 MiB-per-VM
expansion remains isolated in `/tmp/bedrock-amd-alias-capacity-1012`.

A recency-first page-selection probe moved the predicted successor behind
the eight recent virtual pages when filling a four-breakpoint global plan.
It passed all 266 VMX library tests and the in-box SVM suite, then completed
all 10,000 transfers in **42.643 s**. The same-box unchanged control took
**42.328 s**. The candidate caused **17.485 million** NPT faults, up from
the control's **16.770 million**; giving the predictor first choice is useful
on this workload. The recency-first policy remains isolated in
`/tmp/bedrock-amd-recent-first-1012`, with logs under ignored
`target/boxctl-evidence/amd-recentfirst-1012/`.

An additional probe tried the predicted successor's own predicted next page
before recent pages. It passed all 266 VMX library tests and the in-box SVM
suite, then completed 10,000 transfers in **42.400 s**. NPT faults were
**16.731 million**, nearly the unchanged control's **16.770 million**;
VM-entry preparation rose from **51.77** to **52.24 billion cycles**. There
was no application gain. This variant remains isolated in
`/tmp/bedrock-amd-two-predict-1012`, with logs under ignored
`target/boxctl-evidence/amd-twopredict-1012/`.

### Measure the four-breakpoint working-set limit

An isolated one-in-256 diagnostic counted the code pages and hardware
breakpoint slots in accepted global AMD entries. All 266 VMX library tests,
the in-box SVM suite, and the exact 10,000-transfer workload passed on the
same EPYC 4245P HWE 7.0 box as the adjacent **42.328 s** production control.
The diagnostic fair interval was **42.670 s**, with **16,842,167** NPT faults
and **1,463,765** synthetic trap exits. Kernel-log snapshots nearest the
transfer markers span 20 million accepted global entries, yielding 78,125
sampled plans:

| Executable hazardous pages | 0 slots | 1 slot | 2 slots | 3 slots | 4 slots | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1,897 | 327 | 217 | 1,276 | 5,329 | 9,046 |
| 2 | 0 | 4,284 | 3,678 | 700 | 14,838 | 23,500 |
| 3 | 0 | 0 | 0 | 482 | 35,720 | 36,202 |
| 4 | 0 | 0 | 0 | 0 | 9,377 | 9,377 |

**65,264/78,125 plans (83.5%)** used all four slots, while **69,079
(88.4%)** already admitted at least two hazardous pages. Thus the repeated
execute faults are not mainly caused by planning only the current page:
most entries already cover a multi-page working set up to the architectural
four-breakpoint limit. More cache capacity or simple recent-page reordering
cannot make a five-slot guard fit. A larger improvement needs to reduce the
number of simultaneously required exact hazard entries, avoid some
hazard-page crossings with a sound region proof, or use a different exact
stopping mechanism. These sampled plan counts do not show which specific
transition caused each subsequent fault. Diagnostic source remains isolated
in `/tmp/bedrock-amd-slotdiag-1012`; its reproducible instrumentation patch
is `diagnostics/slot-diag.patch`. Snapshots and kernel logs are under ignored
`target/boxctl-evidence/amd-slotdiag-1012/`.

An isolated follow-up added an early stop to the optional-page loop when
the current plan already occupied all four actual breakpoint slots. This
is safe because the planner rejects a candidate that adds no new breakpoint.
All 266 VMX library tests, the in-box suite, and 10,000/10,000 transfers
passed. The same-box candidate interval was **42.507 s**, versus **42.328 s**
for the preceding unchanged control; NPT faults were **16.867 million** versus
**16.770 million**. VM-entry preparation was **2,528 cycles per exit** versus
**2,519** for the control. Thus the extra condition did not reduce normalized
preparation cost or end-to-end time. The existing raw-hazard-count cutoff
likely catches most full-slot plans; this inference needs a direct hit count
before treating it as proven. The candidate remains isolated in
`/tmp/bedrock-amd-fullslot-1012`, with marker snapshots under ignored
`target/boxctl-evidence/amd-fullslot-1012/`.

### Bound global-page candidate search to four recent pages

The earlier fault-rank samples were concentrated at the front of the
eight-page recent list, while candidate screening was a substantial part
of global-plan preparation. An isolated variant kept the successor
prediction first but considered only the four newest recent pages for an
optional global guard. Older pages remain NX, so a missed transition faults
and replans before executing code. All 266 VMX library tests, the in-box SVM
suite, and the exact 10,000-transfer workload passed. The first 4245P box
gave a **40.784 s** candidate fair interval with **16.538 million** NPT
faults and **2,421 preparation cycles per exit**, versus its earlier
**42.328 s** unchanged control with **16.770 million** faults and **2,519
preparation cycles per exit**.

An alternating candidate/control/candidate series on a *fresh* EPYC 4245P
HWE 7.0 box then repeated the result. The same pinned guest kernel, initrd,
10,000-transfer image archive, compose file, seed, and guest RAM were used
for all three runs:

| Run | Recent candidates | Fair interval | VM exits | NPT faults | Preparation cycles/exit |
| --- | ---: | ---: | ---: | ---: | ---: |
| Candidate A | 4 | **40.619 s** | 20,239,208 | 16,574,469 | **2,402** |
| Control B | 8 | 42.052 s | 20,471,081 | 16,734,976 | 2,501 |
| Candidate C | 4 | **41.181 s** | 20,476,964 | 16,778,786 | **2,413** |

Candidate C had nearly the control's exit count and slightly more NPT
faults, yet still saved about **88 preparation cycles per exit** and **0.87
s** in the fair interval. The two candidates averaged **40.900 s**, about
**1.15 s (2.7%)** faster than the adjacent control; a single control and
natural guest-path variation limit precision. This is a measured gain on
the Tempo workload, not Intel parity. The four-page limit is retained in
`prepare_global_into`; marker snapshots and logs are under ignored
`target/boxctl-evidence/amd-recent4-1012/`.

A two-recent-page variant failed two existing planner coverage tests: it
could no longer guard the three- and four-hazardous-page cases those tests
require. It was not loaded. A five-recent-page variant passed all 266 library
tests, the in-box suite, and two full Tempo runs on the same second box.
Alternating the retained four-page module and five-page candidate gave:

| Order | Recent candidates | Fair interval | VM exits | NPT faults | Preparation cycles/exit |
| --- | ---: | ---: | ---: | ---: | ---: |
| C | 4 | 41.181 s | 20,476,964 | 16,778,786 | 2,413 |
| D | 5 | 40.458 s | 19,821,062 | 16,214,828 | 2,490 |
| E | 4 | 41.538 s | 20,534,909 | 16,797,063 | 2,426 |
| F | 5 | 41.439 s | 20,383,869 | 16,663,587 | 2,463 |

Five pages sometimes admitted enough code to reduce exits, but its two
runs spent **37–77 more preparation cycles per exit** than adjacent
four-page runs. The five-page elapsed results straddle the earlier four-page
results, so this small sample does not establish a better end-to-end policy.
The proven four-page improvement remains retained. Five-page source is
isolated in `/tmp/bedrock-amd-recent5-1012`; snapshots and logs are under
ignored `target/boxctl-evidence/amd-recent5-1012/`.

### Retain guarded leaf alias filters across CR3 switches

With the four-recent-page policy, a diagnostic measured about **54 thousand**
full alias-proof walks during the transfer. A direct-mapped ghost record
found only about **160 cache evictions** and **1.5 thousand** later walks for
recently evicted keys, so replacing the four-way proof-cache policy has
little headroom. A separate diagnostic counted roughly **1.6 million**
transfer-phase scans of leaf tables already seen before, after their active
Bloom filter had been cleared. The diagnostics completed all 10,000
transfers; their counters and marker snapshots are under ignored
`target/boxctl-evidence/amd-aliasmiss-1012/`.

The AMD alias walker now retains each guarded leaf table's executable-page
Bloom filter by physical table page and NPT mapping generation. It reuses
the filter only while that leaf's active NPT write guard is valid. Guest
table-write release and host writes revoke the matching filter; dropping all
table guards revokes the cache. A mapping-generation change also prevents
reuse. All 267 VMX library tests, including host-write and guest-write
filter invalidation checks, the in-box SVM suite, and two full 10,000-transfer
runs passed on the same EPYC 4245P HWE 7.0 box:

| Run order | Leaf-filter cache | Fair interval | VM exits | NPT faults | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| Candidate | Retained | **40.118 s** | 20,000,556 | 16,410,573 | **2,358** |
| Control | Active tree only | 41.454 s | 20,638,279 | 16,904,527 | 2,399 |
| Candidate repeat | Retained | **40.710 s** | 20,382,873 | 16,694,473 | **2,347** |

The candidate saved **41–52 preparation cycles per exit** in both runs.
Its average **40.414 s** interval was about **1.04 s (2.5%)** below the
adjacent control. Exit counts also varied, so the normalized preparation
cost is the clearest evidence of this cache's direct effect. This remains
far from the historical Intel 24.087 s workload timing.

### Hide AMD guest ERMS to reduce optional REP entries

The shared CPUID handler passed host ERMS (leaf 7, subleaf 0, EBX bit 9) to
both guests. It already clears leaf 7 EDX, including FSRM. On AMD the guest
now sees ERMS clear, so Linux and CPUID-dispatched user libraries can choose
copy routines with fewer REP entries. Intel guest CPUID remains unchanged.

The exact hot user page with `REP MOVSQ` at offsets `0x3e7` and `0x5f4`
belongs to `/usr/local/bin/tempo` in the pinned `tempo-localnet` image: its
ELF file has the corresponding bytes at offsets `0x7a9a3e7` and
`0x7a9a5f4`. These are embedded in the application executable, rather than
the Alpine/musl check image. Hiding ERMS cannot remove their bytes from the
page. The guest kernel's `memcpy` likewise has an ERMS `rep movsb` path and a
non-REP fallback on the same code page; its unused REP bytes still need a sound
guard unless the reachable-region proof excludes them.

An alternating ERMS-hidden/control/hidden/control series used the same
EPYC 4245P HWE 7.0 box, pinned guest and images, seed, 10,000 transfers,
CPU affinity, and fair markers. All four runs passed:

| Order | Guest ERMS | Fair interval | VM exits | NPT faults | Synthetic trap exits | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| A | Hidden | 41.050 s | 20,598,121 | 17,117,876 | 1,194,335 | 2,313 |
| B | Exposed | 41.464 s | 20,741,500 | 16,973,824 | 1,487,062 | 2,350 |
| C | Hidden | 39.491 s | 19,649,458 | 16,327,837 | 1,173,673 | 2,365 |
| D | Exposed | 40.534 s | 20,323,836 | 16,655,451 | 1,457,163 | 2,337 |

Hiding ERMS reduced synthetic trap exits by about 20% in both comparisons.
The two hidden runs averaged **40.270 s** versus **40.999 s** for exposed,
about **0.73 s (1.8%)** faster. NPT fault and total-exit counts varied, so
this is a modest workload-specific result rather than a precise CPUID speedup
or a route to the Intel 24.087 s interval. The mask is retained on AMD;
marker snapshots are under ignored `target/boxctl-evidence/amd-erms-1012/`.
The 267 local VMX tests and the in-box SVM suite passed with the mask.

### Reject two small follow-up candidates

Hiding RDRAND (leaf 1 ECX bit 30) in addition to ERMS passed the 267 local
tests, the boxctl SVM suite, and all 10,000 transfers. Its fair interval was
**40.974 s**, with **16,961,832** NPT faults and **1,198,906** synthetic trap
exits. The earlier ERMS-only run had just **four** RDRAND exits during its
transfer; the hidden-RDRAND run had zero. That feature mask cannot account for
the remaining performance gap, so it was discarded.

Another isolated candidate let the global page planner use actual occupied
breakpoint slots, rather than the raw number of hazardous offsets, when
deciding whether to try another page. This is safe only when the additional
page's independent cached alias proof fits the remaining slots. It passed
the same library and boxctl suites and all 10,000 transfers, but the adjacent
same-box comparison favored the unchanged planner:

| Variant | Fair interval | VM exits | NPT faults | Preparation cycles/exit |
| --- | ---: | ---: | ---: | ---: |
| Slot-based admission | 40.710 s | 20,197,099 | 16,807,889 | 2,387 |
| Unchanged planner | **39.867 s** | 19,886,493 | 16,545,298 | **2,344** |

Allowing the extra screening raised preparation cost without reducing page
faults in this run. The candidate was discarded. Its source patch is kept at
`/tmp/bedrock-amd-slotfit-1012.patch`; marker snapshots are under ignored
`target/boxctl-evidence/amd-slotfit-1012/`. The production branch remains
the ERMS-only CPUID change.

The unchanged run's marker counters attribute **94.02 billion** host TSC
ticks to the VM runner plus guest, **46.62 billion** to entry preparation,
**8.54 billion** to exit bookkeeping, and **0.97 billion** to the host IRQ
window during its 39.867 s fair interval. At about 19.9 million exits,
the next high-value work is a sound way to avoid repeated guarded-page
crossings or to reduce the per-exit entry path. The RDRAND and relaxed
slot-admission experiments do neither on this workload.

### Recheck the hot-page and planner recommendations

The guest logs `Disabled fast string operations` on this AMD box, including
before the ERMS CPUID mask. `IA32_MISC_ENABLE` is read from the host or
defaults to zero when unavailable, so the suggestion that the kernel had
already disabled optional fast-string dispatch is supported. The retained
ERMS mask can still change user-library dispatch and reduced synthetic trap
exits in the A/B/A/B comparison above. It cannot remove unconditional REP
instructions from either the guest kernel or the pinned Tempo executable.

The proposed three-hazard breakpoint mask does not explain the hot page's
current behavior. The existing `add_page_breakpoint` already packs five
hazards into four masked AMD debug slots. More recently, the reachable-region
proof has usually needed fewer exact slots on that page. Combining three
hazards into one 1 KiB range might release another slot for a second page,
but would enlarge the range of harmless #DB exits; earlier 32-byte packing
and slot-based admission increased planning cost enough to erase their fault
savings. An earlier diagnostic found 1,230 of 1,840 sampled trap-flag exits
(66.8%) on this page. A later byte-identified diagnostic, after further
planner changes, found 28 of 69 sampled synthetic exits (40.6%) there.
Those samples have different denominators and code versions; the later
synthetic exits also include masked breakpoint hits rather than only
hardware trap-flag steps. The proposed mask remains an untested tradeoff,
not an obvious missing correctness fix.

The recommended 200,000-instruction forced-preemption test is useful, but its
proposed mechanism is wrong for the current DST implementation. Forced
preemption is checked at the first deterministic exit at or after its target;
`preempt_deadline` is deliberately excluded from the precise PMU target.
Consequently it does not force a 256-instruction exact-deadline approach on
each preemption. The `200k` workload run below measures the actual behavior.
Changing guest HZ or adding a second exact overflow would target a different
cost and should not follow merely from this preemption setting.

The REP-as-one-tick proposal has not been validated as a compatible change.
An unguarded 5,000-iteration `REP STOSB` already showed AMD IRPERF counting
the entire REP as one instruction and missing an exact stop at iteration
900. The existing guarded path stopped exactly there. Redefining the virtual
clock would change deadline and interrupt placement and would need a matching
Intel/reference definition plus interruption tests. We have not tested a
helper planning core, kernel REP removal, or guest huge-page layout. A
layout-only guest-kernel change is measured below. It changes the common
guest environment while keeping the pinned Tempo binary under test intact.

An isolated, low-rate in-kernel planner probe then passed the box SVM suite
and completed all 10,000 Tempo transfers. The fair interval was **42.576 s**
with probe overhead, so it is not a candidate speed result. Marker-bounded
snapshots cover 21 million `prepare_global_into` calls, with one in 256 timed.
Approximate TSC cycles per sampled call were:

| Planner part | Cycles/call |
| --- | ---: |
| Current-page hazard lookup | 86 |
| Region proof | 5 |
| Breakpoint collection | 351 |
| Optional-page widening, including nested work | 732 |
| Widening: translation | 74 |
| Widening: other-page hazard lookup | 162 |
| Widening: alias proof | 68 |

The widening loop considered about 3.32 candidate pages per sampled call.
The nested subtasks account for roughly 304 of its 732 cycles; the remaining
time includes candidate selection, control flow, and probe overhead. This
narrows the next software target to the optional-page widening path, but a
candidate must reduce preparation cost without increasing protected-page
faults. Diagnostic source is isolated in `/tmp/bedrock-amd-phasecurrent-1012`;
marker snapshots and kernel logs are under ignored
`target/boxctl-evidence/amd-widenphase-1012/`.

On that same instrumented build and box, setting
`BEDROCK_PREEMPT_PERIOD=200000` and `BEDROCK_PREEMPT_SEED=12345` also passed
all 10,000 transfers. The no-preemption run's fair interval was **42.576 s**
with **20,291,514** exits, including **16,870,803** NPT faults and
**1,179,269** synthetic trap exits. The 200k run took **43.053 s** with
**20,575,829** exits, including **16,926,813** NPT faults and **1,266,020**
synthetic trap exits. The 0.477-second, 1.1% difference is one adjacent
comparison with changed guest scheduling, so it is not a pure overhead
estimate. It does show that this setting did not cause an obvious 256-step
per-preemption cliff. Marker snapshots are in the same ignored evidence
directory.

After both diagnostic runs, `contrib/run-boxctl-svm.sh test` restored the
production branch's module on the disposable box. The complete in-box SVM
suite passed, including exact deadlines, masked breakpoints, fork CoW,
replay, and both native-loop comparisons.

### Test predicted-page deduplication and classify widening outcomes

The recent-page list is unique, but its predicted successor can also occur
among those recent pages. An isolated candidate skipped the second screening
of that virtual page after prediction had priority. It passed all 267 VMX
library tests, the in-box SVM suite, and three full Tempo runs (one on the
preceding box). Its two same-box comparisons on a fresh EPYC 4245P HWE box
used the identical pinned guest artifacts and CLI:

| Order | Variant | Fair interval | VM exits | NPT faults | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Unchanged | **40.265 s** | 19,800,506 | 16,468,312 | 2,398 |
| 2 | Deduplicate | 41.067 s | 20,448,527 | 16,972,008 | 2,321 |
| 3 | Unchanged | **40.729 s** | 20,256,026 | 16,852,962 | 2,340 |
| 4 | Deduplicate | 41.798 s | 20,537,820 | 17,038,923 | 2,407 |

The candidate was slower in both pairs, and the normalized preparation
advantage in the first pair did not repeat. It was discarded. Source remains
isolated in `/tmp/bedrock-amd-dedup-1012`; fair-marker JSON is under ignored
`target/boxctl-evidence/amd-pred-dedup-1012/`.

A second isolated diagnostic classified each optional-page candidate at one
in 256 `prepare_global_into` calls. It passed the 267 VMX tests, the in-box
SVM suite, and all 10,000 Tempo transfers. The diagnostic interval was
**42.613 s** with instrumentation overhead. Marker-bounded kernel counters
span 20 million planner calls, about 78,125 sampled calls:

| Outcome | Count | Per sampled call |
| --- | ---: | ---: |
| Additional hazardous page admitted | 98,848 | 1.265 |
| Raw hazards exceed four slots | 39,218 | 0.502 |
| Empty or current recent-page entry | 50,566 | 0.647 |
| Previously trusted page | 20,509 | 0.263 |
| Already selected page | 7,797 | 0.100 |
| Missing alias proof | 86 | 0.001 |
| Predicted page repeated among recent pages (overlaps outcomes above) | 15,318 | 0.196 |

The loop also stops early when the plan fills its slots or page capacity;
the outcomes therefore do not sum to all candidate iterations. No sampled
candidate failed the page-boundary check. Widening is frequently productive,
so simply truncating the loop would sacrifice page coverage. The four-slot
limit and cost of screening admitted pages matter more than duplicate
prediction. Diagnostic source is isolated in
`/tmp/bedrock-amd-phasecurrent-1012`; marker snapshots and kernel logs are
under ignored `target/boxctl-evidence/amd-plan-outcomes-1012/`.

The unchanged production module was restored on the box afterward and the
full in-box SVM suite passed.

### Remove unused SVM profiling atomics in an isolated build

The production `svm_batch.rs` still contained 39 write-only profiling
atomics, including two relaxed increments on most current-page breakpoint
proof calls. An isolated build removed those atomics and the now-empty
diagnostic branches without changing planning decisions. All 267 VMX tests,
the in-box SVM suite, and two complete 10,000-transfer Tempo runs passed.
On the same EPYC 4245P HWE box:

| Order | Variant | Fair interval | VM exits | NPT faults | Preparation cycles/exit |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | No profiling atomics | 40.404 s | 20,084,192 | 16,684,849 | 2,380 |
| 2 | Unchanged control | 40.640 s | 20,134,989 | 16,725,817 | 2,367 |
| 3 | No profiling atomics | 40.456 s | 20,140,356 | 16,729,961 | 2,365 |

The nearly identical exit counts in the final pair make its 0.184-second
candidate lead a useful bound, but the measured preparation difference was
only about two cycles per exit. The first candidate had *higher* normalized
preparation cost than the control. The edit does not demonstrate a meaningful
Tempo speedup and remains isolated in `/tmp/bedrock-amd-no-counters-1012`.
Fair-marker JSON is under ignored
`target/boxctl-evidence/amd-no-counters-1012/`. The production module was
restored on the box and passed the in-box SVM suite afterward.

### Isolate non-REP kernel copy fallbacks from REP hazards

The guest reports `Disabled fast string operations`, so it calls the
non-REP `memcpy_orig` and `memset_orig` fallbacks. In the unmodified Linux
6.18 guest image, `memcpy_orig` shares a 4 KiB code page with unused REP
alternatives. The page-wide hazard proof still guards that page. A guest
kernel patch aligns both non-REP fallbacks to separate 4 KiB pages and pads
after them. Disassembly of both resulting pages found no REP or RNG
instructions. The Bedrock module, initrd, workload archive, seed, and box
were unchanged. The patched `guestKernel` built successfully with Nix.

On `box-7950ba17` (EPYC 4245P, HWE 7.0.0-38), all five complete
10,000-transfer runs passed. Fair-marker intervals and counters were:

| Order | Guest kernel | Fair interval | VM exits | NPT faults |
| --- | --- | ---: | ---: | ---: |
| 1 | Control | 41.979 s | 20,706,176 | 17,184,650 |
| 2 | Aligned fallbacks | 39.236 s | 19,121,342 | 15,740,346 |
| 3 | Control | 40.513 s | 20,038,648 | 16,659,141 |
| 4 | Aligned fallbacks | 40.754 s | 19,987,040 | 16,429,843 |
| 5 | Aligned fallbacks | 39.021 s | 19,088,361 | 15,707,726 |

The layout cut NPT faults in each adjacent comparison, but runtime still
varied: the second candidate was 0.24 seconds slower than the preceding
control. Candidate median was 39.236 seconds versus 41.246 seconds for the
two controls' midpoint; that small sample is not a confidence interval.
This patch affects the shared guest kernel used by both backends and changes
code addresses, so old replay traces for the previous image cannot be used
as a comparison. Marker snapshots are under ignored
`target/boxctl-evidence/amd-guest-layout-1012/`.

### IRPERF / unguarded-REP ceiling probe (2026-10-12)

This diagnostic used a disposable EPYC 4245P `boxctl` box with HWE
`7.0.0-38-generic`; no module was built or loaded on the workstation. The
guest was the aligned-fallback kernel above, with the same 10,000-transfer
image, seed, CPU pin, and fair markers. Experimental source remains in the
isolated `/tmp/bedrock-amd-irperf-probe-1012` worktree and was **not merged**.

| Diagnostic variant | Fair 10,000-transfer interval | VM exits | NPT faults | Replay result |
| --- | ---: | ---: | ---: | --- |
| Guarded control, AMD RDRAND CPUID masked, original counter correction | 38.975 s | 19.073 million | 15.734 million | 128/128 fork replays passed; separate guarded run with revised correction passed 1,024/1,024 |
| Normal page validation, REP omitted from hazard scanner, raw IRPERF | 13.324 s and 13.432 s | 5.428 million in first run | 2.387 million in first run | Failed 128-fork replay even with REP guarded when all PMC correction was removed |
| Normal page validation, REP omitted, selective PMC correction | Not timed | Not timed | Not timed | 128/128 passed; 1,024-fork replay diverged at replay 59 |
| Treat every code page as safe, raw IRPERF, REP unguarded | 7.545 s | 2.879 million | 0.103 million | Unsafe: bypasses runtime code and RNG guards |
| Same all-safe probe with AMD RDRAND CPUID hidden | 7.522 s | 2.893 million | 0.109 million | Unsafe: CPUID hiding does not intercept raw RDRAND |

All timed runs completed 10,000/10,000 transfers, but that application check
does not establish deterministic replay. The 7.5-second variant deliberately
disabled runtime hazard enforcement and is only an upper-bound diagnostic.
The 13.3-second variant retained the other page hazards, but omitting REP is
also not correctness-ready. The current guarded code is the only timed row
that preserves the existing REP policy. These intervals are on the same box;
the historical Intel interval of 24.087 seconds was on different hardware.

The proposed clock uses VMCB IRPERF as the guest retirement count, treating
an entire REP instruction as one retired instruction rather than accounting
for each repeated iteration as the current stepped path does. This changes
virtual-time deadlines and replay checkpoints. AMD RDRAND CPUID masking
steers optional callers away from hardware RNG but does not make raw RDRAND
safe. The page hazard scanner must remain unless an equally strong runtime
mechanism replaces it.

A 16 MiB REP STOSB probe with host IBS sampling showed why the existing
single-tick PMC correction cannot simply be reused for unguarded REP: its
virtual count changed from 111 without IBS to 299--306 with IBS. Removing
the correction kept this narrow probe at 111 with and without IBS, but Linux
fork replay then diverged at replay 13 (and at replay 27 with REP guarded).
Excluding host-interrupt exits from the correction preserved 1,024 replays
with REP guarded; with REP unguarded, it still diverged at replay 59. The
remaining work is a REP-aware interruption/retirement accounting rule that
passes long fork replay and heavy host sampling before this speedup can be
considered safe. Fair-marker snapshots for the timed runs are in ignored
`target/boxctl-evidence/amd-irperf-ceiling-1012/`.

### REP-aware IRPERF replay experiment (2026-10-13)

An isolated candidate on a disposable EPYC 4245P box (HWE 7.0.0-38) made
F3 MOVS/STOS eligible for bounded native execution, used IRPERF as a
one-retirement-per-completed-REP clock, excluded host-interrupt exits from
the one-tick PMC correction, and masked AMD RDRAND in guest CPUID. Normal
code and page-table guards stayed enabled. A page-breakpoint replay initially
looped at a REP entry with no retirement; forcing one scalar replay after the
breakpoint fixed the cached-code-write test. The extra NPT write-replay change
tried during diagnosis was removed because it did not fix that loop.

The final candidate and the current guarded branch ran the same 10,000-transfer
guest, seed, CPU pin, and fair markers on the *same* box:

| Module | Fair interval | VM exits | NPT faults | Application result |
| --- | ---: | ---: | ---: | --- |
| Current guarded branch | 39.949 s | 19.671 million | 16.187 million | 10,000 successes, zero failures/reverts |
| Candidate after breakpoint fix | 20.418 s | 8.723 million | 5.575 million | 10,000 successes, zero failures/reverts |

The candidate's 19.744-second pre-fix run is excluded from the final comparison
because its cached-code-write test hung. The fixed candidate is **1.96x faster**
than the guarded branch in this workload. It is numerically below the
historical Intel 24.087-second result, but that was on different hardware and
the candidate changes REP virtual-clock semantics, so this is not an equivalent
cross-vendor result.

The candidate is **not safe to merge**. Its 16 MiB REP deadline probe was
stable across 50 runs, and the cached-code-write test passed after the fix.
One 1,024-fork replay passed before the fix, but the final candidate diverged
at replay 386. Existing AMD REP deadline tests also fail because they require
each iteration to advance virtual time; the candidate advances the clock only
when the whole REP instruction retires. These clock semantics need an explicit
decision and updated tests, followed by repeated long replay and host-interrupt
stress, before using the fast path. The experimental code remains only in
`/tmp/bedrock-amd-irperf-probe-1012`; raw logs and marker snapshots are in
ignored `target/boxctl-evidence/amd-unified-rep-1013/`. No module was built or
loaded on the workstation.

### Guarded REP retirement follow-up (2026-10-13)

The preceding fast candidate was unsafe because a global gate could enter a
REP in the middle of a batch, then mistake a completed REP followed by a loop
back to the same RIP for an unfinished REP. The follow-up rejects a global
batch *starting* at REP and keeps REP in the page-hazard scanner. At a REP
hazard breakpoint it uses the bounded native executor; a completed REP advances
the virtual instruction clock once, and a partial REP advances it zero times.
The AMD deadline tests were updated to this architectural retirement rule.
The AMD RDRAND CPUID mask used during diagnosis was removed.

On the disposable EPYC 4245P/HWE box, the guarded follow-up passed the cached
code-write REP test, REP deadline and RF tests, 17 transition checks, 25 bench
checks, and three independent 1,024-fork Linux replay runs against a scalar
reference. The 10,000-transfer Tempo run completed with 10,000 successes and
zero failures/reverts. Its fair-marker interval was **39.814 s**, 19.556 million
VM exits, and 16.100 million NPT faults. The same-box guarded branch measured
39.949 s, 19.671 million exits, and 16.187 million NPT faults: the interval is
only 0.3% shorter, within run-to-run noise. The earlier 20.418-second candidate
therefore does not establish a usable speedup. The historical Intel interval
was 24.087 s on different hardware, so this guarded AMD result remains roughly
1.65x slower as an indicative, non-equivalent comparison.

Host IBS sampling via `perf record -e ibs_op//` caused replay divergence in
both the guarded branch and the experimental follow-up. The 3×1,024 replay
passes establish correctness only without concurrent host IBS sampling; that
stress failure is unresolved. The guarded follow-up stays in the isolated
`/tmp/bedrock-amd-irperf-probe-1012` worktree because it has no measured Tempo
benefit and changes virtual-clock semantics. Logs and marker snapshots are in
ignored `target/boxctl-evidence/amd-rep-guard-2026-10-13/`.

### Sample actual masked-breakpoint false hits (2026-10-13)

A disposable box ran the complete 10,000-transfer Tempo workload with one in
256 harmless breakpoint retries logged, including the page's guarded hazard
set and chosen breakpoint masks. The corrected diagnostic run had 979 samples
in the transfer phase; all 10,000 transfers succeeded. The largest page had
577 samples. Its seven hazards were `0x311`, `0x374`, `0x4d4`, `0x854`,
`0x888`, `0xaeb`, and `0xfcc`. The four slots used a `0x7f` mask at `0x300`,
exact breaks at `0x4d4` and `0xfcc`, and a `0x3ff` mask at `0x800`. The hot
false-hit offsets around `0x9c0` through `0xa22` lie in that last range.
With the existing 1 KiB mask limit and four slots, splitting the three
hazards in `0x800`–`0xbff` needs a fifth slot; choosing a different pair alone
cannot remove the hot range.

The five-hazard REP page had 290 samples, but its plan used **exact**, unmasked
reachable-region breakpoints at ordinary offsets such as `0x457`, `0x46d`, and
`0x57b`. Those are proof-frontier stops, not REP entries or masked-range false
hits. This distinguishes its remaining exits from the seven-hazard page's
masked hits. Other pages accounted for 112 samples. The first diagnostic build
had its info logger compiled out; a second run on the replacement box used a
basic compose file without txgen. Neither is used for these counts. The
corrected run and raw samples are in ignored
`target/boxctl-evidence/amd-falsehit-hazards-1013/`.

An isolated candidate extended the existing bounded reachability proof to
pages with at least five hazards, including the seven-hazard page. A synthetic
seven-hazard regression test, 267 VMX library tests, and the full in-box SVM
suite passed. The complete Tempo candidate and adjacent unchanged control on
the same box both completed 10,000 transfers with zero failures:

| Variant | Fair interval | VM exits | NPT faults | Synthetic MTF exits |
| --- | ---: | ---: | ---: | ---: |
| Reachability on pages with ≥5 hazards | 39.389 s | 19,575,586 | 16,130,519 | 1,187,956 |
| Unchanged branch | 39.435 s | 19,576,056 | 16,130,123 | 1,207,807 |

The candidate removed about 20,000 synthetic MTF exits, but total exits and
elapsed time were effectively unchanged. It is not retained on the shared
branch. Its source remains in `/tmp/bedrock-amd-region7-1013`; logs and marker
snapshots are in ignored `target/boxctl-evidence/amd-region7-1013/`. All module
builds and loads stayed inside the boxctl boxes.

A follow-up low-rate counter run on the seven-hazard candidate completed the
same 10,000 transfers. At the final sampled report, it had tried the
seven-hazard reachable-region proof 393,216 times and used it 308,213 times
(78.4%). Across all region proofs, the 256-entry work queue overflowed only
62 times and the outgoing-edge array never overflowed. Raising those capacities
is therefore unlikely to recover the missing speed. The lack of a wall-time
gain despite frequent successful proofs suggests that the remaining proof
frontier stops or the much larger NPT-fault volume dominate. This is an
inference; a candidate-side false-hit histogram would distinguish the two.
The diagnostic dmesg and fair markers are in the same ignored evidence folder.

The candidate-side false-hit sampler explained the cancellation. The
seven-hazard page fell from 577 sampled hits in the unchanged run to 86 in
the candidate run. But a six-hazard page gained 502 sampled exact proof-frontier
hits, 467 at ordinary offset `0x9c0`. The broad proof therefore moved hot
breakpoints rather than removing enough of them. Both samples were one in 256
harmless retries on complete 10,000-transfer runs; guest page placement and
execution mix can vary between runs.

The six-hazard page with the added `0x9c0` frontier stop is file page
`0xb5000` of the pinned initrd's `libsystemd-shared-259.so`. Its six
apparent hazards at offsets `0x172`, `0x372`, `0x572`, `0x772`, `0x972`, and
`0xb72` are `f2` bytes inside the displacement of repeated PLT
`jmp qword ptr [rip+disp32]` stubs. The hot `0x9c0` entry itself is
`sd_json_variant_by_index@plt`, an indirect jump through GOT address
`0x510590`. A reachability proof stops there because the target is read
from guest memory; simply treating the PLT jump as outgoing could allow a
changed GOT entry to branch into a hazardous byte on the same physical page.
A sound extension would have to validate the target and write-protect its
GOT page and translation tables for every accelerated interval, including
cache invalidation. The observed 502 sampled frontier hits at one in 256
represent a bounded part of the roughly 19 million transfer-phase exits,
so this particular PLT proof alone is unlikely to close the application gap.

A second isolated candidate kept the original five-hazard proof and enabled
the new proof only for seven-hazard pages. It passed 268 VMX unit tests, the
full in-box SVM suite, and 1,024 Linux fork replays from virtual count 8.0M
to 8.1M against a scalar reference. Its two 10,000-transfer fair intervals
on the same box were **39.274 s** and **39.504 s**; the adjacent unchanged
control was **39.435 s**. All three runs had 10,000 successes and zero
failures. The candidate's marker-bounded VM exit counts were 19,537,790 and
19,637,337 versus 19,576,056 for the control. The effect is smaller than
run-to-run variation, so the change stays isolated in
`/tmp/bedrock-amd-region7-only-1013`. Its logs, fair markers, and compressed
fork-replay output are in ignored `target/boxctl-evidence/amd-region7-only-1013/`.
The unchanged branch's module was restored on the box and passed the SVM suite.

### Identify reachable-proof frontier instructions (2026-10-13)

A follow-up diagnostic logged the first 16 instruction bytes on one in 256
harmless breakpoint retries while the broad ≥5-hazard proof ran the complete
Tempo workload. The 10,000 transfers succeeded. The retained transfer-phase
kernel ring contains 816 samples over roughly ten seconds. Of those, 434
belonged to one six-hazard page; 413 were at offset `0x9c0`, where the bytes
start `ff 25`, a RIP-relative indirect `JMP`. The bounded region proof must
stop there because its destination comes from memory and could be a different
entry in an executable page. The five-hazard REP page contributed 83 samples
at proof frontiers including direct `CALL`, indirect `CALL`, `RET`, and VEX
instructions. The seven-hazard masked page contributed 63 samples, mostly
ordinary comparisons and moves in its `0x800`–`0xbff` breakpoint range.

Recognizing instruction lengths alone cannot safely remove the indirect
branch and return stops: the proof must also validate their destinations and
any mutable pointers they read. Even eliminating every logged frontier exit
would leave the transfer's roughly 16 million NPT faults as the larger cost.
The diagnostic source remains in `/tmp/bedrock-amd-region7-1013`; logs and
marker snapshots are in ignored `target/boxctl-evidence/amd-frontierbytes-1013/`.
The unchanged module was restored on the disposable box after profiling.

### Guard a proven successor across two hazardous pages (2026-10-13)

The five-hazard reachable-region path returns with only its current
physical page executable. An experimental version admitted one predicted successor when a
direct edge from the first decoded region reaches that page. Both regions
must prove every direct entry into the other is a visited instruction start,
and their exact breakpoint union must fit four AMD debug registers. Unknown
branches remain exact breakpoints; other pages remain NX. Tests cover a
two-page loop, a jump back into an unvisited first-page hazard, and a second
unvisited entry into the successor.

On the EPYC 4245P box `box-67400f8a`, the candidate passed 269 local VMX
unit tests, the in-box SVM suite, and 1,024 Linux fork replays from virtual
count 8.0M to 8.1M against a single-step reference. Complete adjacent Tempo
10,000-transfer runs both had 10,000 successes and zero failures:

| Variant | Fair interval | VM exits | NPT faults | Synthetic MTF exits |
| --- | ---: | ---: | ---: | ---: |
| Two-page reachable proof | 38.320 s | 18,990,303 | 15,652,616 | 1,173,220 |
| Unchanged branch | 38.877 s | 19,287,151 | 15,851,868 | 1,208,825 |

This pair saved 0.556 s (1.4%) and about 297,000 exits. An earlier unchanged
run on the same box took 39.435 s, so run-to-run variation is material.
Logs, fair markers, and stats are in ignored
`target/boxctl-evidence/amd-twopage-1013/`. All module builds, loads, and
runtime tests were performed on the boxctl box.

A second EPYC 4245P box, `box-a3839887`, tested the same admission rule with
minor planner-copy and duplicate-translation reductions. It passed 269 local
unit tests, the in-box SVM suite, and another 1,024 Linux fork replays against
the single-step reference. Its adjacent complete Tempo runs reversed the
first pair:

| Variant | Fair interval | VM exits | NPT faults | Synthetic MTF exits |
| --- | ---: | ---: | ---: | ---: |
| Two-page reachable proof | 39.529 s | 19,534,610 | 16,085,376 | 1,206,536 |
| Unchanged branch | 39.049 s | 19,294,454 | 15,886,532 | 1,192,455 |

Both runs again had 10,000 successes and zero failures. The candidate was
0.479 s slower, with about 240,000 more exits. The two pairs show no
repeatable speedup; the experimental code was removed from the shared branch.
The second box's logs, fair markers, stats, and fork-replay log are in ignored
`target/boxctl-evidence/amd-twopage-repeat-1013/`.

### Replace the pinned Tempo binary's five ZSTD REP copies (2026-10-13)

The hot user page identified above belongs to `ZSTD_compressLiterals` in the
pinned `/usr/local/bin/tempo`. Its five hazards are all `REP MOVSQ`, at ELF
file offsets `0x7a9a3e7`, `0x7a9a4c5`, `0x7a9a5f4`, `0x7a9a7ae`, and
`0x7a9a81b`. An exact-build diagnostic rewrote each copy into a bounded
ordinary `MOVSQ` loop in existing unreachable executable padding. It moves
the following `TEST` or `CMP` into the trampoline, preserving the flags at
the original continuation; the `CMP` at `0x7a9b7b1` is also a branch target,
so that site's preceding `MOV` was moved instead. A byte scan using the
hypervisor's hazard predicate found zero remaining hazards on this code page.
The patch is reproducible with
`diagnostics/patch-zstd-rep-oci.py workloads/tempo/images.tar OUTPUT.tar`.
It verifies the pinned ELF hash, all stolen bytes and code caves, then updates
the OCI layer, config, and manifest digests. No guest kernel or hypervisor
code was changed for this experiment.

Mounting a fetched replacement executable into the container failed with
`Exec format error` even for the unmodified binary, despite a matching guest
SHA-256. The valid comparison therefore rebuilt the OCI image layer itself.
Both complete 10,000-transfer runs on `box-a3839887` passed with zero
failures:

| Guest image | Fair interval | VM exits | NPT faults | Synthetic MTF exits |
| --- | ---: | ---: | ---: | ---: |
| Five ZSTD REP sites rewritten | 38.998 s | 19,339,908 | 15,937,521 | 1,166,820 |
| Original pinned image, adjacent | 38.938 s | 19,257,333 | 15,861,568 | 1,195,233 |

The rewrite reduced MTF exits by about 28,000 but did not reduce NPT faults
or wall time. It is retained as a diagnostic script, not a workload change.
Raw logs and fair markers are in ignored
`target/boxctl-evidence/amd-zstd-norep-1013/`. Module builds and runtime
tests remained inside the disposable boxctl box.

### Sample executable NPT fault pages during the Tempo transfer (2026-10-13)

An isolated diagnostic module on `box-a3839887` logged one in 4,096 execute
NPT faults with guest TSC, RIP, fault GPA, CR3, and the first bytes of the
faulting physical page. The full host `dmesg` stream was captured while the
10,000-transfer workload ran. Filtering to its fair interval retained 3,812
samples. The diagnostic run took 71.239 s because kernel logging perturbed
it; these counts are for ranking pages, not a timing comparison.

| Fault GPA page | Fair samples | Identified file page | Hazardous instructions on page |
| --- | ---: | --- | --- |
| `0x10a28f000` | 400 (10.5%) | Tempo `libc.so.6` `0x152000` | Two `REP MOVSB` |
| `0x1173e6000` | 173 (4.5%) | Tempo `libc.so.6` `0x3c000` | Six `REP STOSQ` |
| `0x11b983000` | 161 (4.2%) | Tempo `libc.so.6` `0x98000` | One `REP STOSQ` |
| `0x13f1aa000` | 113 (3.0%) | Not yet mapped | Unknown |
| `0x11b5ac000` | 101 (2.6%) | Tempo `libc.so.6` `0x96000` | One `REP STOSQ` |
| `0x166e2a000` | 100 (2.6%) | Tempo executable `0x6f11000` | None within page |

File pages were matched by their page-start bytes against the pinned guest
artifacts. NPT execute faults report a physical page base, not the exact
instruction offset. The sampled RIP may also belong to a different page,
so this data does **not** prove that the listed REP instructions executed.
For example, the six `REP STOSQ` sites on libc page `0x3c000` include
`sigblock` setup code; that page's fault volume may largely be false masked
hits. The high share on libc `0x152000` makes it a stronger next diagnostic
target than the ZSTD page, but a guest rewrite needs instruction-level
reachability and a same-box timing/correctness comparison before adoption.
Raw logs and fair markers are in ignored
`target/boxctl-evidence/amd-npt-pages-1013/`. The shared branch's stable
module was restored and its in-box SVM suite passed afterward.

### Relocate two cold glibc REP sites away from hot code (2026-10-13)

A second sampled run on `box-7577ba69` translated each sampled RIP through
the guest page tables. All 3,943 fair-interval samples had a translated RIP
on the reported fault GPA page. The hottest page had 407 samples and matched
the pinned Tempo glibc file page `0x152000`; 296 samples entered at offset
`0x930` and 109 at `0x260`, while its two `REP MOVSB` hazards are at `0xc64`
and `0xcca`. This confirms that the common execute faults on that page are
caused by other entries sharing a page with the REP sites. The diagnostic
module's logging distorted its 73.75 s fair time; it was used only for page
and offset attribution. Its raw log is in ignored
`target/boxctl-evidence/amd-npt-pages-1013/`.

An exact-build OCI diagnostic,
`diagnostics/relocate-libc-rep-oci.py`, moves each `REP MOVSB` and its following
`VMOVDQU` unchanged into existing alignment padding after a `RET` on glibc
page `0x15a000`. Relative jumps connect the original entry and continuation;
they do not change flags or general-purpose registers. The script verifies the
original libc SHA-256, both original instruction sequences, both code caves,
and the patched libc SHA-256 before rebuilding the layer and OCI digests.
The hypervisor's hazard-byte predicate finds zero hazards on the formerly
hot page and two on the destination page. No kernel or hypervisor code changes
are part of this candidate.

The stable shared module passed the in-box SVM suite before these runs. Two
adjacent comparisons in opposite orders used the same box, seed, 10,000
transfers, and fair timing markers. Every run reported 10,000 successes and
zero failures:

| Order | Guest image | Fair interval | VM exits | NPT faults | Synthetic MTF exits |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Two glibc REP sites relocated | 33.989 s | 16,274,461 | 12,861,048 | 1,194,746 |
| 2 | Original pinned image | 38.443 s | 18,957,020 | 15,594,568 | 1,184,458 |
| 3 | Original pinned image | 38.745 s | 19,037,267 | 15,660,380 | 1,201,510 |
| 4 | Two glibc REP sites relocated | 34.520 s | 16,429,102 | 13,008,558 | 1,197,054 |

The repeatable fair-time reduction is 4.2–4.5 s (about 11%); each pair also
removes roughly 2.6–2.7 million NPT faults. The result is still above the
historical Intel 24.087 s run, which used different hardware and the original
image. This binary relocation is a pinned-workload diagnostic, not a general
glibc or hypervisor fix. Raw logs and markers are in ignored
`target/boxctl-evidence/amd-libc-rep-1013/`. All module builds, loads, and
runtime tests remained on the disposable boxctl box.

Two isolated follow-ups tested whether clearing another glibc page helps.
Moving its six `REP STOSQ` sites from file page `0x3c000` to a previously
hazard-free page gave **34.378 s** versus an adjacent two-site control of
**34.376 s** (10,000 successes, zero failures in both). NPT faults were
12,951,225 versus 13,002,141. The new guarded destination page erased the
expected benefit, so this layout was not added to the shared branch.

A second layout placed those six sites in code padding on pages `0x15a000`,
`0x98000`, and `0x96000`, which already had REP hazards. It kept each page at
four or fewer hazards and cleared `0x3c000` without adding a newly hazardous
page. One complete pair gave **33.702 s** versus **34.209 s** for the
two-site image, with 12,740,822 versus 12,924,807 NPT faults; both runs
again had 10,000 successes and zero failures. The 0.507 s difference is
close to observed run variation, so this layout remains an unconfirmed
experiment, not an adopted workload change. Its exact-build diagnostic
patcher is `diagnostics/relocate-libc-rep-reuse-oci.py`; the generated archive
is in ignored `target/boxctl-artifacts/`, and the run logs and markers are in
ignored `target/boxctl-evidence/amd-libc-rep-1013/`.

A further exact-build diagnostic moved the `REP STOSQ` at libc offsets
`0x9828e` and `0x96785` into the already guarded `0x15a000` page. This
cleared two more source pages while keeping the destination at exactly four
hazards. The full paired runs both passed 10,000 transfers with zero failures:
the four-site image took **34.206 s**, with 16,408,298 exits and 12,965,740
NPT faults; the adjacent two-site image took **34.447 s**, with 16,497,460
exits and 13,036,732 NPT faults. The 0.241 s difference is below the observed
run variation, so this is also retained only as a diagnostic script,
`diagnostics/relocate-libc-rep-four-oci.py`. Raw logs and markers are in
ignored `target/boxctl-evidence/amd-libc-rep-1013/`.

### Attribute the remaining hot libc pages to txgen (2026-10-13)

The two-site Tempo image was profiled again with one-in-4,096 execute-fault
logging on `box-7577ba69`. Its 3,139 fair-interval samples showed no faults
on the old Tempo libc page `0x152000` or its relocation page `0x15a000`.
The leading libc pages were still file offsets `0x98000` (149 samples),
`0x3c000` (140), and `0x96000` (87). Their first 16 bytes match both the
Tempo and txgen copies of libc. However, every sample on those pages had CR3
`0x125ce6000`, and 25 other samples in that address space matched libc page
`0x152000` **only in txgen's build**. The same process attribution held for
the later four-site Tempo image: its 3,142 fair samples still included 162,
147, and 98 faults on the three pages, respectively, with txgen-only libc
samples in the same CR3. The latter diagnostic run passed all 10,000 transfers
but took 61.026 s because logging perturbed its timing. This explains why
patching those pages in Tempo's libc barely changed the workload.

The previously unidentified shared pages at physical addresses around
`0x13f5...` map to the guest initrd's Nix glibc 2.42, including file pages
`0xc2000`, `0xae000`, `0x180000`, `0x5e000`, and `0x95000`. Other sampled
pages map to initrd `libsystemd-shared-259.so` and `libjq.so.1.0.4`.
Those mappings came from page-start byte matches against the initrd, and
they do not by themselves establish why each page repeatedly faults.

`diagnostics/relocate-txgen-libc-rep-four-oci.py` is an exact-build diagnostic
that applies **on top of** the two-site Tempo image. It moves txgen's two
`REP MOVSB` sites from libc file page `0x152000` and its one `REP STOSQ`
site each from `0x98000` and `0x96000` into alignment padding after a `RET`
on previously unguarded page `0x15a000`. It verifies the original and patched
libc hashes, source instructions, and code caves before updating OCI digests.
Disassembly of the rebuilt txgen libc shows zero REP sites on all three
source pages and four on the destination page. The stable module's SVM suite
passed before the first full run.

Two 10,000-transfer combined-image runs on separate EPYC 4245P boxes passed
with zero failures. A second box ran an adjacent Tempo-only control:

| Box and guest image | Fair interval | NPT faults | Synthetic MTF exits |
| --- | ---: | ---: | ---: |
| `box-7577ba69`, Tempo plus txgen relocation | 31.361 s | 10,951,582 | 1,203,316 |
| `box-7577ba69`, closest earlier Tempo-only control | 34.447 s | 13,036,732 | 1,197,054 |
| `box-3fa32d2b`, Tempo plus txgen relocation | 30.950 s | 10,806,879 | 1,201,357 |
| `box-3fa32d2b`, adjacent Tempo-only control | 34.739 s | 13,029,092 | 1,190,320 |

The adjacent pair saves **3.789 s** and **2,222,213 NPT faults**. This is
larger than prior run variation and shows that moving the sites in the correct
libc copy matters. The best AMD result is about 1.28 times the historical
Intel 24.087 s run, but those runs used different hardware and guest images;
that ratio is a workload reference, not an architecture-normalized overhead.
The experiment remains a pinned guest-image rewrite, not a general hypervisor
optimization. Logs and timing markers are in ignored
`target/boxctl-evidence/amd-npt-pages-1013/` and
`target/boxctl-evidence/amd-libc-rep-1013/`.

One replacement `m4-metal-small` box exposed an EPYC 4244P (family 25)
instead of the 4245P (family 26) used above. The stable module's SVM benchmark
failed its control-flow acceleration assertion twice, reporting 20,030 and
20,040 exits. It was not used for Tempo timing. The matching 4245P box passed
the full SVM suite before the adjacent comparison. This is an unresolved
hardware-portability issue; the 4244P log is in ignored
`target/boxctl-evidence/box-9a86491b/`.

### Reprofile the faster combined image (2026-10-13)

A one-in-4,096 execute-fault sample on `box-3fa32d2b` retained 2,621 hits
inside the fair interval. The full transfer passed 10,000/10,000, but logging
stretched that interval to 53.691 s; its counts are for attribution only.
The former hot txgen libc pages `0x152000`, `0x98000`, and `0x96000` had
zero sampled faults. The leading identified pages are now:

| Guest file page | Fair samples | Leading entry offsets |
| --- | ---: | --- |
| Initrd Nix glibc `0xc2000` | 114 | `0x310` (all 114) |
| Initrd Nix glibc `0xae000` | 92 | `0x7b0` (90) |
| Initrd Nix glibc `0x180000` | 82 | `0x490` (41), `0x4b0` (26) |
| Tempo libc `0x153000` | 75 | `0x350` (72) |
| Initrd `libjq.so.1.0.4` `0x31000` | 74 | `0x53a` (41), `0x33d` (26) |
| Initrd `libsystemd-shared-259.so` `0xb6000` | 70 | `0xb30` (25), `0xc10` (25) |
| Initrd Nix glibc `0x5e000` | 61 | `0x9c0` (all 61) |
| Initrd Nix glibc `0x95000` | 61 | `0x710` (49), `0x76f` (11) |

Two of the hot Nix glibc pages (`0xc2000` and `0x95000`) end in an incomplete
`0x0f` opcode. Bedrock's scanner already records that suffix as a boundary
hazard even though disassembly shows no REP instruction on either page. A
proposed zero-hazard-region optimization was discarded when its targeted
unit test found one boundary hazard on a synthetic page with the same suffix.
The remaining profile therefore calls for a plan that covers boundary
hazards and common entry offsets without adding frequent proof-frontier
stops. The raw sample and test output are in ignored
`target/boxctl-evidence/amd-npt-pages-1013/` and the box logs.

### Clear the remaining hot Tempo libc REP page (2026-10-13)

The combined image still sampled 75 faults on Tempo libc file page
`0x153000`, mostly at ordinary entry offset `0x350`. Its one `REP STOSB`
is at `0x15348a`. Exact-build diagnostic
`diagnostics/relocate-tempo-libc-rep-third-oci.py` takes the two-site Tempo
image (optionally with the txgen patch already applied) and moves that REP
and its following `MOV RAX,RDX` to unused alignment padding at `0x15a98e`,
between the two existing Tempo relocations. The original five bytes become
a relative jump; the moved instructions return to `0x15348f`. The script
checks the pinned input/output libc hashes, the source bytes, and the code
cave before rebuilding OCI digests. Static disassembly shows no REP sites on
Tempo libc pages `0x152000` or `0x153000` and three on destination page
`0x15a000`. The txgen image descriptor is identical in both archives.
From the repository root, the full measured image can be rebuilt with
`workloads/tempo/native-comparison/diagnostics/build-amd-tempo-shaped-image.sh workloads/tempo/images.tar OUTPUT.tar`.
The wrapper's output SHA-256 is
`05cf1c7ef4cf24f90d1f87c01708d3a7aced52c2817442e21bcb9901101ed04b`,
identical to the archive used for the runs below.

The same EPYC 4245P box, seed, stable module, and fair markers produced:

| Guest image | Fair interval | NPT faults | Synthetic MTF exits |
| --- | ---: | ---: | ---: |
| Tempo two-site plus txgen four-site control | 30.950 s | 10,806,879 | 1,201,357 |
| Tempo three-site plus txgen four-site | 30.301 s | 10,413,071 | 1,180,222 |
| Tempo two-site plus txgen four-site control | 31.166 s | 10,891,515 | 1,198,887 |
| Tempo three-site plus txgen four-site | 30.354 s | 10,424,117 | 1,189,293 |

All four runs passed 10,000 transfers with zero failures. The extra move
repeatably saves about **0.6–0.8 s** and **0.38–0.47 million NPT faults**.
The best AMD fair interval is now 30.301 s, about 1.26 times the historical
Intel 24.087 s run on different hardware and guest inputs. As with the other
relocations, this is a pinned-workload diagnostic, not a general hypervisor
fix or an architecture-normalized overhead claim. Logs and fair markers are
in ignored `target/boxctl-evidence/amd-libc-rep-1013/`; all kernel module
builds, loads, and runtime tests were on disposable boxctl boxes.

### Relocate the initrd's tcache REP without adding a hot guard (2026-10-13)

The optimized-image profile retained 92 samples on the guest initrd's Nix
glibc file page `0xae000`, chiefly ordinary entry offset `0x7b0`. That
page has one `REP STOSQ` in `tcache_init.part.0` at `0xae9dc`. An exact-build
initrd diagnostic moved the REP and following `LEA RCX,[RDX+0x90]` unchanged
into executable alignment padding after a `RET`, replacing the original ten
bytes with a relative jump and NOPs. It checks the pinned compressed initrd,
libc build, source bytes, cave bytes, and patched libc hash before writing a
same-size cpio payload and deterministic gzip output. Both layouts booted and
passed the complete transfer on the EPYC 4245P box:

| Initrd libc destination | Fair interval | NPT faults | Synthetic MTF exits |
| --- | ---: | ---: | ---: |
| Original initrd, two nearby controls | 30.301 / 30.354 s | 10,413,071 / 10,424,117 | 1,180,222 / 1,189,293 |
| Existing hot libc page `0x180000` | 30.592 s | 10,802,717 | 1,190,342 |
| Previously cold guarded page `0xa6000` | 30.268 s | 10,139,771 | 1,229,397 |

Every run reported 10,000 successes and zero failures. Moving into hot page
`0x180000` increased NPT faults by about 0.38 million; it consumes another
debug-breakpoint slot on a page already used heavily. The cold-page layout
removed about 0.28 million NPT faults but added about 40,000 synthetic traps
and did **not** establish a wall-time gain beyond run variation. The cold
layout is reproducible with
`diagnostics/patch-nix-libc-rep-initrd.py ORIGINAL_INITRD_GZ OUTPUT_INITRD_GZ`;
its output SHA-256 is
`cff6ae36f303168c91e75447568dfd48f01595289ba60789807651e904066a0a`.
The hot layout is retained only in ignored artifacts and run logs. Neither
initrd rewrite is adopted as the performance baseline. Logs and markers are
in ignored `target/boxctl-evidence/amd-libc-rep-1013/`.

### Profile the fastest image during the full transfer (2026-10-13)

On the same EPYC 4245P HWE box, the stable module and the three-site Tempo
plus four-site txgen image passed **10,000/10,000** transfers with zero
failures while host AMD IBS sampled the first 15 seconds of the fair interval
at `ibs_op//`, period 100 million. The marker interval was **30.487 s**; this
sampled diagnostic is not a replacement for the adjacent unsampled 30.301 s
and 30.354 s comparisons. It recorded **754 samples, zero lost**, with
22.68% attributed to `svm_run_guest`, 9.42% to `handle_run`, 3.58% to
`collect_page_breakpoints`, 2.92% to `page_hazards_memo`, and 2.12% to
`prepare_global_into`. Attribution to `svm_run_guest` may include guest
residency at `VMRUN`, so it is not a measure of transition cost alone.

The fair-marker deltas were **10,418,913 NPT faults**, **1,194,764 synthetic
MTF exits**, and **13,848,766 total exits**. VM-entry preparation accounted
for **37.64 billion of 117.26 billion measured runner cycles** (32.1%).
The earlier in-kernel cycle probes remain better evidence for the internal
planner split; this lower-resolution profile confirms that guarded-page
faults and their per-entry software work still dominate after the guest
relocations. A concurrent host `cycles` profile captured only four samples,
so it cannot locate a new bottleneck. Raw logs, marker snapshots, and both
`perf.data` files are in ignored
`target/boxctl-evidence/amd-best-ibs-1013/`. All module execution and
profiling occurred inside the disposable boxctl box.

### Check whether a larger guarded-hazard hint helps (2026-10-13)

The current guarded-page hazard fast path has a direct-mapped 64-slot hint.
An isolated variant enlarged it to 256 slots to test whether collisions
still account for material planner time. It passed the full in-box SVM suite,
including replay and native-loop checks. On the same EPYC 4245P box, stable
workload, seed, and shaped image, the variant took **30.199 s** for 10,000
successful transfers and zero failures. After restoring the unchanged
64-slot module and rerunning its suite, the adjacent control took **30.176 s**
with the same transfer result. The variant had 10,471,087 NPT faults and
1,154,451 synthetic MTF exits; the control had 10,452,588 and 1,182,277.
Measured VM-entry preparation was 37.46 billion cycles versus 37.38 billion
for the control. The larger hint shows no end-to-end gain and is discarded;
the shared branch retains 64 slots. Logs and fair markers are in ignored
`target/boxctl-evidence/amd-hint256-1013/`.

### Move the initrd libjq REP off a hot page (2026-10-13)

The one-in-4,096 NPT profile of the shaped Tempo image found 74 fair-interval
fault samples on the initrd's `libjq.so.1.0.4` file page `0x31000`, mainly
at ordinary entry offsets `0x53a` and `0x33d`. Its one disassembled hazard is
`REP MOVSQ` at `0x31801`, in `jq_set_colors`. Exact-build diagnostic
`diagnostics/relocate-libjq-rep-initrd.py` replaces those three bytes with a
short jump to otherwise unreachable alignment padding at `0x3183a`, which
then jumps to a second padding cave at `0x34096` on a page already guarded
for another REP. That cave executes the original `REP MOVSQ` and returns to
`0x31804`, leaving the existing branch target and `MOV EAX,1` intact. The
script checks the compressed initrd hash, libjq hash, each source/cave byte
sequence, and patched hashes. Rebuilding produced exactly the tested
initrd, SHA-256
`8236cca295d44362989436882e0995e45308b472355583eb4e03b123f61c2029`.
Static disassembly verifies the jump chain and the relocated REP. This is a
pinned-build guest diagnostic, not a general hypervisor rewrite.

On one EPYC 4245P HWE box with the stable module, same shaped OCI image,
seed, and fair markers, alternating patched/original initrds gave:

| Initrd | Fair interval | NPT faults | Synthetic MTF exits | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| libjq relocated | 29.074 s | 9,700,419 | 1,162,723 | 37.04 billion |
| original control | 30.424 s | 10,530,206 | 1,156,155 | 37.79 billion |
| libjq relocated | 28.635 s | 9,697,285 | 1,169,899 | 35.90 billion |
| original control | 30.501 s | 10,562,070 | 1,188,344 | 38.10 billion |

Every run passed **10,000/10,000** transfers with zero failures. The change
removed about **0.83–0.86 million NPT faults** and improved the fair interval
by **1.35–1.87 s** relative to the adjacent original-initrd controls. The
best AMD interval is now **28.635 s**, about 1.19 times the historical Intel
24.087 s interval on different hardware and guest inputs. The separately
measured glibc tcache REP relocation did not help, so this libjq change is
applied to the original initrd alone. Logs and marker snapshots are in
ignored `target/boxctl-evidence/amd-libjq-1013/`. All module builds, loads,
and runtime tests occurred inside disposable boxctl boxes.

### Remove a conservative boundary hazard from hot Nix libc (2026-10-13)

A new one-in-4,096 NPT sample of the libjq-relocated workload retained 2,373
fault samples in its fair interval. The hottest identified initrd page was
Nix glibc file page `0xc2000`: **137 samples**, all at entry offset `0x310`
in `__strlen_sse2`. The page has no disassembled REP instruction, but its
last byte is `0x0f`, inside a `PADDB` that starts at file offset `0xc2ffd`
and crosses into page `0xc3000`. Bedrock conservatively treats that suffix
as a possible instruction-boundary hazard under a different executable
alias. The sample's 49.245 s fair interval is distorted by logging and is
used only for attribution. A live `/dev/kmsg` reader captured the complete
sample after the first attempt's kernel ring buffer lost its early lines.

Exact-build diagnostic `diagnostics/relocate-nix-libc-boundary-initrd.py`
starts from the libjq-patched initrd. It replaces the five-byte cross-page
`PADDB` with a relative jump into otherwise unreachable alignment padding
at `0xc21a2` on the same file page. The cave executes the original `PADDB`
then jumps back to `0xc3002`. This leaves the hot `__strlen_sse2` entry
unchanged and removes the trailing `0x0f` boundary hazard from its page.
The script checks both input hashes, source/cave bytes, and output hashes;
its output SHA-256 is
`cf1632c3409f3d709cb5be5d2a7501f86201508af4ec68d660d268ddae2f70c4`.
Static disassembly verifies the jump chain. This is a pinned-build guest
diagnostic, not a general relaxation of the hypervisor's boundary proof.
From the repository root, reproduce the two initrd changes in order:

```sh
python3 workloads/tempo/native-comparison/diagnostics/relocate-libjq-rep-initrd.py \
  workloads/tempo/native-comparison/initrd.gz /tmp/bedrock-libjq-initrd.gz
python3 workloads/tempo/native-comparison/diagnostics/relocate-nix-libc-boundary-initrd.py \
  /tmp/bedrock-libjq-initrd.gz /tmp/bedrock-shaped-initrd.gz
```

The stable module on the same EPYC 4245P box, seed, and shaped OCI image
gave these alternating full-workload results:

| Initrd | Fair interval | NPT faults | Synthetic MTF exits | VM-entry preparation cycles |
| --- | ---: | ---: | ---: | ---: |
| libjq plus boundary relocation | 27.507 s | 8,853,684 | 1,193,771 | 35.32 billion |
| libjq-only control | 29.520 s | 9,995,231 | 1,168,468 | 36.59 billion |
| libjq plus boundary relocation | 28.421 s | 9,175,427 | 1,184,689 | 36.05 billion |
| libjq-only control | 29.644 s | 9,983,536 | 1,200,024 | 37.11 billion |

All four runs passed **10,000/10,000** transfers with zero failures. The
relocation removed about **0.8–1.1 million NPT faults** and cut the fair
interval by roughly **1.1–2.1 s** against adjacent controls. The best
AMD interval is now **27.507 s**, about 1.14 times the historical Intel
24.087 s interval on different hardware and guest inputs. Neither guest
rewrite by itself closes the general AMD hypervisor performance gap; a
transparent boundary/alias proof would need to validate every executable
alias and writes/remappings of the next page. Logs and marker snapshots are
in ignored `target/boxctl-evidence/amd-boundary-1013/`; the complete NPT
sample is in ignored `target/boxctl-evidence/amd-postjq-profile-1013/`.
An additional one-in-4,096 execute-fault profile of the boundary-patched
initrd passed 10,000/10,000 transfers. **None of its 2,113 fair-interval
samples** matched libc page `0xc2000`, versus 137 of 2,373 before this
relocation. Its logged fair interval was 45.613 s and is for attribution,
not speed comparison. The full live sample is in ignored
`target/boxctl-evidence/amd-boundary-profile-1013/`; the stable module was
restored afterward and passed the full in-box SVM suite.

### Test a hypervisor-only single-alias boundary proof (2026-10-13)

An isolated candidate tried to avoid the boundary breakpoint without
rewriting libc. It required a single exact breakpoint at page offset `0xfff`
under a guarded translation tree, one executable virtual alias, an active
NPT write guard on the current page, and a trusted/write-protected next page.
It read the joined instruction bytes on each entry and retained the
breakpoint if they formed a forbidden instruction. Targeted tests checked
the safe continuation and fallback for unsafe bytes, an untrusted next page,
and a second alias. All 268 VMX library tests and the full in-box SVM suite
passed.

The complete libjq-only Tempo run passed 10,000 transfers with zero failures,
but its fair interval was **29.911 s**, versus **29.520 s** and **29.644 s**
for the adjacent stable-module controls. NPT faults fell to **9,733,231**
from about **9.99 million**, while measured VM-entry preparation rose to
**40.19 billion cycles** from **36.59–37.11 billion**. This does not match
the guest relocation's 0.8–1.1 million-fault reduction or improve wall time.
The source remains isolated and the stable module was restored and retested;
the exact candidate diff and run evidence are in ignored
`target/boxctl-evidence/amd-hv-boundary-1013/`. More work would need to
identify why the proof misses most hot transitions and avoid its entry-time
checks without weakening alias/write invalidation.

An instrumented repeat passed 10,000/10,000 transfers and took **29.845 s**,
with **9,684,164 NPT faults** and **38.89 billion VM-entry preparation
cycles**. Its last periodic diagnostic snapshot, covering boot and transfer,
recorded 56.62 million proof calls: 6.89 million had the single `0x0f`
boundary shape, 6.49 million retained an active current-page write guard,
and 4.52 million had a trusted next page and passed the joined-byte check.
The one-alias and guarded-tree checks did not reject any of the shaped
cases in that snapshot. Thus the proof often succeeds, but its repeated
checks run millions of times to remove only roughly 0.25–0.31 million
transfer-phase NPT faults. A cheaper cached proof would need explicit
invalidation for page-table changes, both code pages' writes, and host
mapping changes; simply widening this uncached proof cannot supply the
measured guest-relocation gain. The diagnostic counters and instrumented
candidate diff are in the same ignored evidence directory.

### Measure the native Tempo floor on EPYC 4245P (2026-10-13)

On a disposable `m4-metal-small` box with an AMD EPYC 4245P, Ubuntu
6.8.0-146-generic, and Docker 29.1.3, `measure-native.py` ran the original
pinned Tempo and txgen images without Bedrock. Both containers were restricted
to the same host CPU set. Its host-monotonic fair markers cover 10,000
unthrottled transfers and exclude setup. All four runs had 10,000 successes,
zero failures, and zero reverted transactions:

| Native CPU set | Run 1 | Run 2 |
| --- | ---: | ---: |
| CPUs 2–6 (five logical CPUs) | 13.080 s | 13.080 s |
| CPU 1 only | 13.217 s | 13.211 s |

The one-CPU check shows that the five-CPU native result is not mainly a CPU
allocation effect. Against the one-CPU native interval, the original-image
AMD guest's stable 38.877–38.938 s fair runs are about **2.94×**, while the
best 27.507 s shaped-image guest run is about **2.08×**. The native box has
the same CPU model as the guest test box but is a separate physical machine,
and the shaped guest uses different image and initrd bytes. These ratios
locate the gap; they are not a precise virtualization-overhead measurement.
The historical Intel Bedrock 24.087 s result is on different hardware.
Native logs and JSON results are in ignored
`target/boxctl-evidence/amd-native-1013/`.

### Test removal of unused virtual-PMC resets (2026-10-13)

An isolated SVM candidate skipped ten VMCB writes that re-zero PMC1–5 on
each counted entry. These fields start zero, their guest MSRs are intercepted,
and Bedrock only arms PMC0. All 23 local `bedrock-svm` tests and the full
in-box SVM suite passed on a disposable EPYC 4245P HWE 7.0 box. The original
pinned Tempo guest and images, seed, CPU affinity, and fair markers were
identical in the adjacent complete runs; both passed 10,000/10,000 transfers
with zero failures:

| Module | Fair interval | NPT faults | Total exits | Entry cycles/exit |
| --- | ---: | ---: | ---: | ---: |
| Skip unused PMC resets | 38.572 s | 15,723,322 | 19,117,801 | 2,395 |
| Unchanged control | 39.250 s | 16,041,879 | 19,479,886 | 2,401 |

The candidate had about 319,000 fewer NPT faults, which can explain the
0.678-second wall-time difference. The six-cycle normalized entry difference
does not establish a useful saving from the removed writes. This change is
not merged; source remains in `/tmp/bedrock-amd-pmu-lean-1013` and logs and
marker snapshots are in ignored `target/boxctl-evidence/amd-pmu-lean-1013/`.
The unchanged module was restored and passed the in-box suite before its
control run. No module was built or loaded on the workstation.

### Try eager masked-breakpoint merging before slot admission (2026-10-13)

An isolated candidate merged nearby hazard entries into a masked AMD debug
slot before all four slots filled, then let the global planner admit another
page based on actual occupied slots rather than raw hazard count. A focused
test proved that three pages with six REP hazards could fit three conservative
256-byte masks; all 269 VMX library tests and the full in-box SVM suite
passed. The broad version made no guest serial progress in 90 seconds on an
EPYC 4245P HWE box and was stopped. The unchanged module produced normal
guest output within 30 seconds on that box.

To isolate the promising glibc case, a diagnostic variant merged only the
measured `0xc64`/`0xcca` REP pair and kept the other pages' mask policy.
It passed the same local and in-box suites. Both full original-image Tempo
runs passed 10,000 transfers with zero failures:

| Module | Fair interval | NPT faults | Total exits | Entry cycles/exit | Runner plus guest cycles |
| --- | ---: | ---: | ---: | ---: | ---: |
| Pair-only diagnostic | 55.474 s | 14,620,726 | 18,011,746 | 3,426 | 215.11 billion |
| Unchanged control | 38.068 s | 15,544,624 | 18,856,548 | 2,384 | 147.61 billion |

Although the candidate removed about 924,000 NPT faults, its measured
entry and runner/guest cycles rose far more than those faults saved.
This is a measured regression, so neither the broad nor pair-only mask is
merged. Source remains in `/tmp/bedrock-amd-coldmask-1013`; the exact patch,
logs, and marker snapshots are under ignored
`target/boxctl-evidence/amd-coldmask-1013/`. The unchanged module was
restored and passed the in-box suite. All module builds and loads were inside
the disposable box.

### Move a cold kernel REP off the hot syscall page (2026-10-13)

The one-in-4,096 execute-fault profile of the fastest shaped image found 107
fair-interval samples on guest-kernel physical page `0x1fb1000`, including 38
entries at `do_syscall_64` offset `0x60` and 26 at its post-syscall offset
`0x128`. The matching pinned `vmlinux-layout-candidate` has one REP hazard on
that page: `REP MOVSQ` at `0xffffffff81fb1e39`, in the colder
`exc_general_protection` handler. A byte-level scan using the hypervisor's
hazard predicate found only that entry, and found none on the page after the
diagnostic rewrite. The page's trailing bytes have no boundary hazard.

`diagnostics/relocate-kernel-gp-rep.py` requires the exact original vmlinux
SHA-256 and copies the REP plus its following stack `MOV` to 16 `INT3` padding
bytes at `0xffffffff81fb0ef0`, on a different kernel page. It inserts a
relative jump at the original site and a jump back after the copied bytes.
The source, cave, and final output hashes are checked; disassembly verifies
both jumps and the unchanged copied instructions. The output SHA-256 is
`93f151be55863fd3d27de20e9b4e778fadd794ac93f29bd3e4db7ee08bf54b33`.
This is an exact-build guest-layout diagnostic. The padding is within the
kernel's `__noinstr_text_start` region and has no dedicated unwind metadata;
the binary rewrite needs source-level integration and exception-path review
before it can be called a general kernel change.

On one disposable EPYC 4245P box with HWE 7.0.0-38, the stable module passed
the full in-box SVM suite. The patched/control/patched sequence used the same
module, pinned shaped OCI archive, initrd, seed, CPU pin, and 10,000-transfer
command. All three runs passed the application check:

| Kernel | Fair interval | Whole-VM NPT faults | Whole-VM entry cycles |
| --- | ---: | ---: | ---: |
| GP REP relocated | 27.094 s | 48,752,030 | 172.28 billion |
| Unchanged layout | 27.905 s | 51,190,069 | 174.82 billion |
| GP REP relocated | 27.373 s | 48,969,842 | 172.44 billion |

The paired fair-time gain is **0.53–0.81 s**, or about **2–3%**, and the
whole-VM NPT count fell by 2.22–2.44 million. The count covers startup as
well as transfer and should not be interpreted as a fair-interval count.
The fastest measured shaped-image interval is now 27.094 s, about 1.12 times
the historical Intel 24.087 s interval on different hardware and inputs.
Logs and counters are in ignored `target/boxctl-evidence/amd-kernel-gp-1013/`.
No module was built or loaded on the workstation.

### Replace the kernel GP REP at source level (2026-10-13)

A first source-level attempt aligned the entire `exc_general_protection`
handler to a page boundary. The Nix guest-kernel build and full box SVM suite
passed, and the new syscall page had no hazard bytes. On one EPYC 4245P box,
however, the 10,000-transfer fair interval regressed to **29.787 s** versus
**27.713 s** for the unchanged kernel. Whole-VM NPT faults fell from
51,046,115 to 49,189,685, but entry-preparation cycles rose from 173.73 to
183.87 billion. The alignment patch was discarded. Its paired logs are in
ignored `target/boxctl-evidence/amd-gp-source-align-1013/`.

The retained guest patch, `guest/patches/0009-x86-entry-avoid-rep-in-gp-handler.patch`,
keeps the handler at its previous address. It initializes the same 92-byte
GP descriptor using eleven explicit `MOVSQ` instructions and one `MOVSL`,
without a `REP` prefix. The pinned kernel build succeeded; disassembly of
its `vmlinux` shows `do_syscall_64` still at `0xffffffff81fb1060`,
`exc_general_protection` still at `0xffffffff81fb1df0`, and zero hazard
entries on page `0xffffffff81fb1000`. Unlike the earlier binary diagnostic,
this patch uses normal source, linking, and kernel unwind generation. Its
tested `vmlinux` SHA-256 is
`8735375bae8368849a6d75665c70150c061e3756e09e38c8f31e0aad4091ea81`.

The stable AMD module passed the full in-box SVM suite. On the same disposable
EPYC 4245P HWE 7.0.0-38 box, alternating unchanged and fixed-copy guest
kernels used the same shaped OCI archive, initrd, seed, CPU pin, and workload:

| Guest kernel | Fair interval | Whole-VM NPT faults | Whole-VM entry cycles |
| --- | ---: | ---: | ---: |
| Unchanged control | 27.713 s | 51,046,115 | 173.73 billion |
| Fixed-count GP copy | 26.849 s | 49,121,431 | 169.18 billion |
| Unchanged control | 27.671 s | 51,195,734 | 173.84 billion |
| Fixed-count GP copy | 26.075 s | 48,876,995 | 168.27 billion |

All four runs completed 10,000 transfers with zero failures and zero
reverts. The fixed-copy improvement was **0.82–1.64 s** against the adjacent
controls; whole-VM NPT faults fell by roughly 1.9–2.3 million. The fair-time
range reflects workload/host variation, so the 26.075 s run is the fastest
observed interval rather than a guaranteed steady-state rate. It is about
1.08 times the historical Intel 24.087 s interval on different hardware and
inputs. This result applies to the shaped guest image; the original-image
AMD guest still has a much larger gap. Logs and counters are in ignored
`target/boxctl-evidence/amd-gp-fixed-source-1013/`. No module was built or
loaded on the workstation.

### Check the source-level GP fix with original Tempo images (2026-10-13)

The preceding fixed-count GP kernel was also compared against the unchanged
aligned-fallback kernel using the **original** pinned `images.tar` and
`initrd.gz`, not the shaped OCI or initrd. One disposable EPYC 4245P HWE
7.0.0-38 box ran the same stable module, seed, CPU pin, and 10,000-transfer
command in control/fixed/control/fixed order. Its full in-box SVM suite passed
before the workload. Every run reported 10,000 successes, zero failures, and
zero reverted transactions:

| Guest kernel | Fair interval | Whole-VM NPT faults |
| --- | ---: | ---: |
| Unchanged control | 39.644 s | 61,295,839 |
| Fixed-count GP copy | 38.667 s | 58,851,890 |
| Unchanged control | 39.128 s | 60,933,377 |
| Fixed-count GP copy | 37.951 s | 58,549,011 |

The paired gain is **0.98–1.18 s**, with about **2.4 million** fewer NPT
faults over each entire VM run. The first control's startup was unusually
slow, so only the marker-bounded transfer interval is used for the timing
claim. The best original-image interval is now **37.951 s** on this host,
still about 2.87 times the separate one-core EPYC native measurement and
1.58 times the historical Intel 24.087 s run on different hardware. The
source patch helps the unchanged workload, but it leaves the user-space
hazardous-code working set as the main performance gap. Logs and counters
are in ignored `target/boxctl-evidence/amd-gp-original-1013/`. All module
builds and loads stayed inside the box.

### Remove libjq's cold color-copy REP through the normal guest build (2026-10-13)

The earlier exact-build `libjq` relocation saved 1.35–1.87 s on a shaped
Tempo image, but could not be reproduced by rebuilding the guest. Nix's jq
1.8.1 compiler generated `REP MOVSQ` for the eight-pointer fallback copy in
`jq_set_colors`. That instruction shared file page `0x31000` with hot jq
printing code. `guest/patches/jq-avoid-rep-colors.patch` makes only the
fallback destination stores volatile, retaining the same pointer values while
preventing that REP lowering. `nix/podman-initrd.nix` uses the patched jq in
the guest closure. The compiled `libjq.so.1.0.4` has an ordinary scalar
store loop at `0x317c0` and no disassembled REP on page `0x31000`. All nine
upstream jq tests passed in the disposable box.

Both control and patched `tempoInitrd` outputs were built through Nix inside
the same EPYC 4245P HWE 7.0.0-38 box. The control used commit `8b2f4c7`,
and the candidate applied only the jq guest override. The
`native-comparison/prepare-initrd.sh` helper injected the identical
`fair-bench` and `fair-run.sh` into both archives. Their normalized archive
path lists were identical; the other rebuilt guest helper binaries
(`bedrock-file-fetch`, `bedrock-file-store`, and `bedrock-pebs-register`) were
byte-identical. The guest kernel was the same source-patched build in all
runs (`vmlinux` SHA-256
`8735375bae8368849a6d75665c70150c061e3756e09e38c8f31e0aad4091ea81`).
The same in-box Bedrock module passed the SVM suite first. The original pinned
`images.tar`, compose file, seed 12345, one host CPU, and 10,000 transfers
were unchanged. `BEDROCK_FAIR_TIMING=1` provided host markers around the
complete transfer command. Every measured run passed 10,000/10,000 with zero
failures and zero reverted transactions.

| Order | Guest jq | Fair interval | Whole-VM NPT faults | Total exits |
| --- | --- | ---: | ---: | ---: |
| 1 | Patched | 37.559 s | 57,291,096 | 77,995,090 |
| 2 | Control | 39.102 s | 58,690,813 | 79,751,207 |
| 3 | Control | 37.764 s | 58,129,969 | 78,983,307 |
| 4 | Patched | 36.647 s | 57,169,406 | 77,982,968 |

The two adjacent pairs saved **1.54 s and 1.12 s**, with **1.40 million and
0.96 million fewer NPT faults**, respectively. The run-to-run spread is real,
so this is a roughly 3.5% measured improvement, not an exact prediction for
other hosts. This is guest shaping, not a general AMD hypervisor solution:
the original-image AMD result remains much slower than native and the
historical Intel interval was measured on different hardware and guest bits.
Build and runtime logs are in ignored
`target/boxctl-evidence/amd-jq-source-1013/`. All module builds and loads
remained inside the disposable box.

### Reprofile the source-patched jq initrd (2026-10-13)

A diagnostic module logged one in 4,096 execute NPT faults while the new
source-built jq initrd ran the original Tempo OCI images. It passed the full
10,000-transfer workload. The fair interval contained 3,519 samples; 778
matched a unique file page in the initrd. The former `libjq` page `0x31000`
had **zero** sampled faults, confirming that its hazard was removed. The
largest identified initrd pages were Nix glibc `0xc2000` (141 samples, all
entry offset `0x310`), `libsystemd-shared` `0xb6000` (76), and Nix glibc
`0x180000` (73). Other jq pages `0x2d000`, `0x2e000`, and `0x34000` had 42,
40, and 36 samples, respectively. The glibc `0xc2000` page has the previously
identified cross-page opcode boundary hazard; `0x180000` has two REP MOVSB
instructions. The diagnostic logging stretched the fair interval to 71.647
s, so it is only an attribution run. The stable module was restored in the
box and passed smoke. The live kernel log and workload output are in ignored
`target/boxctl-evidence/amd-postjq-source-profile-1013/`.

### Check jq's remaining input-buffer REP (2026-10-13)

`libjq` page `0x34000` contains a `REP STOSQ` at `0x34205` generated for
`memset(state->buf, 0xff, sizeof(state->buf))` in
`jq_util_input_read_more`. A source-level diagnostic used a volatile
function pointer to call libc's `memset`, removing the REP from that jq page
without changing the 4 KiB initialization. The rebuilt jq passed all nine
upstream tests, and its disassembly showed the call through the pointer.
The guest initrd was built normally through Nix in the box. With the same
stable module, kernel, original Tempo OCI images, seed, and CPU pin, both
10,000-transfer runs passed:

| Guest jq | Fair interval | Whole-VM NPT faults | Total exits |
| --- | ---: | ---: | ---: |
| Additional input-buffer change | 37.322 s | 57,532,626 | 78,443,476 |
| Pushed jq color-copy fix | 37.296 s | 57,429,484 | 78,297,387 |

The extra change did not improve elapsed time and produced about 103,000
more NPT faults. It remains isolated in `/tmp/bedrock-jq-stos-1013` and was
not pushed. Logs are in ignored `target/boxctl-evidence/amd-jq-stos-1013/`.

### Cache the guarded boundary continuation proof (2026-10-13)

The earlier single-alias proof removed some false breakpoint stops on pages
ending in `0x0f`, but repeated translation and joined-byte inspection made
VM entry more expensive than the saved NPT faults. The retained AMD path
caches a safe continuation only when the guarded page-table tree, current
hazard memo revision, and NPT mapping generation match. An EPT trusted-code
generation changes whenever a trusted leaf is armed or revoked. A cache hit
therefore skips the next-page translation and byte read while the current
hazard memo remains write-guarded and the next page's trusted-code proof has
not been revoked. A miss runs the complete existing proof; unsafe joins keep
their breakpoint. Tests cover changed continuation bytes, an untrusted next
page, a second executable alias, and the trusted-code generation transitions.

The first cached prototype still walked both NPT protections on every hit.
Although it cut about 0.53 million whole-VM NPT faults, its 39.968 s fair
interval was slower than the 39.736 s control and its entry preparation rose
from 190.28 to 198.14 billion cycles. A generation-checked version removed
the repeated joined-byte read but still spent about 193.20 billion entry
cycles and took 39.818 s. Neither prototype is retained. The final version
uses the guarded-memo and mapping-generation invariant already used by the
hazard-memo fast path and checks the next trusted leaf only on a cache miss.

The final uninstrumented module passed 19 EPT and 268 VMX library tests and
the in-box smoke, transition, mixed-memory, and native-loop suites. On one
EPYC 4245P HWE 7.0.0-38 box, the original pinned Tempo `initrd.gz` and
`images.tar`, original aligned GP kernel, seed 12345, 16 GiB guest RAM,
one host CPU, and 10,000-transfer compose file were held constant. Every
run passed 10,000/10,000 transfers with zero failures and zero reverts.
The final candidate/control/candidate series gave:

| Module | Fair interval | Whole-VM NPT faults | Total exits | Entry cycles |
| --- | ---: | ---: | ---: | ---: |
| Cached boundary proof | 38.807 s | 60,488,972 | 81,300,094 | 189.39 billion |
| Unchanged control | 39.493 s | 60,867,171 | 81,816,350 | 190.20 billion |
| Cached boundary proof | 38.809 s | 60,355,016 | 81,178,594 | 190.34 billion |

The candidate runs were **0.684–0.686 s faster** than the adjacent
control, with **0.38–0.51 million fewer NPT faults**. An earlier unchanged
control on the same box took 39.736 s. The observed gain is modest and does
not close the gap to the historical Intel run on different hardware. Build,
suite, full-run, and counter logs are in ignored
`target/boxctl-evidence/amd-boundary-cache-1013/`. All module builds and
loads occurred inside the disposable boxctl box.

### Check the boundary cache against the shaped Tempo workload (2026-10-13)

Commit `3685477` and its parent `5289f93` were built as separate modules on
one EPYC 4245P HWE 7.0.0-38 box. The CLI, one CPU pin, seed 12345, 16 GiB
guest, source-patched GP kernel (SHA-256 `8735375b…`), and shaped OCI archive
(SHA-256 `05cf1c7e…`) were held constant. The initial cached/control/cached
sequence used the original pinned initrd; all 10,000 transfers passed in
**29.210 / 29.346 / 28.081 s**. The large spread between the cached runs
prevents treating this as a precise cache speedup.

The follow-up isolated the hot Nix glibc page-boundary hazard. The libjq-only
initrd relocates its REP but leaves glibc's `0xc2fff` boundary intact. The
second initrd additionally relocates that boundary; neither module nor OCI
image changed between those two guest layouts. Fair marker snapshots count
only the 10,000-transfer interval:

| Module | Initrd | Fair interval | NPT faults | Synthetic MTF exits | Entry cycles |
| --- | --- | ---: | ---: | ---: | ---: |
| Cached | libjq-only | 28.025 s | 9,216,687 | 1,147,493 | 35.80 billion |
| Cached | libjq + boundary | 26.304 s | 8,280,629 | 1,143,537 | 33.86 billion |
| Parent control | libjq-only | 28.561 s | 9,391,001 | 1,197,555 | 36.05 billion |
| Cached | libjq + boundary | 27.257 s | 8,586,976 | 1,161,945 | 34.73 billion |
| Cached | libjq-only | 27.903 s | 9,106,810 | 1,160,746 | 35.56 billion |

All five runs passed 10,000/10,000 transfers with zero failures or reverts.
The cached module is **0.54–0.66 s faster** than the one same-box parent
control on the libjq-only layout, with **0.17–0.28 million fewer NPT faults**.
Even with that cache, physically removing the glibc boundary saves
**0.65–1.72 s** and **0.52–0.94 million NPT faults** relative to adjacent
libjq-only runs of the same module. The exact interval varies with the host
and workload. The cached proof suppresses a per-entry breakpoint under a
guarded root, but it cannot mark the hazardous physical page globally trusted:
another address space may give its trailing `0x0f` an unsafe continuation.
One possible hypervisor path is a sound context-scoped execute proof that
revokes on CR3 changes, translation-tree changes, current/next-page writes,
and host mapping edits. Source-level removal of the boundary is another
route; neither has been implemented for this glibc page.
The libjq and boundary initrds are exact-build diagnostics, not production
guest rebuilds. Full logs and fair-marker JSON are in ignored
`target/boxctl-evidence/amd-boundary-shaped-1013/`. All module operations
occurred inside the disposable box.

### Try a CR3-scoped executable lease for the hot boundary (2026-10-13)

An isolated hypervisor candidate temporarily promoted a guarded page whose
sole `0x0f` boundary had a safe continuation under the current CR3. Before
each guest entry it checked the guarded root, current-page write-protected
hazard memo, NPT mapping generation, and trusted-code generation. It revoked
the executable leaf on changed context, page-table guard, code proof, or
mapping. It also cleared a selected counted plan if plan construction itself
revoked the lease. On a write fault, the recurrent code path dropped the
trusted leaf before restoring write permission. Forked AMD NPTs began with
all child code NX, because a child has none of its parent's scoped proof
state. These are safety requirements for any later version of this approach.

The 19 EPT and 268 VMX library tests and full EPYC 4245P HWE box suite passed,
including smoke, transitions, fork replay, mixed memory, and native loops.
The same shaped OCI, libjq-only initrd, source-patched GP kernel, CLI, seed,
CPU pin, and 10,000-transfer command were used on one box. Every full run
passed 10,000 transfers with zero failures or reverts:

| Module | Fair interval | Fair NPT faults | Fair MTF exits | Fair entry cycles |
| --- | ---: | ---: | ---: | ---: |
| Initial scoped lease | 28.267 s | 9,053,992 | 1,180,304 | 37.44 billion |
| Pushed control | 28.588 s | 9,401,222 | 1,183,443 | 36.79 billion |
| Scoped lease with plan-revocation fix | 28.419 s | 9,209,479 | 1,187,185 | 37.01 billion |

The corrected candidate saved **0.17 s** and **0.19 million NPT faults**
against its one same-box control, while adding **0.22 billion entry cycles**.
This is a much smaller gain than physically removing the guest boundary, and
one control does not establish a stable percentage. The large safety surface
is not justified by this result, so the scoped lease is **not merged**.

Two diagnostic runs logged lease activity while passing the complete
workload. The last periodic snapshots showed roughly **0.44–0.49 million
lease arm/revoke pairs** and **139–153 million active-lease checks** per VM
run. About **0.36–0.41 million** revocations came from guarded-tree state,
**0.07–0.08 million** from CR3 changes, and around **5,000** from mapping or
trusted-code generation; none came from hazard-memo eviction. Removing the
scoped lease's check of the scratch tree generation did not materially alter
the outcome (diagnostic fair intervals **28.322 / 28.326 s**). The guarded
tree often becomes dirty or unavailable, so the page rarely stays executable
long enough to replace the guest relocation. A later hypervisor attempt would
need to revalidate and renew the lease after guarded-tree refresh, while
preserving the fork, write, and plan-revocation invariants above. The exact
candidate patches, complete logs, and diagnostic dmesg are in ignored
`target/boxctl-evidence/amd-scoped-boundary-1013/`. No module was built or
loaded on the workstation.

### Renew the scoped boundary lease after guarded-tree refresh (2026-10-13)

An isolated follow-up deferred revocation while the guest was stopped for a
guarded-tree refresh, then revalidated the current page's sole alias, the
next-page translation and bytes, and both NPT code guards before the next
VMRUN. A changed or unavailable proof still revoked the executable leaf.
A focused test also covered an unsafe next-page remapping during refresh.
The 19 EPT and 268 VMX library tests and the EPYC 4245P HWE box suite passed.

The candidate and pushed control ran candidate/control/candidate on the same
box, pinned to one CPU, with the same seed, 16 GiB guest, source-patched GP
kernel, libjq-only initrd, and shaped OCI archive. All three passed 10,000
transfers with zero failures or reverts. Fair markers cover only the transfer
interval:

| Module | Fair interval | Fair NPT faults | Fair MTF exits | Fair entry cycles |
| --- | ---: | ---: | ---: | ---: |
| Lease renewal | 27.805 s | 9,030,564 | 1,154,346 | 36.08 billion |
| Pushed control | 27.908 s | 9,236,438 | 1,160,795 | 35.70 billion |
| Lease renewal | 27.725 s | 9,052,465 | 1,163,321 | 35.78 billion |

Renewal saved **0.10–0.18 s** and **0.18–0.21 million NPT faults** against
the one same-box control. This small, variable gain does not justify the
additional scoped-execute safety state; the candidate remains **unmerged**.
Its patch, full logs, marker JSON, and box suite logs are in ignored
`target/boxctl-evidence/amd-scoped-renew-1013/`. The ephemeral box was
deleted. No module was built or loaded on the workstation.

### Host perf guest-only counter on pre-PmcVirt EPYC (2026-10-13)

An isolated candidate tested a host `perf_event` counting AMD guest-only
retired instructions on an EPYC 4244P without SVM PMC virtualization. Linux
perf delivered guest-only overflows, so the candidate armed a physical PMI
before VMRUN and read the event after exit. It retained the existing PmcVirt
path on newer CPUs. A 4,096-instruction advance margin was needed because
physical PMI delivery lagged the requested period in short hardware tests.
The initial candidate missed a deadline after forced CPU migration; the
follow-up allowed this fallback only with a single-CPU affinity mask and
permanently disabled it for a VM after a CPU change.

The pinned hardware benchmark passed at **270,338 instructions in 0.037 s**
(**7.30 million instructions/s**). An unpinned mixed-memory sample passed
**37.75 million instructions in 0.0154 s**, with 6,179 exits versus
0.00328 s native. A forced migration during a longer guest run preserved its
checksum and exact **3.687 billion** instruction count, but triggered the
conservative path: **458,590 exits**, 1.178 s guest versus 0.387 s native.
The 19 EPT, 268 VMX, and 23 SVM library tests passed. These focused results
do not establish full-guest correctness or safety for the new counter path.

The same shaped Tempo boot, pinned to CPU 1, advanced only to guest virtual
time 51.445 after roughly seven minutes of host time. It then failed before
any fair transfer marker with `SVM PMU read failed` on exit code `0x61`
(NMI); the exact reason the perf event stopped is undetermined. A five-second
host-cycle profile during the boot assigned 33.17% of samples to
`svm_run_guest`, 12.87% to `handle_run`, 7.17% to `prepare_global_into`,
and just 0.84% to `perf_event_period`. This suggests that reprogramming the
event is not the dominant sampled cost, although the profile does not
explain the failed read. Because the boot did not reach the transfer interval,
there is **no comparable Tempo throughput measurement** for this candidate.
The existing pushed implementation remains the validated path, with shaped
same-box 10,000-transfer intervals around 26–28 s in prior runs. The host
perf candidate is **not merged**.

The candidate patch, logs, local test results, host details, and profile
summary are in ignored `target/boxctl-evidence/amd-host-pmc-1013/`. All
module building and runtime tests took place inside a disposable boxctl box;
no module was built or loaded on the workstation.

### Limit optional-page widening to two recent pages (2026-10-13)

An isolated candidate kept the predicted successor but screened only the two
most recent pages for a counted global plan, rather than four. This reduces
planner work while leaving omitted pages NX. Two of 268 VMX library tests
failed because they explicitly require the three- and four-page breakpoint
coverage that the candidate removes; the in-box SVM smoke, transition,
benchmark, and mixed-memory suites passed. The candidate was measured only
to determine whether the cheaper planner could outweigh its extra NPT faults.

The pushed module and candidate ran with identical guest artifacts on one
EPYC 4245P HWE 7.0.0-38 box: the pinned CPU, seed, 16 GiB RAM, exact-build
GP-relocated kernel, libjq-relocated initrd, shaped OCI archive, compose file,
and CLI were held constant. Both completed 10,000 transfers with zero
failures or reverts:

| Module | Fair interval | Fair NPT faults | Host time to first marker |
| --- | ---: | ---: | ---: |
| Pushed control | **28.531 s** | **9,357,302** | **121.436 s** |
| Two recent pages | 29.182 s | 10,201,306 | 133.766 s |

The candidate added **844,004** transfer-interval NPT faults, was **0.651 s**
slower in that interval, and reached its first marker **12.330 s** later.
This single pair is not a precise percentage estimate, but both the lost
planner coverage and the extra faults point against the change. It remains
**unmerged**. The unchanged module was restored and passed SVM smoke before
the box was deleted. The source diff, test output, full logs, markers, and
stats are in ignored `target/boxctl-evidence/amd-widen2-1013/`. No module was
built or loaded on the workstation.
