# Bedrock bare-metal host AMI

Debian 13 with Linux 6.18 built with `CONFIG_RUST=y`, so `bedrock.ko` loads
natively on bare-metal Intel hosts with EPT-friendly PEBS. Running bedrock
nested under KVM does not work for bedrock-lab branching: KVM hides
`PEBS_BASELINE` from guests (`vmx_get_perf_capabilities()`), and without
precise exits a guest spinning in user code never receives timer interrupts.

## What the build does

| Step | Script | Fails the build if |
|---|---|---|
| Toolchain | `scripts/10-toolchain.sh` | rustup-init checksum mismatch, package missing |
| Kernel | `scripts/20-kernel.sh` | tarball checksum mismatch, `CONFIG_RUST` dropped, NVMe/ENA/ext4 missing, `MODULE_SIG_FORCE` set, 6.18 not GRUB's default |
| Reboot | (inline) | the instance does not come back on 6.18 within 45 min |
| Verify | `scripts/30-verify.sh` | wrong kernel running, KVM loaded, no PEBS (`IA32_PERF_CAPABILITIES` bit 14 / format >= 4), `bedrock.ko` fails to build or load, integration tests fail |
| Cleanup | `scripts/40-cleanup.sh` | |

Pinned inputs:

| Input | Pin |
|---|---|
| Base image | latest `debian-13-amd64-*` from the Debian cloud team (`136693071363`) |
| Kernel | `linux-6.18.tar.xz`, sha256 `9106a4605da9e31ff17659d958782b815f9591ab308d03b0ee21aad6c7dced4b` (kernel.org autosigner `B8868C80BA62A1FFFAF5FDA9632D3A06589DA6B1`; same tree as the flake's `v6.18`) |
| Kernel config | the base image's own `/boot/config-*-cloud-amd64` + Rust requirements |
| Rust | 1.94.0 via rustup-init 1.28.2 (sha256-checked), bindgen-cli 0.72.1 — same as the flake |
| LLVM | Debian `llvm-19` (kernel minimum: 15) |
| Nix (verification only) | 2.35.2 installer (sha256-checked) |

The stock Debian kernel stays installed as a fallback GRUB entry.

## Build

Requirements: Packer >= 1.10, AWS credentials allowed to run EC2 instances
and create AMIs/snapshots, and on-demand capacity for `m7i.metal-24xl` (~$5/h;
a build takes roughly 1–1.5 h with integration tests).

```sh
git archive --format=tar.gz -o /tmp/bedrock-src.tar.gz HEAD   # source used for verification
cd contrib/ami
packer init bedrock-ami.pkr.hcl
packer build -var region=us-east-1 -var bedrock_src=/tmp/bedrock-src.tar.gz bedrock-ami.pkr.hcl
```

Optional variables: `instance_type` (any bare-metal Ice Lake-SP or newer),
`subnet_id` (default VPC otherwise), `run_integration_tests=false` to skip the
Nix-based suite. The AMI id is written to `manifest.json`.

Enable the EC2 serial console for the account/region before building: if the
6.18 boot ever fails, the serial log is the only way to see why.

## Using the image

```sh
uname -r                          # 6.18.0-bedrock
make -C crates/bedrock LLVM=1     # KDIR defaults to /lib/modules/$(uname -r)/build
sudo insmod crates/bedrock/bedrock.ko
ls -l /dev/bedrock
```

The toolchain is on `PATH` for login shells (`/etc/profile.d/bedrock-toolchain.sh`).
`kvm_intel` and `kvm` are blacklisted (`/etc/modprobe.d/bedrock-no-kvm.conf`):
bedrock is the hypervisor here. Remove that file to run KVM guests instead.

## Dry run without AWS

The toolchain and kernel-config steps can be checked in a container; this
catches package, toolchain, and Kconfig problems before paying for a metal
instance:

```sh
docker run --rm --platform linux/amd64 -v "$PWD/scripts:/scripts:ro" debian:trixie bash -c '
  apt-get update && apt-get install -y linux-image-cloud-amd64
  bash /scripts/10-toolchain.sh && BEDROCK_CONFIG_ONLY=1 bash /scripts/20-kernel.sh'
```
