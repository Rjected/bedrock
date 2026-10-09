#!/usr/bin/env bash
# Build Linux 6.18 with CONFIG_RUST=y from Debian's own cloud kernel config
# (keeps the EC2 NVMe/ENA/Nitro drivers), install it as a Debian package so the
# distro hooks build its initramfs and GRUB entry, and keep the built tree as
# the module build directory for out-of-tree Rust modules (bedrock.ko).
#
# The stock Debian kernel stays installed as a fallback GRUB entry.
set -euo pipefail
# shellcheck source=/dev/null
. /etc/profile.d/bedrock-toolchain.sh

KVER=6.18
# kernel.org sha256sums.asc, signed by the kernel.org checksum autosigner
# (B8868C80BA62A1FFFAF5FDA9632D3A06589DA6B1); same tree as the flake's v6.18.
KSHA256=9106a4605da9e31ff17659d958782b815f9591ab308d03b0ee21aad6c7dced4b
LOCALVERSION=-bedrock
SRC=/usr/src/linux-${KVER}${LOCALVERSION}

base_config=$(printf '%s\n' /boot/config-*-cloud-amd64 | sort -V | tail -1)
test -f "$base_config"
echo "base config: $base_config"

mkdir -p /usr/src
cd /usr/src
curl -fsSL -o "linux-${KVER}.tar.xz" "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KVER}.tar.xz"
echo "${KSHA256}  linux-${KVER}.tar.xz" | sha256sum -c -
rm -rf "$SRC"
mkdir "$SRC"
tar -xf "linux-${KVER}.tar.xz" -C "$SRC" --strip-components=1
rm "linux-${KVER}.tar.xz"
cd "$SRC"

mk() { make LLVM=1 "$@"; }

cp "$base_config" .config
mk olddefconfig
# Rust and its requirements; Debian's module signing keys are not available.
scripts/config \
  --enable RUST \
  --disable MODVERSIONS \
  --disable DEBUG_INFO_BTF \
  --disable DEBUG_INFO_DWARF5 \
  --disable DEBUG_INFO_DWARF4 \
  --disable DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT \
  --enable DEBUG_INFO_NONE \
  --set-str SYSTEM_TRUSTED_KEYS "" \
  --set-str SYSTEM_REVOCATION_KEYS "" \
  --set-str LOCALVERSION "${LOCALVERSION}" \
  --disable LOCALVERSION_AUTO \
  --enable MODULE_FORCE_LOAD \
  --module KVM \
  --module KVM_INTEL
mk olddefconfig

# Fail fast (before the long build) if a Rust dependency dropped RUST.
mk rustavailable
if ! grep -q '^CONFIG_RUST=y$' .config; then
  echo "CONFIG_RUST was disabled by olddefconfig; unmet dependencies:" >&2
  grep -E '^config RUST$' -A25 init/Kconfig >&2
  exit 1
fi
for opt in BLK_DEV_NVME ENA_ETHERNET EXT4_FS; do
  grep -Eq "^CONFIG_${opt}=(y|m)$" .config || { echo "missing CONFIG_${opt}" >&2; exit 1; }
done
if grep -q '^CONFIG_MODULE_SIG_FORCE=y$' .config; then
  echo "MODULE_SIG_FORCE would refuse unsigned bedrock.ko" >&2
  exit 1
fi

# kernelrelease reads CONFIG_LOCALVERSION from include/config/auto.conf,
# which only syncconfig generates.
mk syncconfig
krel=$(mk -s kernelrelease)
echo "kernel release: $krel"
if [ "$krel" != "${KVER}.0${LOCALVERSION}" ]; then
  echo "unexpected kernel release ${krel}" >&2
  exit 1
fi

# Local dry run of everything up to the build (see README.md).
if [ "${BEDROCK_CONFIG_ONLY:-0}" = 1 ]; then
  echo "CONFIG_ONLY: config for ${krel} OK"
  exit 0
fi

# bindeb-pkg writes the .debs to the parent directory.
mk -j"$(nproc)" bindeb-pkg KDEB_PKGVERSION="${KVER}.0-1"
image_deb="../linux-image-${krel}_${KVER}.0-1_amd64.deb"
test -f "$image_deb" || { ls -la .. >&2; echo "missing $image_deb" >&2; exit 1; }
dpkg -i "$image_deb"
rm -f ../linux-*.deb ../linux-*.buildinfo ../linux-*.changes

# Out-of-tree modules (incl. Rust) build against the full tree.
ln -sfn "$SRC" "/lib/modules/${krel}/build"
chmod -R a+rX "$SRC"
echo "$krel" > /etc/bedrock-kernel-release

# Bedrock owns VMX; KVM must not be loaded alongside it.
cat > /etc/modprobe.d/bedrock-no-kvm.conf <<'EOF'
# Bedrock is the hypervisor on this host; kvm_intel would claim VMX.
blacklist kvm_intel
blacklist kvm
EOF
cat > /etc/udev/rules.d/99-bedrock.rules <<'EOF'
KERNEL=="bedrock", MODE="0666"
EOF
update-initramfs -u -k "$krel"

# The new kernel must be GRUB's default entry for the reboot. Debian's first
# menuentry is the generic "Debian GNU/Linux"; its kernel is the first
# `linux /boot/vmlinuz-...` line.
update-grub
default_kernel=$(grep -m1 -oE '^[[:space:]]*linux[[:space:]]+/boot/vmlinuz-[^[:space:]]+' /boot/grub/grub.cfg | awk '{print $2}')
echo "GRUB default kernel: $default_kernel"
if [ "$default_kernel" != "/boot/vmlinuz-${krel}" ]; then
  echo "GRUB default is not ${krel}" >&2
  exit 1
fi
