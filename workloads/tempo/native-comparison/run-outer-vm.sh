#! /nix/store/v8sa6r6q037ihghxfbwzjj4p59v2x0pv-bash-5.3p9/bin/bash

export PATH=/nix/store/74sind1d6vf2bfwd7yklg8chsvzqxmmq-coreutils-9.10/bin${PATH:+:}$PATH

set -e

# Create an empty ext4 filesystem image. A filesystem image does not
# contain a partition table but just a filesystem.
createEmptyFilesystemImage() {
  local name=$1
  local size=$2
  local temp=$(mktemp)
  /nix/store/zmxs40sh9m6g01jdz9w9l031wph4pskb-qemu-host-cpu-only-10.2.2/bin/qemu-img create -f raw "$temp" "$size"
  /nix/store/1022kp8vwbjr5pdbrcsplhlsiaj2kffz-e2fsprogs-1.47.3-bin/bin/mkfs.ext4 -L nixos "$temp"
  /nix/store/zmxs40sh9m6g01jdz9w9l031wph4pskb-qemu-host-cpu-only-10.2.2/bin/qemu-img convert -f raw -O qcow2 "$temp" "$name"
  rm "$temp"
}

NIX_DISK_IMAGE=$(readlink -f "${NIX_DISK_IMAGE:-./bedrock-vm.qcow2}") || test -z "$NIX_DISK_IMAGE"

if test -n "$NIX_DISK_IMAGE" && ! test -e "$NIX_DISK_IMAGE"; then
    echo "Disk image does not exist, creating the virtualisation disk image..."

    createEmptyFilesystemImage "$NIX_DISK_IMAGE" "4096M"

    echo "Virtualisation disk image created."
fi

# Create a directory for storing temporary data of the running VM.
if [ -z "$TMPDIR" ] || [ -z "$USE_TMPDIR" ]; then
    TMPDIR=$(mktemp -d nix-vm.XXXXXXXXXX --tmpdir)
fi



# Create a directory for exchanging data with the VM.
mkdir -p "$TMPDIR/xchg"







cd "$TMPDIR"



# Start QEMU.
exec /nix/store/zmxs40sh9m6g01jdz9w9l031wph4pskb-qemu-host-cpu-only-10.2.2/bin/qemu-system-x86_64 -machine accel=kvm:tcg -cpu max \
    -name bedrock-vm \
    -m 32768 \
    -smp 5 \
    -device virtio-rng-pci \
    -net nic,netdev=user.0,model=virtio -netdev user,id=user.0,hostfwd=tcp:127.0.0.1:2222-:22,"$QEMU_NET_OPTS" \
    -virtfs local,path=/home/ubuntu/bedrock,security_model=mapped-xattr,mount_tag=bedrock \
    -virtfs local,path=/nix/store,security_model=none,mount_tag=nix-store \
    -virtfs local,path="${SHARED_DIR:-$TMPDIR/xchg}",security_model=none,mount_tag=shared \
    -virtfs local,path="$TMPDIR"/xchg,security_model=none,mount_tag=xchg \
    -drive cache=writeback,file="$NIX_DISK_IMAGE",id=drive1,if=none,index=1,werror=report -device virtio-blk-pci,bootindex=1,drive=drive1,serial=root \
    -device virtio-keyboard \
    -usb \
    -device usb-tablet,bus=usb-bus.0 \
    -kernel ${NIXPKGS_QEMU_KERNEL_bedrock_vm:-/nix/store/5zqk1zrhvxgv1m2424z1k6ivrzsl3p8j-nixos-system-bedrock-vm-26.05pre-git/kernel} \
    -initrd /nix/store/4ljp1cw26f9nhxn2s9avh9rs7ixzvpfd-initrd-linux-6.18.0/initrd \
    -append "$(cat /nix/store/5zqk1zrhvxgv1m2424z1k6ivrzsl3p8j-nixos-system-bedrock-vm-26.05pre-git/kernel-params) init=/nix/store/5zqk1zrhvxgv1m2424z1k6ivrzsl3p8j-nixos-system-bedrock-vm-26.05pre-git/init regInfo=/nix/store/cw37gzgafdqmfk7dggkvrdpx9vhq2n84-closure-info/registration console=tty0 console=ttyS0,115200n8 $QEMU_KERNEL_PARAMS" \
    -nographic \
    -enable-kvm \
    -cpu \
    host \
    -nographic \
    $QEMU_OPTS \
    "$@"
