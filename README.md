# Bedrock

An experimental x86-64 hypervisor purpose-built for deterministic software
testing.

Bedrock uses Intel VT-x, or its experimental AMD SVM backend, to run guest VMs with fully emulated time (TSC),
controlled randomness (RDRAND/RDSEED), various other device emulation, and
copy-on-write VM forking - enabling reproducible execution for deterministic
testing.

<a href="https://asciinema.org/a/icy1rkUAHbCEQsRN" target="_blank"><img src="https://asciinema.org/a/icy1rkUAHbCEQsRN.svg" /></a>

## Architecture

```
┌────────────────────────────────────────────────────┐
│                    User Space                      │
│                                                    │
│  bedrock-vm      Rust library for VM control       │
│                                                    │
│                        │ ioctl                     │
├────────────────────────┼───────────────────────────┤
│                        ▼                           │
│                   Kernel Space                     │
│                                                    │
│  bedrock.ko      Kernel module (/dev/bedrock)      │
│                  - VT-x/SVM setup and VM execution      │
│                  - EPT/NPT memory virtualization       │
│                  - Deterministic device emulation  │
│                  - ...                             │
│                                                    │
└────────────────────────────────────────────────────┘
```

## Requirements

- Host kernel with `CONFIG_RUST=y`: the Nix build uses [Linux 6.18]; the AMD
  backend has also been tested on Ubuntu Linux 7.0.0-38-generic (see below).
- Patched linux 6.18 guest kernel (see [guest-patches/](guest/patches/))
- The accelerated Intel backend requires a modern Intel CPU and the feature
  `EPT-friendly PEBS`, which was introduced in the Ice Lake-SP
  microarchitecture. Therefore, Ice Lake-SP CPUs (or newer) should work.
  
  The following CPUs have been confirmed to work:
  - `Intel(R) Xeon(R) Gold 5412U` (rented on [Hetzner])
  - `Intel(R) Xeon(R) Silver 4310` (rented on [Worldstream])
  - `{r,m}7i.metal-24xl` instances on [AWS].

[Linux 6.18]: https://github.com/torvalds/linux/tree/v6.18
[Hetzner]: https://www.hetzner.com/dedicated-rootserver/
[Worldstream]: https://www.worldstream.com/en/dedicated-servers/
[AWS]: https://aws.amazon.com/de/ec2/instance-types/

## Experimental AMD backend

The AMD backend implements SVM entry/exit, VMCB state, nested page tables,
and MSR/I/O intercept bitmaps. The common device, randomness and fork logic is
shared with the Intel backend. AMD execution batches verified straight-line
instructions and bounded REP stores, and uses hardware-swapped guest IRPERF
for verified regions containing conditional branches and relative jumps.
One SVM entry tick is removed from that count; REP iterations
use RCX accounting. Code and translation guards prevent a batch
from modifying the instructions it has decoded. Long-mode MOV stores and PUSH
can join a batch when their address registers retain their entry values and
their destinations cannot rewrite the code or subsequent store translations.
These stores can also join forward-only branches when their address registers
are stable across every decoded path.
Counted MOV-store loops using DEC on ECX, EDX, RCX, or RDX, an advancing RDI,
and JNZ run in bounded chunks after validating each chunk's destination range.
Entry temporarily clamps the counter to iterations that fit before the deadline;
exit restores its architectural value and DEC flags, including 32-bit zero
extension, and rewinds an artificial loop termination. The counter and exit
instruction boundary give the exact retirement count without a performance-counter
interrupt margin. Stores reading the selected counter, including CL/CH or DL/DH
aliases, use the stepping fallback.
The range must map to contiguous physical pages and cannot overlap code or
any page table used by the code or destination translations.
Decoded long-mode memory blocks also use hardware-counter execution with code
and every reachable page-table frame write-protected, including loops whose
store addresses change. A guarded write fault restores permissions and replays
the instruction with stepping before rebuilding the proof. Direct branches
can enter decoded boundaries or stop before up to three outgoing targets using
execution breakpoints, with the fourth slot reserved for the block endpoint.
Straight-line blocks and outgoing branches with unambiguous prefix lengths use
exact decoded counts without arming the PMU; internal branches and ambiguous
paths use IRPERF. Other instructions terminate the block. Without table-guard
support or enough deadline margin, stores need
static destination proofs or fall back to stepping. Unsupported instructions
also use stepping.
Bounded native REP MOVS/STOS chunks are marked as RAM writes and revoke
cached code and table proofs before entry. Their destination validation protects
the current execution; it does not protect unrelated cached pages that the copy
may rewrite. A hardware regression warms a code page, installs RDRAND there
with REP MOVSB, and checks that re-entry traps for controlled randomness.
REP entry points bypass whole-page execution, which would stop immediately at
the string-instruction breakpoint and fall back to one scalar iteration. They
instead use the validated, deadline-bounded native chunk directly. A 10,000-byte
REP copy completes in seven exits on the validation host.
When a global gate does stop at a REP hazard breakpoint, its unretired
instruction can also enter that bounded chunk after the boundary instead of
forcing one scalar iteration. If the chunk cannot be proved, scalar execution
remains the fallback.
Control-flow acceleration requires AMD PerfMonV2, PMC virtualization, virtual
NMI, and IRPERF enabled by the host kernel; unavailable features or a failed
perf counter reservation disable it. VMRUN saves and restores the host's
performance counters in hardware. A guest programmable counter drives the
overflow interrupt while the dedicated guest IRPERF supplies instruction counts;
the programmable retired-instruction event can overcount at nested page faults.
Counter overflow raises a virtual NMI. During counted execution a temporary
zero IDT limit traps its delivery before an interrupt frame is written, and
SIDT/LIDT intercepts stop execution before observing or changing that limit.
Guest IDT and interrupt controls are restored before handling the exit.
Counted regions also intercept IRET and replay it with exact stepping: a native
region containing IRET retired 45 instructions in the stepped reference but
reported 44 on the validation host. Scalar IRETQ retains RAM proofs only when
its descriptor accessed-bit updates cannot modify protected code or table
frames; CET-enabled transitions revoke them.
Forward-only regions stop at their endpoint and can run inside
the performance-counter interrupt margin when their maximum instruction count
fits before the next deadline. Backward branches still require that margin.
Long-mode execution can also run across calls, returns, and store loops within
up to 64 recently visited code pages. Hardware execution breakpoints stop
before possible RDRAND/RDSEED/RDPID, repeated-string encodings, and SYSRET;
their prefix entry points and executable virtual aliases must fit in four
breakpoint slots. Alias enumeration uses a preallocated workspace of 512
virtual table paths and falls back if that capacity is exceeded. When alias
breakpoints do not fit, a reachable-region proof can instead stop at unknown
instructions and indirect transfers before they can reach an unguarded alias.
SYSRET uses the scalar path because it can restore guest
TF from R11. Ambiguous encodings across page boundaries use the fallback.
A temporary NPT guard makes the selected code pages read-only and blocks
instruction fetch from every other page. It also protects all reachable guest
page tables, preventing translation changes during the run. The code and guest
table frames must fit in a preallocated workspace of 128 table frames;
larger trees use the
verified-region fallback. This path requires AMD ROGPT
(CPUID 8000000A:EDX[21]); VMCB nested-control
bit 6 makes page-table walks request nested writes only for actual A/D updates.
Those updates and explicit table writes end the run and use stepping before
replanning. See [AMD APM, section 15.25.5](https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2).
The counter accounts for all retired instructions;
a transition outside the guarded pages ends the run and validates the new page.
Non-writing entry instructions can replan directly; entry stores, split
instructions, code/table writes, and other intercepts use scalar replay after
timer handling. Guest exceptions
and software interrupts are delivered through the shared exit handlers.
Guest single-step traps use
the scalar path and are reported after retirement. Page hazard scans are cached
within a RUN while guarded execution protects their bytes or scalar stores are
proven disjoint from the cached pages. Pages omitted from a guard lose their
scan approval before execution; emulation and a new RUN revoke approvals.
Native page entries and decoded store blocks also write-protect previously
scanned code omitted from their executable set, within the existing guard
workspace capacity. Those pages remain guarded as data, allowing their hazard
scans to survive the entry without another full-page byte comparison. A write
fault on such a page restores permissions and uses scalar replay; pages that
cannot be guarded lose their scan approval.
Up to 32 alias proofs are reused while the table-frame proof remains valid, with
hazardous physical pages and hazard offsets as the cache key, independent of
selection order and ordinary code pages. Cached edge summaries check cross-page
boundaries on each preparation. Optional pages that cannot fit the four
breakpoints are omitted before alias enumeration.
Reachable-region proofs are tied to exact code bytes and recheck outgoing
branch translations before reuse.
Page-execution plans also reuse their selected pages and breakpoints for a
repeated RIP/CR3 pair while the guarded translation tree and selected code
proofs remain valid. The current instruction deadline is applied on each reuse.
NPT permission guards reuse a heap workspace and cached executable-entry masks;
write guards restore leaf permissions directly without another table walk.
Optional code-page translations are cached while the guarded guest table proof
remains valid; revoking that proof or changing CR3 rebuilds the cache.
Code-page hazard scans can survive proof invalidation through an exact-byte
cache. All 4KB are compared before reusing a scan; changed bytes are rescanned.
The 64-page byte cache adds approximately 256KB to each VM's heap workspace.
Page execution requires more than 256 instructions before the next deadline.
The largest observed PMC overflow lag was 135 retired instructions in a
200-million-TSC checkpoint and a complete boot on the validation host. The
margin is not a calibrated bound for every AMD processor; a late exit fails
closed rather than returning an incorrect instruction count.
On a boxctl EPYC 4245P, reducing the margin from 512 to 256 cut pinned Linux
boot-and-fork replay's five-run median from 3.40 to 2.85 seconds, with the same
snapshot instruction count and RAM hash. Sixteen further full runs and fresh
replays matched. Also, 256 accelerated forks matched an instruction-stepped
reference at ten checkpoints
from 8.00 to 8.10 million instructions and 64 forks at ten checkpoints from
100.00 to 100.10 million instructions.
An instrumented Linux boot/replay on that EPYC 4245P counted the first million
SVM entries: 526,656 global batches, 149,182 page batches, 108,331 bounded
batches, and 215,831 scalar entries. The normal run still makes about 880,000
VM exits and takes about 2.85 seconds; the near-native synthetic loop result
does not describe this boot. Attempts to shorten instruction fetches, retain
the global gate across ordinary COW remaps, or remove redundant VMCB/PMU writes
did not produce a repeatable Linux speedup and were reverted. The remaining
work is to reduce the Linux exit rate and per-entry cost while preserving exact
instruction counts and replay state.
In a box-only diagnostic over one million entries, the SVM runner spent about
4.53 billion cycles between calling its assembly entry and returning from it;
this interval includes guest execution. Temporarily omitting XSAVE/XRSTOR
reduced that interval to 3.80 billion cycles, and omitting XCR0 switching as
well reduced it to 3.55 billion. Both changes violate FPU isolation or
guest-visible XCR0 semantics and were reverted. They bound the benefit of
FPU-switch optimization on this workload; reducing VM exits remains necessary.
On an EPYC 4245P box, using XSAVEOPT only for the guest image immediately
following XRSTOR of that same image reduced a five-run Linux boot/fork median
from 2.885 to 2.867 seconds in a fresh same-box A/B run, saving about 49
million VM runner cycles (0.6% wall time). Earlier comparisons showed larger
but variable gains. Extending XSAVEOPT to the host image passed the FPU test
but did not improve this workload, so the host image still uses full XSAVE. CPUs
without XSAVEOPT keep the original guest save. Each guest image receives a
full XSAVE before XSAVEOPT, repeated after guest XCR0 changes. A hardware test
preserves a guest XMM value and a host x87 value across VM exits and two forks;
the Linux snapshot and RAM hash matched in all measured runs. This is a modest
entry-cost improvement, not a solution to the repeated execute faults.
The SVM entry assembly now swaps only the enabled DR0-DR3 breakpoint address
registers. On the same EPYC box, a five-run Linux baseline median of 2.888
seconds compared with 2.846 and 2.855 seconds in two modified five-run passes;
all runs matched the snapshot and RAM hash. This reduces per-entry debug-state
work but does not address the repeated faults on hazardous code pages.
Skipping the host DR7 disable write when DR7 is already `0x400` reduced a
same-box five-run median from 2.862 to 2.805 seconds; a second modified pass
was 2.837 seconds. Both used the selective DR0-DR3 swap and matched replay
state.
VMEXIT also disables host breakpoints, so an already-disabled saved DR7
(`0x400`) needs no restore write; other host DR7 values are still restored.
Against the preceding entry-only optimization, a same-box five-run Linux
median fell from 2.854 to 2.800 seconds, then to 2.769 seconds in a second
modified pass. Median VM runner cycles fell from 5.111 billion to 4.952 and
4.911 billion. All runs matched the snapshot and RAM hash.
A boxctl EPYC 4245P profile classified the first 350,000 nested-page faults
in the Linux boot/fork run: 326,702 (93.3%) selected scalar execution on an
untrusted code page, 18,464 released a page-table write guard, 3,587 trusted a
safe code page, 677 handled copy-on-write, and 570 invalidated trusted code.
Samples of the scalar fetch faults clustered on Linux's `memcpy`/`memmove`
page (`0x1ed1000`) and `insn_decode` page (`0x1ecc000`), with the global
page-table gate ready. This points to repeated execution of pages containing
instruction hazards, rather than gate refresh or COW, as the main fault source.
An EPYC 4245P box-only experiment used AMD's DR address-mask MSRs to fit the
seven hazards on these two pages into four instruction breakpoints and permit
both pages to execute under the existing global gate. The Linux boot/fork
replay matched the baseline snapshot TSC (`499445737`) and RAM hash
(`dd6a53449a59f0aa`). Promoting both pages from every eligible trusted-page
entry reduced exits from 844,300 to 697,715, but increased entry-preparation
cost from 4.82 to 6.13 billion cycles and root runtime from 2.83 to 3.00
seconds. Promoting them only when starting on either hot page made 773,769
exits and a 2.80-second root run, which is too close to the baseline to claim
a speedup from one sample. The hardcoded page promotion and mask swapping were
reverted; a general cached proof and lower entry cost would be needed before
using masked breakpoints in production.
A second box-only upper-bound diagnostic reused those pages' virtual-alias
breakpoint plan after its first validation, deliberately omitting code-write
invalidation. Six Linux runs passed the snapshot and RAM-hash replay checks
and made about 692,000-694,000 exits. Their 2.77-second median was effectively
the same as the unchanged branch's 2.76-second median over six nearby runs,
despite roughly 150,000 fewer exits. Its entry-preparation cost was about
4.83-6.05 billion cycles across the runs, versus 4.45-4.76 billion for the
control. The unsafe cache was discarded; this experiment does not justify
persistent masked breakpoints without a cheaper setup path.
A same-box `RDPMC` trial replaced the host PMC0 `RDMSR` in SVM entry with an
ordered `RDPMC` read. The hardware suite and Linux replay passed, but two
six-run modified passes had Linux medians of 2.74 and 2.75 seconds against a
2.72-second control pass. VM runner-cycle medians overlapped as well. The
change was reverted: the replacement counter read did not produce a repeatable
end-to-end improvement on this workload.
Recurrent hazardous pages now retain their full-page scan under a persistent
NPT write guard. A page is selected only after 16 repeated fetches, with an
eight-page cap. Guest writes release the guard and invalidate the proof;
Bedrock host writes, RUN boundaries, and changed NPT mappings force a byte
recheck before reuse. On a boxctl EPYC 4245P, the final guarded-cache variant
had a six-run Linux boot/fork median of 2.66 seconds, versus 2.78 seconds in
the same-box control. Exit counts remained about 844,000, but median exit
handling fell from 420 to 265 million cycles and median VM-entry preparation
from 4.74 to 4.45 billion cycles. The hardware suite, exact-deadline checks,
two fresh Linux boots, fork replay, snapshot TSC, and RAM hash matched. This
is a roughly 4.5% improvement to Linux runtime, while the broader near-native
goal remains unmet. As with the existing globally trusted code path, direct
userspace writes through a RAM mapping must occur between RUN calls; writes
performed by Bedrock while RUN is active invalidate the cached proof.
Revisiting the masked-breakpoint hot-page probe with these guarded scans still
failed to improve Linux runtime. Promoting both pages from trusted entries
made about 699,000 exits but a six-run median of 2.82 seconds, with entry
preparation rising to roughly 5.4-5.9 billion cycles. Restricting promotion
to starts on either hot page made about 771,000 exits and a 2.66-second
median, effectively equal to the guarded-cache baseline. Both hardcoded
probes were reverted. The remaining cost lies in arming and protecting the
promoted pages on each entry, not only in revalidating their bytes.
An isolated Linux-entry profile attributed about 0.59 billion cycles over
200,000 unsafe global-gate successes to executable-alias collection. A
direct-mapped alias-plan cache hit on roughly 177,000 of those successes, but
about 14,000 full page-table walks still dominated the stage; its measured
cost only fell to about 0.56 billion cycles. Repeated six-run wall-time passes
did not show a stable benefit, so the experimental cache and temporary
profiling counters were discarded. The remaining alias work is concentrated
after guest page-table writes invalidate the global table guard.
On an EPYC 4245P boxctl box running HWE `7.0.0-38`, a clean-build
1,048,576-pass memory workload retired 38.659 billion guest instructions in
3.152 seconds (three-sample median), versus 3.147 seconds natively. That is
12.27 billion retired guest instructions per second, not a 12.27 GHz clock.
The same box's Linux boot and two-fork replay took 2.733 seconds with 847,994
root VM exits and the expected snapshot TSC and RAM hash. The memory-loop
result therefore does not establish near-native Linux performance.
A later leaf-table refresh shortcut rearmed only released leaf NPT guards
instead of rebuilding the complete guarded tree. Its nearby six-run control
median was 2.682 seconds and the eight-run shortcut median was 2.618 seconds,
but correctness stress exposed a rare one-instruction difference: two of 36
shortcut Linux boots reached snapshot TSC `499445738` rather than
`499445737`, changing the guest time report, while 60 same-box control boots
all reached `499445737`. The shortcut was reverted. This is a correctness
regression candidate, not a usable speedup; the source of the extra tick
remains to be identified before any similar tree-refresh optimization.
A stricter leaf-only refresh now requires the NPT mapping generation to match
the last full tree guard. Any COW remap or other changed NPT mapping takes the
complete refresh path. This closes a gap in the first shortcut, which trusted
guard metadata even when an NPT mapping had changed. On the same EPYC 4245P
box, the 40-run control median was 2.678 seconds and the revised shortcut's
80-run median was 2.620 seconds (about 2.2% faster). All 80 revised boots
matched snapshot TSC `499445737` and RAM hash `dd6a53449a59f0aa`; a fresh
two-root replay and the box hardware suite passed. The exact cause of the
first shortcut's rare extra tick is not yet independently isolated, so this
optimization needs continued replay testing on other AMD boxes.
A box-only diagnostic confirmed that this fallback is exercised: it observed
NPT mapping-generation changes during the root boot as early as emulated TSC
2 and again near TSC 200,000, plus changes in both forked children. The
temporary counters were removed after the check.
On a second EPYC 4245P box with the same HWE kernel, ten modified Linux
boot/two-fork runs had a 2.567-second median versus 2.588 seconds for ten
nearby control runs (about 0.8% faster). Every run matched the snapshot TSC
and RAM hash. This supports a smaller cross-box gain than the first box's
2.2% estimate, and leaves the broader near-native target unmet.
A separate EPYC 4245P diagnostic counted the first million SVM entries during
Linux boot and fork replay: none had guest CR0.TS or CR0.EM set, and the guest
XCR0 was `0x7` versus the box host's `0x2e7`. Skipping FPU state switching
only while the guest disables FPU access, or skipping redundant XCR0 changes,
therefore cannot improve this workload. The diagnostic was removed.
An isolated assembly profile measured about 800 million cycles in FPU state
switching over the first 800,000 Linux SVM entries, or roughly 1,000 cycles per
entry. The SVM runner now reserves the host FPU state across at most 64 run-loop
iterations or two million host TSC ticks. It still saves and restores guest
XSTATE on every entry and restores host XCR0 before each host IRQ window. The
reservation begins and ends with host interrupts enabled; a window rotation
briefly releases it so softirqs can run. On the same EPYC 4245P box, six nearby
clean-branch Linux boot/two-fork runs had a 2.585-second median versus 2.454
seconds for six runs with the reservation (about 5.1% faster). Median SVM
runner cycles fell from 4.80 to 4.38 billion while exit counts stayed near
845,000. All runs matched snapshot TSC `499445737` and RAM hash
`dd6a53449a59f0aa`. The hardware suite, a 320-CPUID guest XMM/host x87
isolation check, two fresh Linux boots, and a 38.659-billion-instruction memory
workload passed. The memory workload remained near native speed: 3.158 seconds
guest versus 3.153 seconds native. General near-native AMD execution remains
unfinished because most Linux time still comes from VM exits and batch planning.
The same FPU reservation now also keeps guest register state live across SVM
entries, skipping guest XRSTOR after the first entry in each group. Guest XSAVE
still updates the memory image on every exit. A reservation rotation, RUN
boundary, or guest XSETBV invalidates the live-register shortcut. In a
same-box ten-run comparison against host-only FPU batching, Linux median time
fell from 2.465 to 2.417 seconds (about 2.0%) and median SVM runner cycles
from 4.40 to 4.15 billion (about 5.7%); exits stayed near 843,000. The SVM
hardware suite, guest XMM/host x87 rotation check, 24 further exact Linux
boot/fork replays, two fresh Linux boots, and the long memory workload passed.
That workload remained near native at 3.157 seconds guest versus 3.152 native.
A box-only stage profile of the same Linux boot and two forks attributed about
2.73–2.84 billion cycles over roughly 1.05 million entries to batch planning,
versus 0.43–0.48 billion each for global-tree refresh, instruction-window
preparation, and execution protection. Within planning, the global-gate attempt
used about 1.1 billion cycles and the fallback batch planner about 1.6 billion.
The NPT trusted-code lookup and deadline calculation each cost only about
29 cycles per measured call, so moving those checks did not improve the next
Linux run. These figures include temporary `RDTSC` instrumentation and the
forked children; they are diagnostic attribution, not an uninstrumented
root-runtime breakdown. The temporary instrumentation was not committed.
A box-only trial deferred guest XSAVE until FPU-reservation rotation, guest
XSETBV, or RUN completion. It passed the hardware suite and exact Linux replay,
but six alternating same-box comparisons (12 roots per variant) had essentially
equal medians: 2.398 seconds for the current code and 2.401 seconds with
deferred saving. The added state-management complexity was discarded.
A host-cycle diagnostic then identified repeated 4KB code-image comparisons in
the hazard memo as a significant planning cost. Hardware `perf` sampling
perturbed Bedrock's guest instruction counter, so it was used only to identify
the hot function; all results below use uninstrumented runs. The root and fork
memory backends now compare unchanged code images eight 64-bit words at a time,
branching once per 64 bytes while still checking every byte. In a pinned,
reversed-order same-box comparison of five Linux boot/two-fork replays per
variant, the ten-root median fell from 2.367 to 2.297 seconds (about 3.0%).
Median VM-entry preparation fell from 4.145 to 3.870 billion cycles, with about
846,000 exits in both variants. Six further alternating unpinned comparisons
also favored the change, though those timings were more variable. Every replay
matched snapshot TSC `499445737` and RAM hash `dd6a53449a59f0aa`; the SVM
hardware suite, byte-comparison unit tests, and the 38.659-billion-instruction
memory workload passed. General near-native Linux execution remains unfinished.
The alias-breakpoint page-table walk now skips absent or non-executable entries
before calling its per-entry visitor; the visitor retains the same check as a
backstop. In five pinned same-box Linux boot/two-fork replays per variant, the
ten-root median fell from 2.334 to 2.131 seconds (8.7%) and median VM-entry
preparation fell from 3.932 to 3.266 billion cycles. Reversing A/B order in
another five paired replays gave 2.289 versus 2.182 seconds (4.7%). Exit counts
stayed near 845,000. Every replay matched snapshot TSC `499445737` and RAM
hash `dd6a53449a59f0aa`; the SVM hardware suite and long memory workload
also passed. This removes avoidable host work per exit but does not yet prove
roughly 5% overhead for general Linux execution.
The same walk now filters 4KB leaf mappings whose physical page is not among
the selected code pages; upper-level large-page entries retain the original
visitor path. Five paired, pinned Linux replays in each A/B order passed exact
boot/fork checks. The ten-root medians changed from 2.142 to 2.114 seconds in
one order and from 2.144 to 2.133 seconds in the reverse order. Median
VM-entry preparation fell from 3.248 to 3.075 billion cycles and from 3.266
to 3.155 billion cycles, respectively. The wall-time gain is small relative
to run-to-run noise, but the measured preparation cost fell in both sets.
A narrower box-only upper-bound probe promoted just the recurrent Linux
`memcpy`/`memset` page using three debug breakpoints and one address-mask MSR.
Six nearby clean-branch runs had a 2.576-second median and about 843,000 exits;
six probe runs had a 2.737-second median and about 820,000 exits. Every run
matched snapshot TSC `499445737` and RAM hash `dd6a53449a59f0aa`.
The probe's hardcoded page and cached alias proof lack code-write invalidation,
so it was discarded; even its exit reduction was too small to offset setup
cost. This reinforces that a useful accelerated path must cut planner and
entry cost as well as exit count.
A previous scalar-entry profile was contaminated by the boxctl test suite,
which ran before Linux without resetting module counters. Its roughly 148,000
entries on low page `0x1000` came from `svm_bench`'s explicit 0–40,000
single-step window, not Linux boot. That page exceeded the four-breakpoint
whole-page hazard capacity, but a counted-loop experiment there did not
improve Linux exits and was reverted. The earlier low-page attribution and
conclusion about Linux's scalar work were incorrect.
An isolated Linux run after reloading the module sampled every 1,000th scalar
entry: among 271 samples, 79 were on physical page `0x1253000` (including
`apply_returns`), 30 on `0x129a000` (`note_page`), 23 on `0x1252000`
(`text_poke_early`/`apply_alternatives`), 18 on `0x1545000`, and 17 on
`0x143c000`; none were on `0x1000`. 139 samples had a forced step pending.
On `0x1253000`, 49,968 of 50,000 scalar entries followed a global-gate
replay boundary, versus three following a write guard. The most frequent RIP
there was `iretq` in `apply_returns`; other repeated sites included `iretq`
in early text patching and `REP MOVS` in `memcpy_fromio`. The temporary
per-sample kernel logging slowed that diagnostic boot, so it is a hotspot
profile rather than a wall-time benchmark.
The bounded planner now validates consecutive `PUSH` destinations against
the progressively decremented stack pointer. It still stops after any other
RSP write. On the same EPYC 4245P box, the three-run control had a median
880,928 Linux exits versus roughly 852,500 across nine modified runs (3.2%
fewer). The snapshot count and RAM hash matched in every run, and a fresh
two-root replay passed. Wall-time medians were 2.77 seconds in the three-run
control and 2.80 seconds in the modified runs, so this change has not shown a
Linux wall-time improvement despite reducing exits.
The global-gate REP handoff then removed a further roughly 9,800 exits on the
same box: the three-run control median was 853,322 exits, and six modified
runs had a median of about 843,500. Three-run wall-time medians were 2.77
seconds for the control and 2.71 and 2.75 seconds in two modified passes;
these short timings remain noisy. An isolated diagnostic observed more than
8,000 REP hazard handoffs in the Linux run and a validated chunk on nearly
every one, mostly at `memcpy_fromio`. The snapshot count and RAM hash matched
in every A/B run. The full suite, including the REP exact-deadline regression,
passed; an earlier unsafe experiment that made whole REP pages executable
failed that regression.
A box-only diagnostic disabled the counted-region IRET intercept to estimate
its cost. Linux exits fell to 746,479 and wall time to 2.58 seconds, but the
snapshot TSC changed from 499,445,737 to 499,419,858 and the RAM hash
changed. That variant is incorrect and was reverted. It confirms that
removing IRET replay needs a precise retirement correction, not just a faster
control path.
On an EPYC 4245P (family 1Ah, model 44h), a box-only probe also enabled a
second virtualized PMC for retired far control transfers while temporarily
allowing native IRET. The counter reported a baseline event on entries with
no guest far transfer, and the Linux guest failed during early boot with an
event-vector error when IRET interception was removed. The small IRET
transition test passed, so it was insufficient to establish exact accounting
for a full guest. This probe was reverted; the regular boxctl suite passed
again with IRET interception restored.
An isolated Linux nested-page diagnostic saw no selected scalar page among
the first 300,000 execute faults. These faults occur as native execution
reaches untrusted code, so retaining a scalar page across scalar steps would
not remove them. Trying bounded page batches before the global gate cut one
run from about 382,000 to 208,000 nested-page faults and from about 843,000
to 666,000 total exits, but wall time rose from roughly 2.7 to 4.16 seconds:
VM-entry preparation grew from about 4.6 to 8.65 billion cycles. That broad
ordering change was reverted. A narrower cached-plan-first trial did not
complete the transition suite: the box stopped answering SSH during that
test, so the change was discarded without a performance claim. A fresh box
passed the unchanged suite.
On a later EPYC 4245P box, a fresh Linux boot with temporary kernel-only
diagnostic counters recorded 280,114 instruction-fetch faults in the first
300,000 nested-page faults; 277,076 fetches reached code the global gate
could not trust. Only 14,676 faults released protected page-table writes and
596 targeted trusted-code writes. Two physical pages accounted for 199,582
of those fetches (71%): 129,053 on the page containing `memcpy`/`memset`, and
70,529 on the page containing `insn_decode`. The former also contains four
`REP MOVS`/`REP STOS` instructions; the latter contains `RDRAND` and
`REP MOVSB`. Even entry into ordinary instructions on either page must fault
because the page also contains these hazardous instructions. The diagnostic
boot preserved the expected snapshot TSC and RAM hash; its counters were
removed after profiling. Faster handling of protected page-table writes
alone cannot remove the dominant exit source on this guest image.
An exact-breakpoint trial also let the `memcpy` page execute during counted
global regions, guarded its bytes, and armed breakpoints for every hazardous
alias. The boxctl suite and Linux replay passed, and one Linux run had about
22,000 fewer exits, but VM-entry preparation rose from roughly 4.8 to 6.2
billion cycles and wall time from roughly 2.8 to 3.24 seconds. It was reverted.
Separate temporary entry-stage counters found batch planning to be the largest
measured preparation stage. A per-code-page revision cache for bounded plans
passed the suite and Linux replay, but nine same-box Linux runs had medians
of 2.732 seconds with the change and 2.755 seconds without it, with heavily
overlapping runs. The added proof state and logic were discarded. The hot
faulting pages and the expensive bounded-plan pages were different; reducing
faults or planner work in isolation has not yet delivered the general target.
An isolated diagnostic that skipped code-byte rechecks for rejected pages
changed the five-run median only from 2.89 to 2.85 seconds on that box;
skipping rechecks for accepted pages failed the REP code-write regression.
Both diagnostic changes were reverted.
An unsafe, box-only upper-bound probe made the Linux `memcpy` physical page
globally executable despite its four `REP MOVS`/`REP STOS` hazards. One Linux
boot/fork replay fell from 2.93 seconds and about 880,000 exits to 2.13 seconds
and 664,446 exits, with the same final snapshot and RAM hash. The broader
REP-page rule failed the exact-deadline hardware test: after 10,000 REP
iterations the accelerated child reported instruction count 19, while the
stepped reference required 10,004. Thus the tempting exit reduction is not a
valid deterministic backend; both unsafe probes were reverted. A viable
solution must trap or account for REP iterations at every executable alias
and preserve mid-REP deadline behavior.
A safe negative-cache experiment compared rejected code pages only every 256th
visit, with immediate invalidation for host writes. It passed unit and hardware
tests, including Linux replay, but its five-run median was 2.885 seconds,
identical to the same-box baseline; it was reverted. On this Linux image, the
first million global-gate planning attempts included about 537,000 successes;
about 270,000 fallback bounded plans succeeded and cost roughly 1.56 billion
host cycles to prepare. Temporarily allowing one recent hazardous page during
otherwise trusted global batches passed replay checks but slowed the Linux
five-run median from 2.93 to 3.14 seconds, so it was reverted. Limiting this
promotion to a short window after the page was last executed also failed: a
one-entry window removed about 4,000 Linux exits without a clear speedup,
while a four-entry window
removed about 10,000 exits but raised VM-entry preparation by about 0.5 billion
cycles and slowed the five-run median to 2.93 seconds. Both were reverted.
Keeping a page plan's tree-generation key stable while reusing an unchanged
global table tree also passed checks but did not improve the median (2.902
versus 2.904 seconds);
it was reverted. The next optimization needs to reduce the number or cost of
bounded page plans without adding per-entry byte checks or breakpoint setup.
A single-page plan reuse probe under the global table guard found only 307
new cache hits in 500,000 calls; its five-run median was 2.897 seconds against
2.902 seconds for the nearby baseline. This was too small to justify its
full-page byte check and extra hot-path branches, so it was also reverted.
The hot `memcpy` page has four REP hazard-entry offsets (`0x5cc`, `0x7c1`, `0x7e8`,
`0x8ff`). A guarded global batch can cover them with all four hardware
breakpoints when its current translation tree exposes one executable alias,
but those breakpoints are available only for that batch. Retaining the page's
execute permission across other batches requires persistent hazard traps and
coordination with their own breakpoint slots. The tested EPYC 4245P advertises
[AMD's 32-bit instruction-breakpoint address-mask extension](https://docs.amd.com/api/khub/documents/sD1_QL~h4Afq2_tvzxqqSQ/content), but combining
these four offset values into two masks would trap many additional positions.
The table-frame list can be reused within a RUN while guarded execution,
known non-writing instructions (including ENDBR64 and conditional branches),
or MOV/PUSH stores proven disjoint from table frames preserve its shape.
PUSHF emulation checks every translated stack destination: table overlap
revokes the table proof, and code overlap revokes cached code scans.
Guarded counter entries run PUSHF natively when TF and RF are clear; the
counter exit restores interception before stepping can resume. NPT write
guards stop stores into protected code or tables. CPUID, RDTSC/RDTSCP, POPF,
and port I/O handlers retain still-valid RAM proofs; guest event delivery
revokes them before
writing an interrupt frame.
Other unproven writes, emulation, and a new RUN invalidate the table proof;
a changed CR3 rebuilds it.

Regions ending at an unconditional SVM intercept omit the execution breakpoint,
then replay that intercept after timer and deadline handling at the boundary.

This is partial acceleration, not an equivalent of Intel's PEBS execution path.
Linux still spends substantial time in the stepping fallback. Near-native
execution of general Linux workloads, with roughly 5% overhead as the target,
has not been demonstrated.
PMI skid can exceed the deadline margin; such a run fails instead of returning
an incorrect instruction count. A real-mode forward-branch deadline test
intermittently stopped one instruction away from its expected boundary.
Bounded non-paged forward branches now stop at either successor and use exact
prefix counts instead of IRPERF. The isolated deadline/fork test passes 1,024
consecutive repetitions; instruction-count correctness is not established
across all execution paths. Fresh 50-million checkpoint comparisons have also
reproduced a one-instruction divergence on the clock-proof baseline after a
module reload; the earlier matching pairs do not establish cold-run repeatability.

Native validation on an AMD EPYC 4585PX with Ubuntu Linux 7.0.0-38-generic
includes Linux 6.18 booting to userspace and matching executions of two Linux
forks with the reference stepping backend. The accelerated backend passes exact
Linux checkpoints with matching registers and guest-memory hashes, plus loop
and REP deadline/fork tests. Its complete Linux integration test also passes:
boot to a userspace snapshot, matching clock/randomness/registers/instruction
counts in two children, and isolation of the parent's memory.
Fresh roots have matched at the 100-million instruction checkpoint using guest
IRPERF; the earlier GuestOnly programmable-counter clock diverged there.
Earlier 50-million checkpoint pairs and full fresh boots have diverged.
With guarded native PUSHF and proof retention across POPF and port I/O, a
50-million checkpoint pair and a full fresh-boot pair now match on the
validation host. The full comparison includes snapshot clocks, RAM hashes,
registers, child clocks and reports, and parent isolation. Another 128 snapshot
replays match the stepping reference.
Subsequent fresh-boot comparisons have also diverged on both the guarded
native-PUSHF baseline and the extended counted-loop backend. Individual roots
and their fork comparisons pass, but fresh-root determinism remains unresolved;
a passing pair does not establish repeatability across runs.
After intercepting native IRET, 128 accelerated replays of the one-million
instruction span from 30 million to 31 million match the stepped reference.
A fresh 50-million checkpoint pair also matches at 3.11 and 3.12 seconds, and
a full fresh-boot pair matches at 36.09 seconds per root, compared with
3.53 seconds and 38.05 seconds before this change. These checks cover the
observed counting discrepancy, not every previously failing execution path.
Retaining scans through cached-code write guards further reduces a matching
50-million checkpoint pair to 2.83 and 2.82 seconds on the same host.
The full fresh-boot pair also matches at 36.20 and 36.08 seconds; this change
has not shown a measurable full-boot speedup.
Retaining proofs across CPUID and RDTSC/RDTSCP exits reduces the matching
50-million checkpoint pair to 2.80 and 2.79 seconds, and the full fresh-boot
pair to 33.40 and 33.37 seconds. Four replays of the one-million-instruction
span from 100 million to 101 million also match the stepped reference.
Prioritizing bounded native REP chunks reduces two 50-million runs to 2.48 and
2.41 seconds and approximately 528,000 exits. Those cold roots diverged by four
instructions, so these timings are not a successful pair comparison. Eight
accelerated replays of the span from 8 million to 9 million instructions match
the stepped reference at the first checkpoint where an earlier cold-root pair
diverged. Four accelerated replays from 100 million to 110 million instructions
also match the stepped reference.
Allowing exact outgoing-branch breakpoints inside the former 4,096-instruction
counter deadline margin reduced the complete boot from 31.55 to 28.07 seconds
on this host, and from 8.61 million to 6.34 million exits. Reducing the margin
to 512 instructions then reduced it to 22.16 seconds and 2.73 million exits.
Thirty-two accelerated replays from 8 to 9 million instructions and sixteen
from 100 to 110 million match the stepping reference at the smaller margin.
These samples do not establish repeatability: a later run with 10,000-instruction
intermediate checkpoints from 8 to 8.1 million diverged within 256 replays.
It also diverged with the former 4,096-instruction margin and with whole-page
execution disabled. A separate trial that kept only whole-page counter batches
also diverged. Disabling every counter-backed batch, while retaining
exact-breakpoint batches, matched 256 replays; all-scalar execution did too.
The one-instruction replay mismatch was traced to a counted guarded batch,
before a `REP MOVSL` made the error visible. In a failing CPUID loop iteration,
the programmable retired-instruction counter advanced 16 ticks but guest
IRPERF advanced only 15, with no overflow status. Both normally advance 16;
the backend subtracts the common VMRUN tick to report 15 guest instructions.
The PMU path now corrects a single-tick disagreement only before overflow.
It matches 2,048 per-exit replays from 8 to 8.02 million instructions,
1,024 checkpoint replays from 8 to 8.1 million, and 64 checkpoint replays
from 100 to 101 million. A complete Linux boot and its two forked children
also pass at 21.90 seconds and 2.73 million exits on the validation host.
These runs cover the reproduced discrepancy, not every possible PMU exit.
Native REP continues to use RCX-delta accounting: guest IRPERF reports one
retirement for a native three-iteration REP batch. A direct REP completion
test compares a stepped child against a native child and passes.
Checking the physical page of RIP when an NPT fetch guard fires lets a new
code page replan without a scalar replay, while a split instruction still
steps. The full Linux integration run passes at 20.71 seconds and 1.83 million
exits; 1,024 sensitive fork replays from 8 to 8.1 million instructions also
match the stepped reference with this change.
Recognizing memory-free `TEST` immediates and retaining table proofs when a
direct `CALL` pushes to a proved-disjoint stack slot reduces two full Linux
integration runs to 19.72 and 19.94 seconds, with about 1.82 million exits.
Another 1,024 fork replays from 8 to 8.1 million instructions match. Profiling
shows that frequent CR3 writes change the translation root, so their table
proofs still need rebuilding.
Before the reachable-region path, the boot had about 1.7 million scalar MTF exits. Roughly 130,000
sampled scalar returns land on one `memcpy`/`memmove` page: it has four real
`REP MOVS`/`REP STOS` hazards and two executable virtual aliases, requiring
more than the four available address breakpoints. A prototype that retired
about 129,000 such returns in software passed the boot/fork comparison but
left wall time near 19.6 seconds because per-entry planning still dominated;
it was not retained. The reachable-region proof now lets the hot page execute
without breakpoints for hazards outside its decoded control flow. A 32-entry
alias-proof cache avoids repeating expensive page-table walks across code
pages. A full Linux boot and fork replay pass at 19.44 seconds and about
0.93 million exits on the validation host, versus 19.72–19.94 seconds and
about 1.82 million exits before these changes. The lower exit count has not
yet produced a large wall-time improvement; alias-cache-only boot time was
19.18 seconds in one A/B run.
Expanding the guarded executable working set from 16 to 64 pages reduces a
full Linux boot/fork run to 17.91 seconds and about 0.86 million exits on the
validation host. The planner's largest measured stack frame stays below the
8KB kernel limit, and 1,024 checkpoint replays from 8 to 8.1 million match.
Caching guarded page plans reduces two full boot/fork runs to 16.02 and
16.05 seconds with about 0.86 million exits. The SVM hardware suite and
another 1,024 checkpoint replays pass. Later boot phases still invalidate
these plans frequently, leaving planning as the main measured cost.
VM entry setup remains the largest measured cost; the near-native register-loop
benchmark does not represent general Linux boot overhead.
On an EPYC 4244P `m4-metal-small` AMD box running Linux 7.0.0-38, the experimental
global gate initially repeated an unretired intercepted instruction during
early Linux boot. Forcing a scalar step at that boundary lets the same guest
boot and fork successfully; two fresh roots matched in one repeat test.
The fixed gate took 18.08 and 17.76 seconds per root with about 1.34 million
exits, while the pre-gate branch took 17.48 and 17.49 seconds with about
0.87 million exits on the same box and guest artifacts. The gate caused about
584,000 nested-page violations versus 8,500 before it. It is currently a
Linux performance regression on that host, despite near-native results for the
narrow register-loop benchmark.
The `svm_workload` example runs the exact same memory-writing integer routine
natively and in a long-mode guest, checking the checksum and every output word.
On the EPYC 4244P box, 512 passes over 32 KiB retired 18.88 million guest
instructions in 10.35 seconds, versus 1.71 ms natively: about 6,064 times slower. About
4.196 million of its 4.203 million VM exits were single steps. That is 1.82
million instructions per second in the guest versus 11.1 billion natively;
these are instruction rates, not CPU clock frequencies. The pre-gate AMD branch
took 9.59 seconds for the same guest work. The mode counter recorded no
global-gate entries for this workload. It is still well outside the 5%
native-overhead goal on that host and exposes an unaccelerated path for
memory-reading and writing loops. A 1,024-pass run gave 20.34 seconds guest versus
3.35 ms native, a similar 6,076-fold slowdown.
Two fresh EPYC 4245P `m4-metal-small` boxes with the same branch and HWE kernel
produced a very different result for the 1,024-pass workload: 37.75 million
instructions in 3.118 ms guest versus 3.074 ms native on one box, and
3.126 ms guest versus 3.085 ms native on the other. That is about 1% overhead
with only 11 to 21 VM exits per run. The second box reported SVM PMC
virtualization, ROGPT, and virtual NMI in CPUID. The earlier box's CPUID feature
bits were not saved, so the reason for the difference is not yet established.
On an EPYC 4245P boxctl box running HWE `7.0.0-38`, a sustained 1,048,576-pass
run retired 38.659 billion guest instructions in a three-sample median of
3.1625 seconds versus 3.1579 seconds natively (0.15% slower), with 3,904 to
4,310 VM exits per sample. That is 12.22 billion guest instructions per second,
an instruction throughput rather than a 12.22 GHz CPU clock. The Linux
`amd-pstate-epp` frequency reading on the pinned CPU was about 5.44–5.45 GHz
during the paired run, implying about 2.24 retired guest instructions per CPU
cycle. In the same box's short branch-heavy comparisons, guest time was about
50% higher than native. These measurements show near-native performance for
this memory workload, not for the full Linux boot/fork path.
On a later EPYC 4245P box, a clean build of the current branch passed the SVM
smoke, transition, and benchmark suites. A nine-sample 1,024-pass memory run
took 4.030 ms guest versus 3.974 ms native (1.01x, about 12 exits). To isolate a likely
feature dependency, a box-local diagnostic build disabled ROGPT in both the
VMCB and planner. With that feature disabled, the same workload took 12.346 ms
guest versus 3.087 ms native (4.00x, about 5,140 exits, nine samples).
Its translation-write and replay correctness checks passed, but its native
PUSHF acceleration assertion still failed. This is a feature ablation on a
4245P, not a measurement on a 4244P; it does not establish why the earlier
4244P run was 6,000x slower or meet the 5% goal on hosts without ROGPT.
This result is specific to the memory workload. With the earlier Linux 6.18
reference guest kernel and `svmGuestInitrd`, one EPYC 4245P box booted to the
snapshot and replayed two forks in 18.43 seconds; the fork outputs matched.
That verifies the integration path, but does not establish 5% overhead for
Linux. A different, newer Linux 6.18 guest kernel reached its snapshot but
spent over two minutes in a `delay_tsc` loop during the first fork. Those
kernel artifacts are different workloads and must not be compared as an A/B
performance result.
On a subsequent EPYC 4245P box, the reference Linux guest booted and replayed
two forks in 18.49 seconds with ROGPT enabled (1.16 million exits). On that
same box and guest artifacts, disabling ROGPT took 213.45 seconds and 122.55
million exits with the previous fallback. Allowing verified read-modify-write
stores and decoding MOVSXD reduced the no-ROGPT run to 205.72 seconds and
117.35 million exits; both no-ROGPT runs reached the same snapshot TSC and
memory hash. About 117 million exits were still single steps. An early-boot
opcode sample found direct CALL and RET at nearly half of scalar entries; the
Linux `__x86_return_thunk` RET was the most frequent sampled site. The
no-ROGPT functional suite passes with its PUSHF performance assertion replaced
by a diagnostic: native PUSHF still exits once per iteration. Near-native AMD
execution without ROGPT remains unfinished.
On a fresh EPYC 4245P box, caching instruction-fetch translations only while
the global gate protects the current guest page tables reduced two reference
Linux runs from 18.99/19.04 seconds to 17.83/17.84 seconds on the same box;
the final build with explicit guard-revocation checks took 17.64 seconds.
The snapshot TSC and memory hash matched; VM-entry setup cycles fell by about
9%. This improves the ROGPT path but does not address the unaccelerated
no-ROGPT path or establish 5% overhead for Linux against native execution.
Keeping an untrusted code page selected across scalar steps, while restoring
its NPT execute permission after every step, reduced nested-page faults on a
later EPYC 4245P Linux boot from 581,817 to 392,952–395,252. Boot and fork
replay still matched the reference snapshot and memory hash. The two measured
Linux wall times were 17.77 and 16.74 seconds versus 17.94 seconds before this
change on the same box; that limited timing sample does not establish a stable
wall-time speedup. A fetch-fault profile showed that most remaining faults are
repeated visits to pages with instruction hazards, and sampled scalar entries
cluster in early Linux text patching, including `IRETQ`.
On the same 4245P box, disabling only Bedrock's instruction-counter capability
for a diagnostic run made the 37.75-million-instruction memory workload 4.43x
slower than native; the smoke and transition suites still passed. This ablation
matches the separate 4244P finding that PMC virtualization is the decisive
feature for the current general fast path.
An EPYC 4244P box later identified the other major feature dependency:
CPUID Fn8000_000A_EDX was `0x1ebfbcff`, with ROGPT and virtual NMI present
but PMC virtualization absent. With instruction-counter batches disabled, a
589,893-instruction memory workload took 0.367 seconds and about 131,400 exits.
Counted store loops need no hardware PMC: enabling their existing range and
register proof independently reduced that same workload to 0.330 ms and 104
exits. A 37.75-million-instruction run took 21.5 ms guest versus 3.97 ms
native (5.4x, about 6,180 exits). This is a large improvement on the 4244P
but is still far from the 5% target. A guest-only host perf counter measured
the expected instruction count plus one tick per VM entry, but its overflow
interrupt did not stop a long SVM run: an exact-deadline test rejected a
159,998-instruction batch against a 99,995-instruction budget. Host perf
counting therefore cannot replace virtual PMC overflow without another
reliable way to bound execution. General Linux execution on this 4244P still
single-steps heavily.
On the EPYC 4244P box, the default `svm_bench` suite also fails its
decoded-branch acceleration assertion: one case needs 40,131 exits with the global gate and
20,029 without it, where the test requires fewer than 100. Its functional
branch result matched the stepped reference, but this host's acceleration
failure remains unresolved. The same suite passes on the EPYC 4245P box.
This backend requires SVM and nested paging.

For one-hour remote AMD bare-metal boxes, use the
[boxctl setup and test guide](contrib/BOXCTL_AMD.md).

The following hardware examples run on the test host after its module is
loaded. They exercise instruction deadlines, fork isolation, controlled
RDRAND/RDSEED, prefixed system-call transitions, interrupt flags, page-fault
recovery, and IRET stack restoration:

```sh
cargo build --release -p bedrock-vm --examples
sudo timeout 10 target/release/examples/svm_smoke
sudo timeout 10 target/release/examples/svm_transitions
sudo timeout 15 target/release/examples/svm_bench
sudo timeout 15 target/release/examples/svm_bench page-loops
sudo timeout 15 target/release/examples/svm_bench guarded-loops
sudo timeout 15 target/release/examples/svm_bench loop-deadlines
sudo timeout 15 target/release/examples/svm_bench control-flow
sudo timeout 10 target/release/examples/svm_bench rep-proof
sudo timeout 15 taskset -c 1 target/release/examples/svm_bench native
sudo timeout 15 taskset -c 1 target/release/examples/svm_bench native-branches
cargo build --release -p bedrock-vm --example svm_workload
sudo timeout 90 taskset -c 1 target/release/examples/svm_workload 512 3
```

The `native` comparison runs the same register-only loop natively and in a
long-mode guest nine times, with matching code alignment, and reports median
wall and thread CPU time. `native-branches` uses a loop with varying instruction
counts on its two paths. These comparisons measure the
verified loop path; it does not represent Linux or general guest workloads.
`page-loops` exercises calls, returns, and stores across data pages, an exact
mid-loop deadline, and two forks with matching final state and parent isolation.
`guarded-loops` exercises a counted store loop on a page excluded from page-wide
execution, including the same deadline and fork checks. The default suite also
checks such a loop storing its counter as payload, using page-table guards rather
than counter clamping.
`loop-deadlines` checks every instruction boundary with DEC before and after the
store for ECX, EDX, RCX, and RDX, including large counter values, 32-bit zero
extension, and arithmetic-flag restoration.
`svm_bench VMLINUX INITRD [INSTRUCTIONS]` stops Linux at an exact instruction
checkpoint (one million by default), reports its registers and RAM hash, and
limits each check to ten seconds.

The Linux integration example reports progress every ten seconds, including
the guest instruction count, RIP, and active page-table count. Each phase has
a ten-minute timeout. It uses a userspace snapshot, then compares two
forks' clock syscall, `getrandom`, RDRAND/RDSEED, final registers, and virtual
TSC. It also checks that the children leave the parent's memory intact:

```sh
nix build .#svmGuestKernel .#svmGuestInitrd --no-link
cargo build --release -p bedrock-vm --example svm_linux
sudo target/release/examples/svm_linux \
  "$(nix path-info .#svmGuestKernel)/vmlinux" \
  "$(nix path-info .#svmGuestInitrd)"
```

Append `repeat` to compare two fresh boots as well, including the snapshot RAM
hash, registers, virtual TSC, and both children's results.
Set `BEDROCK_SVM_EXIT_STATS=1` to print the root VM's exit and cycle breakdown
at the userspace snapshot.
For a shorter reproducibility check, run `svm_bench VMLINUX INITRD INSTRUCTIONS
repeat`; it compares the RAM hash and registers at an exact instruction
deadline. `BEDROCK_CHECKPOINT_TIMEOUT_SECONDS` overrides its default 10-second
limit when checking a later checkpoint. `BEDROCK_CHECKPOINT_RNG_SEED` sets the
checkpoint's deterministic RNG seed; use `42` with the integration example's
initrd to match its inputs. `BEDROCK_CHECKPOINT_INTERVAL` records intermediate
checkpoints in each boot; with `repeat`, the first mismatch is reported. These
extra stops change where execution is replanned, so also check a single deadline
when investigating a divergence. Each integration root reports its elapsed time,
snapshot instruction count, and RAM hash after the fork checks.
For repeated tests of a short span, `BEDROCK_CHECKPOINT_FORK_START` with `repeat`
boots once to that snapshot, then compares child registers at the requested
deadline and checks parent RAM isolation. Fork RAM hashes are unavailable through
the SDK. `BEDROCK_CHECKPOINT_FORK_REPLAYS` controls the number of children (eight
by default). `BEDROCK_CHECKPOINT_FORK_INTERVAL` sets intermediate checkpoints
only in the children, allowing detailed comparison without repeated parent
checkpoints. It falls back to `BEDROCK_CHECKPOINT_INTERVAL` when unset.
This checks replay from a shared snapshot, rather than fresh boots.
Set `BEDROCK_CHECKPOINT_REFERENCE_STEP` to run the first child with instruction
stepping, then compare the accelerated replays against that reference.
A fast regression check for the unresolved counter discrepancy uses
`BEDROCK_CHECKPOINT_FORK_START=8000000`,
`BEDROCK_CHECKPOINT_FORK_INTERVAL=10000`,
`BEDROCK_CHECKPOINT_FORK_REPLAYS=256`,
`BEDROCK_CHECKPOINT_REFERENCE_STEP=1`, and a target of `8100000` with `repeat`.

This reference kernel disables ftrace and ORC metadata to reduce boot-time
stepping. The integration test uses a 100 MHz virtual TSC: a very low frequency
can make timer handlers consume more virtual time than the timer period and
starve boot. Boot can still take several minutes with this backend.

The reference backend supports real mode and four-level long-mode paging.
Legacy protected-mode interrupt delivery and legacy paging are unsupported;
legacy paging is rejected before instructions can bypass RNG decoding. Guest
debugging and fault delivery during software emulation need further coverage.

The default guest kernel's ftrace and BTF initialization is expensive under
instruction stepping. A native run hit its 30-minute wall-clock timeout during
BTF initialization, before reaching userspace; use the reference kernel for
the validated AMD boot and fork tests above.

For workloads that size thread pools from CPU affinity, add `bedrock_ncpus=8`
to the guest kernel command line. On either backend, the guest reports eight
CPUs through `sched_getaffinity` and accepts affinity changes as no-ops.
Threads still share one vCPU; sysfs and `/proc` report the actual topology.
Omitting the parameter retains the normal affinity behavior.

Use unit tests and these short native examples while changing the backend;
reserve Linux boot and fork replay for integration checks. The affinity patch
can be checked independently through KVM, with a 20-second limit per boot:

```sh
# With Bedrock unloaded and KVM available:
python3 contrib/check-guest-affinity.py /path/to/patched-guest/bzImage
```

This checks the default, eight-CPU, and zero-CPU-override settings. It exercises
Linux affinity behavior; the native examples validate the Bedrock backend.

`contrib/run-svm-test.py` runs these executables in a diskless nested test
host. Its required `--expect` marker prevents a successful CLI return after
an unhandled guest exit from being mistaken for a successful boot.

On Ubuntu's Linux 7.0 HWE kernel, install its matching Rust libraries and
compiler. The headers' Rust library symlink may point to a nonexistent HWE
directory; pass the installed library directory explicitly. `SVM_ONLY=1`
omits Intel's CR4 helper, which this kernel reserves for KVM:

```sh
sudo apt-get install linux-lib-rust-7.0.0-38-generic rustc-1.91
make -C /lib/modules/7.0.0-38-generic/build \
  M="$PWD/crates/bedrock" RUSTC=/usr/bin/rustc-1.91 \
  CC=x86_64-linux-gnu-gcc-13 \
  KRUSTFLAGS='-L /usr/src/linux-lib-rust-7.0.0-38-generic/rust' \
  SVM_ONLY=1 modules
```

KVM and Bedrock cannot own SVM simultaneously. Stop KVM guests before
unloading `kvm_amd` and loading Bedrock. The guest hypercall library consults
Bedrock's CPUID leaf `0x40000001` (EAX bit zero) to select VMMCALL. Guest CPU
identification follows Bedrock's Intel profile on either hardware backend,
so Linux can calibrate its clock from the emulated CPUID frequency leaves.

## CI

CI uses [RunsOn](https://runs-on.com/) with `m7i` AWS instances.

---

*This project was created with heavy assistance from LLMs. Might freeze/hang or
otherwise corrupt host machine, run at your own risk.*
