#!/usr/bin/env bash
# Runs after rebooting into the new kernel. Any failure fails the Packer build,
# so an AMI is only produced if bedrock actually works on this image.
set -euo pipefail
# shellcheck source=/dev/null
. /etc/profile.d/bedrock-toolchain.sh

krel=$(cat /etc/bedrock-kernel-release)
echo "running: $(uname -r), expected: ${krel}"
[ "$(uname -r)" = "$krel" ]

if lsmod | grep -Eq '^kvm(_intel)? '; then
  echo "kvm is loaded; the blacklist did not take effect" >&2
  exit 1
fi

grep -qw vmx /proc/cpuinfo || { echo "no VMX: not bare metal?" >&2; exit 1; }

# EPT-friendly PEBS: IA32_PERF_CAPABILITIES (0x345) PEBS_BASELINE (bit 14)
# and PEBS_FMT (bits 11:8) >= 4. KVM guests never have it.
modprobe msr
caps=$((16#$(rdmsr -p 0 0x345)))
printf 'IA32_PERF_CAPABILITIES=0x%x\n' "$caps"
if (( ((caps >> 14) & 1) != 1 || ((caps >> 8) & 0xf) < 4 )); then
  echo "EPT-friendly PEBS unavailable" >&2
  exit 1
fi

# Build and load bedrock.ko from the uploaded source.
src=/tmp/bedrock-src
rm -rf "$src"
mkdir -p "$src"
tar -xzf /tmp/bedrock-src.tar.gz -C "$src"
make -C "$src/crates/bedrock" KDIR="/lib/modules/${krel}/build" LLVM=1
insmod "$src/crates/bedrock/bedrock.ko"
udevadm settle
test -c /dev/bedrock
dmesg | grep -i bedrock | tail -5

if [ "${RUN_INTEGRATION_TESTS:-true}" = true ]; then
  # Same suite upstream CI runs on bare metal: bedrock-lab against
  # /dev/bedrock, with the guest kernel/initrd built by Nix.
  if ! command -v nix >/dev/null && [ ! -x /nix/var/nix/profiles/default/bin/nix ]; then
    # Pinned installer; it verifies the Nix tarball's hash itself.
    curl -fsSL https://releases.nixos.org/nix/nix-2.35.2/install -o /tmp/nix-install
    echo "9adda97297d9e8ab360df95c729eabff4f4f93d6db091953c3a68f29e3fb130c  /tmp/nix-install" | sha256sum -c -
    sh /tmp/nix-install --daemon --yes
  fi
  export PATH=/nix/var/nix/profiles/default/bin:$PATH
  mkdir -p /etc/nix
  grep -q '^experimental-features' /etc/nix/nix.conf 2>/dev/null ||
    echo 'experimental-features = nix-command flakes' >> /etc/nix/nix.conf
  systemctl restart nix-daemon

  systemctl enable --now docker
  cd "$src"
  DOCKER=docker ./workloads/integration-tests/build.sh
  nix run .#integration-tests -- --test-threads=1
fi

rmmod bedrock
echo "VERIFIED: ${krel} with EPT-friendly PEBS; bedrock.ko builds and loads"
