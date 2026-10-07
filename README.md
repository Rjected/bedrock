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
shared with the Intel backend. AMD execution currently steps every instruction;
Intel PEBS registration and acceleration are not implemented on AMD.

Native validation on an AMD EPYC 4585PX with Ubuntu Linux 7.0.0-38-generic
includes Linux 6.18 booting to userspace and matching executions of two Linux
forks. This backend requires SVM and nested paging.

The hardware examples exercise instruction deadlines, fork isolation,
controlled RDRAND/RDSEED, prefixed system-call transitions, interrupt flags,
page-fault recovery, and IRET stack restoration:

```sh
cargo build --release -p bedrock-vm --examples
sudo timeout 10 target/release/examples/svm_smoke
sudo timeout 10 target/release/examples/svm_transitions
```

The Linux integration example uses a userspace snapshot, then compares two
forks' clock syscall, `getrandom`, RDRAND/RDSEED, final registers, and virtual
TSC. It also checks that the children leave the parent's memory intact:

```sh
nix build .#svmGuestKernel .#svmGuestInitrd --no-link
cargo build --release -p bedrock-vm --example svm_linux
sudo target/release/examples/svm_linux \
  "$(nix path-info .#svmGuestKernel)/vmlinux" \
  "$(nix path-info .#svmGuestInitrd)"
```

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
