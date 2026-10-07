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
up to sixteen recently visited code pages. Hardware execution breakpoints stop
before possible RDRAND/RDSEED/RDPID, repeated-string encodings, and SYSRET;
their prefix entry points and executable virtual aliases must fit in four
breakpoint slots. Alias enumeration uses a preallocated workspace of 512
virtual table paths and falls back if that capacity is exceeded. SYSRET uses the scalar path because it can restore guest
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
Alias proofs are reused while the table-frame proof remains valid, with
hazardous physical pages and hazard offsets as the cache key, independent of
selection order and ordinary code pages. Cached edge summaries check cross-page
boundaries on each preparation. Optional pages that cannot fit the four
breakpoints are omitted before alias enumeration.
NPT permission guards reuse a heap workspace and cached executable-entry masks;
write guards restore leaf permissions directly without another table walk.
Optional code-page translations are cached while the guarded guest table proof
remains valid; revoking that proof or changing CR3 rebuilds the cache.
Code-page hazard scans can survive proof invalidation through an exact-byte
cache. All 4KB are compared before reusing a scan; changed bytes are rescanned.
The sixteen-page cache adds approximately 64KB to each VM's heap workspace.
Page execution requires more than
4,096 instructions before the next deadline. This margin has been exercised on
the validation host; it is not a calibrated bound for every AMD processor.
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
across all execution paths.

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
This backend requires SVM and nested paging.

The hardware examples exercise instruction deadlines, fork isolation,
controlled RDRAND/RDSEED, prefixed system-call transitions, interrupt flags,
page-fault recovery, and IRET stack restoration:

```sh
cargo build --release -p bedrock-vm --examples
sudo timeout 10 target/release/examples/svm_smoke
sudo timeout 10 target/release/examples/svm_transitions
sudo timeout 15 target/release/examples/svm_bench
sudo timeout 15 target/release/examples/svm_bench page-loops
sudo timeout 15 target/release/examples/svm_bench guarded-loops
sudo timeout 15 target/release/examples/svm_bench loop-deadlines
sudo timeout 15 target/release/examples/svm_bench control-flow
sudo timeout 15 taskset -c 1 target/release/examples/svm_bench native
sudo timeout 15 taskset -c 1 target/release/examples/svm_bench native-branches
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
