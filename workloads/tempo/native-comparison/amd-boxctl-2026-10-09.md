# AMD boxctl transfer comparison (2026-10-09)

The historical Intel Bedrock comparison at commit
`b834e859037c1707089a50097a775f53e9559bc8` measured the complete
`bench send` command for 10,000 pathUSD transfers. Its two Bedrock runs took
24.097 and 24.077 seconds (mean **24.087 s**); its five-CPU native runs took
13.274 and 13.275 seconds (mean **13.275 s**). All runs included 10,000
transfers with no failures or reverts. See
`git show b834e859:workloads/tempo/native-comparison/16cpu/comparison.json`.

We repeated the same benchmark command on disposable AMD EPYC 4245P boxctl
hosts running Ubuntu HWE kernel `7.0.0-38`. Bedrock used one execution CPU,
reported five CPUs to the guest, and ran the Linux 6.18 Tempo guest with
`bedrock_loop_delay lpj=1000000`. The guest wrapper and `fair-run.sh` are
unchanged from the Intel comparison. `BEDROCK_FAIR_TIMING=1` records host
`Instant` at the two `HYPERCALL_READY` boundaries around fork/exec/wait for
`bench send`; the same wrapper records native `CLOCK_MONOTONIC` boundaries.
Boot and transaction generation are outside the measured interval. Both
workloads used txgen seed 99, uncapped sending, max 200 pending transactions,
max 16 concurrent requests, and 200 ms blocks.

| Transfers | Execution | Complete `bench send` wall time | Result |
| ---: | --- | ---: | --- |
| 10,000 | Intel Bedrock (historical) | 24.087 s mean | 10,000 included; zero failures/reverts |
| 10,000 | Native AMD, five CPUs | 13.109 s mean | 10,000 included; zero failures/reverts |
| 10,000 | AMD Bedrock, one CPU | **>28 min 40 s** | Timed out before end marker or report |
| 100 | Native AMD, five CPUs | 3.216 s mean | 100 included; zero failures/reverts |
| 100 | AMD Bedrock, one CPU | **>17 min 15 s** | Stopped before end marker/report; guest log shows 100 successful submissions and 100 transactions in one block |

The 10,000-transfer AMD Bedrock attempt exceeded **70 times** the historical
Intel Bedrock duration and **130 times** the native AMD duration without
finishing. The 100-transfer attempt exceeded **320 times** its native AMD
control. These are lower bounds on *completion time*, not throughput numbers:
the Bedrock runs did not produce final benchmark reports. The 100-transfer
guest reached the txpool-drain step after its block included all 100
transactions, but advanced only a small amount of guest virtual time over
several more host minutes. It was stopped to keep the feedback loop bounded.

The AMD runs used the current guest initrd and image archive. The Tempo
localnet image has the historical pinned digest; the rebuilt txgen image
digest differs, although the historical benchmark script is bind-mounted in
both AMD native and guest runs. The Intel Bedrock run was nested in a NixOS/KVM
VM on a different CPU. This is the same workload and timing boundary, but not
a byte-identical artifact or host comparison. The historical 5% overhead
figure for Intel's hardware execution path is a separate microbenchmark and
does not describe this transfer application, where historical Intel Bedrock
was about 1.82 times its native control.

All AMD module builds, loads, and hardware tests took place in boxctl boxes;
no kernel module operation was performed on the workstation. Full logs and
native reports are saved locally under `target/boxctl-evidence/` (ignored by
Git). The next useful experiment is a short guest run that stops at known
virtual-time checkpoints after `bench send` begins and dumps SVM exit counts;
the current full-run measurements do not identify the dominant exit path.
