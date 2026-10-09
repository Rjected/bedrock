#!/usr/bin/env bash
# Build and test the current worktree on an existing, disposable boxctl box.
# All kernel operations run over SSH on the box, never on this machine.
set -euo pipefail

if [[ $# != 1 || ! $1 =~ ^[a-zA-Z0-9-]+$ ]]; then
    echo "usage: $0 BOX_NAME" >&2
    exit 2
fi

box_name=$1
box_target="ubuntu@${box_name}"
kernel=7.0.0-38-generic
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
evidence_dir="${source_dir}/target/boxctl-evidence/${box_name}"
mkdir -p "$evidence_dir"

ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new "$box_target" 'bash -s' <<'REMOTE'
set -euo pipefail
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    linux-image-7.0.0-38-generic linux-headers-7.0.0-38-generic \
    linux-lib-rust-7.0.0-38-generic rustc-1.91 gcc-13 make kexec-tools \
    curl rsync >/tmp/bedrock-box-packages.log 2>&1
REMOTE

if [[ $(ssh -o BatchMode=yes "$box_target" 'uname -r') != "$kernel" ]]; then
    # SSH may disconnect before systemctl kexec returns to its caller.
    ssh -o BatchMode=yes "$box_target" \
        'sudo kexec -l /boot/vmlinuz-7.0.0-38-generic --initrd=/boot/initrd.img-7.0.0-38-generic --reuse-cmdline && sudo systemctl kexec' \
        || true
fi
for attempt in {1..45}; do
    if [[ $(ssh -o BatchMode=yes -o ConnectTimeout=2 "$box_target" \
        'uname -r' 2>/dev/null || true) == "$kernel" ]]; then
        break
    fi
    sleep 2
done
if [[ $(ssh -o BatchMode=yes -o ConnectTimeout=5 "$box_target" \
    'uname -r') != "$kernel" ]]; then
    echo "box did not boot $kernel" >&2
    exit 1
fi
ssh -o BatchMode=yes "$box_target" \
    'grep -qw svm /proc/cpuinfo && grep -qw npt /proc/cpuinfo && grep -qw perfctr_core /proc/cpuinfo'

ssh -o BatchMode=yes "$box_target" 'bash -s' <<'REMOTE'
set -euo pipefail
if [[ ! -x /home/ubuntu/.cargo/bin/cargo ]]; then
    curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.96.1 \
        >/tmp/bedrock-rustup.log 2>&1
fi
REMOTE

rsync -az --exclude .git --exclude target --exclude '.env*' \
    --exclude .claude --exclude linux --exclude bhyve \
    "$source_dir/" "$box_target:/home/ubuntu/bedrock/"

test_status=0
ssh -o BatchMode=yes "$box_target" 'bash -s' <<'REMOTE' || test_status=$?
set -euo pipefail
cd /home/ubuntu/bedrock
make -C /lib/modules/7.0.0-38-generic/build M="$PWD/crates/bedrock" \
    RUSTC=/usr/bin/rustc-1.91 CC=x86_64-linux-gnu-gcc-13 \
    KRUSTFLAGS='-L /usr/src/linux-lib-rust-7.0.0-38-generic/rust' \
    SVM_ONLY=1 modules >/tmp/bedrock-module-build.log 2>&1
/home/ubuntu/.cargo/bin/cargo build --release -p bedrock-vm \
    --example svm_smoke --example svm_transitions --example svm_bench \
    >/tmp/bedrock-cargo-build.log 2>&1
sudo rmmod bedrock 2>/dev/null || true
if lsmod | grep -q '^kvm_amd '; then sudo rmmod kvm_amd; fi
if lsmod | grep -q '^kvm '; then sudo rmmod kvm; fi
sudo insmod crates/bedrock/bedrock.ko
for test_case in svm_smoke svm_transitions svm_bench; do
    sudo timeout 30 "target/release/examples/$test_case" \
        >"/tmp/bedrock-$test_case.log" 2>&1
done
REMOTE

for log_name in bedrock-module-build bedrock-cargo-build \
    bedrock-svm_smoke bedrock-svm_transitions bedrock-svm_bench; do
    scp -q "$box_target:/tmp/$log_name.log" "$evidence_dir/" 2>/dev/null || true
done
for test_case in svm_smoke svm_transitions svm_bench; do
    if [[ -f $evidence_dir/bedrock-$test_case.log ]]; then
        grep 'PASS' "$evidence_dir/bedrock-$test_case.log" | tail -3 || true
    fi
done
ssh -o BatchMode=yes "$box_target" \
    'uname -r; lscpu | grep -E "Vendor ID|Model name|Virtualization:"' \
    >"$evidence_dir/host.txt"
echo "logs: $evidence_dir"
exit "$test_status"
