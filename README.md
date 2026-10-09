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
Page execution requires more than 512 instructions before the next deadline.
The largest observed PMC overflow lag was 135 retired instructions in a
200-million-TSC checkpoint and a complete boot on the validation host. The
margin is not a calibrated bound for every AMD processor; a late exit fails
closed rather than returning an incorrect instruction count.
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
This result is specific to the memory workload. With the earlier Linux 6.18
reference guest kernel and `svmGuestInitrd`, one EPYC 4245P box booted to the
snapshot and replayed two forks in 18.43 seconds; the fork outputs matched.
That verifies the integration path, but does not establish 5% overhead for
Linux. A different, newer Linux 6.18 guest kernel reached its snapshot but
spent over two minutes in a `delay_tsc` loop during the first fork. Those
kernel artifacts are different workloads and must not be compared as an A/B
performance result.
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
