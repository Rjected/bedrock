# AMD SVM testing with boxctl

All Bedrock module builds, loads, and hardware tests in this workflow run on a
disposable AMD bare-metal box. The workstation uses `boxctl`, SSH, rsync, and
scp to manage it; it never loads a Bedrock module. A box created with the
command below expires after one hour unless deleted sooner.

## One-time local setup

Install the [Tempo boxctl CLI](https://github.com/tempoxyz/boxctl) as described
in its `USERGUIDE.md`, join the Tempo tailnet, and confirm that you can run
`boxctl get boxes` and SSH to your boxes. The helper needs `ssh`, `scp`, and
`rsync` locally. Add the CLI's install directory (for example `~/.local/bin` or
`~/go/bin`) to `PATH` if necessary.

Run the following from the Bedrock worktree whose current files you want to
test. An uncommitted source edit is synced to the box just like a committed
one. Only one remote run should use a given box at a time, because the helper
reloads its Bedrock module.

## Create and prepare a box

```sh
BOX=$(boxctl create box --keep-alive 1h --plan m4-metal-small \
  --region FRA --disable-job-agent --wait -o name)
echo "$BOX"
contrib/run-boxctl-svm.sh prepare "$BOX"
```

`prepare` installs the Linux 7.0.0-38 HWE image, matching headers and Rust
libraries, `rustc-1.91`, and build tools **inside the box**. The current
Ubuntu 24.04 box image boots Linux 6.8 despite having the HWE image installed;
`prepare` switches it to 7.0 with `kexec` and waits for SSH to return. It also
installs the userspace Rust toolchain and checks for AMD `svm`, `npt`, and
`perfctr_core` CPU flags. A matching header package alone cannot make a
module load into a different running kernel.

## Fast edit–test loop

```sh
contrib/run-boxctl-svm.sh test "$BOX"
# Edit source locally, then run the same command again.
contrib/run-boxctl-svm.sh test "$BOX"
```

`test` syncs the current worktree, builds `bedrock.ko` against the box's
running HWE kernel, releases `kvm_amd` and `kvm` on the box, loads Bedrock
there, and runs a 37.75-million-instruction mixed memory workload, the smoke
and transition suites, the default SVM suite, and both native-loop comparisons.
It prints each build and test phase, and bounds SSH waits if a box drops off
the network. It skips package installation and rebooting on every
repeat. A fresh box can run both phases with
`contrib/run-boxctl-svm.sh all "$BOX"` (or just
`contrib/run-boxctl-svm.sh "$BOX"`).

Logs are copied to `target/boxctl-evidence/$BOX/`. If a build or test fails,
the script still attempts to copy its logs and leaves the box available for
inspection. The box can also be inspected with `boxctl ssh "$BOX"`. Its disk
and any unsaved logs disappear when it expires.

The current experimental global-gate branch failed a decoded-branch
performance assertion on an EPYC 4244P `m4-metal-small` box but passed on an
EPYC 4245P box; see the AMD status in the [main README](../README.md).
On a later 4244P box, CPUID showed ROGPT present but PMC virtualization
absent. Record `SVM_WORKLOAD_HOST` along with the model name: the two CPU
models select very different instruction-counting paths despite using the
same plan name.
If `test` returns nonzero, the box remains available for focused benchmarks
and debugging.

The native-loop example runs nine paired measurements and reports their
median. These short timings still vary between invocations; repeat a run
before treating a percentage as a performance result.

The default `test` run now measures 1,024 rounds of `svm_workload`. It executes
identical assembly natively and in a guest, and checks every output word. For
an additional stress run with a different round count, use:

```sh
ssh "ubuntu@$BOX" 'cd /home/ubuntu/bedrock && \
  /home/ubuntu/.cargo/bin/cargo build --release -p bedrock-vm --example svm_workload && \
  sudo timeout 90 taskset -c 1 target/release/examples/svm_workload 512 3 \
    > /tmp/bedrock-workload-extra.log 2>&1'
contrib/run-boxctl-svm.sh collect "$BOX"
```

Its `instructions` and `seconds` fields give a measured instruction rate;
CPU GHz and instructions per second are different units. The sample also
reports VM exits and the native-to-guest slowdown. `SVM_WORKLOAD_HOST` records
the SVM feature bits used to select accelerated paths. Keep the box's CPU
model and feature line with each result: an EPYC 4244P with ROGPT but no PMC
virtualization measured 5.41x slowdown after counted-store-loop acceleration,
while an EPYC 4245P with PMC virtualization measured about 1% overhead. This
difference is a hardware feature gate, not a CPU-frequency comparison.

## Linux boot and replay checks

The quick loop does not build a guest Linux kernel. For the integration example,
build or fetch the `svmGuestKernel` `vmlinux` and `svmGuestInitrd` artifacts
described in the [AMD section of the main README](../README.md). A Nix-equipped
builder can run `nix build .#svmGuestKernel .#svmGuestInitrd --no-link` and use
`nix path-info` to locate the outputs. Put copies of both files on the
workstation and set `VMLINUX` and `INITRD` to their paths before starting the
one-hour hardware lease. Copy the files to the box, run `svm_linux` there,
and keep its output in `/tmp` for `collect`:

```sh
scp "$VMLINUX" "ubuntu@$BOX:/home/ubuntu/bedrock-vmlinux"
scp "$INITRD" "ubuntu@$BOX:/home/ubuntu/bedrock-initrd"
ssh "ubuntu@$BOX" 'set -o pipefail; cd /home/ubuntu/bedrock && \
  /home/ubuntu/.cargo/bin/cargo build --release -p bedrock-vm --example svm_linux && \
  sudo env BEDROCK_SVM_EXIT_STATS=1 timeout 1200 target/release/examples/svm_linux \
    /home/ubuntu/bedrock-vmlinux /home/ubuntu/bedrock-initrd \
    2>&1 | tee /tmp/bedrock-linux.log'
contrib/run-boxctl-svm.sh collect "$BOX"
```

Append `repeat` to the `svm_linux` arguments to compare two fresh boots. The
exit statistics in the log help distinguish VM-entry cost from exit handling
and nested-page faults. Compare changes on the same box and guest artifacts.

Use `svm_bench` with the same two files for a bounded instruction
checkpoint; the main README lists its deadline and replay arguments. The
native-loop percentages from the quick loop measure only those loop programs,
not Linux boot overhead.

For isolated kernel-module diagnostic counters, reload the module **inside
the box** after the quick suite and before the Linux run:

```sh
ssh "ubuntu@$BOX" 'cd /home/ubuntu/bedrock && \
  sudo rmmod bedrock && sudo insmod crates/bedrock/bedrock.ko && sudo dmesg -C'
```

The quick suite includes deliberate single-step windows. Its module counters
persist into a later Linux run unless the module is reloaded, and old `dmesg`
lines persist unless the log is cleared. Neither operation runs on the
workstation.

## Collect and delete

```sh
contrib/run-boxctl-svm.sh collect "$BOX"
boxctl delete box "$BOX" --wait
```

`collect` can be run at any point while the box is reachable. Delete the box
after collecting the evidence you need; the one-hour lifetime also destroys
it automatically. On the workstation, do not run the repository's local
`just load`, `modprobe`, `insmod`, or hardware test recipes.
