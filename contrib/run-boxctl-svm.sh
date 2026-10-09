#!/usr/bin/env bash
# Build and test Bedrock on a disposable boxctl box. No local kernel commands.
set -euo pipefail

usage() {
    echo "usage: $0 [prepare|test|collect|all] BOX_NAME" >&2
    echo "       $0 BOX_NAME  # prepare and test" >&2
    exit 2
}

if [[ $# == 1 ]]; then
    mode=all
    box_name=$1
elif [[ $# == 2 ]]; then
    mode=$1
    box_name=$2
else
    usage
fi
case "$mode" in prepare|test|collect|all) ;; *) usage ;; esac
[[ $box_name =~ ^[a-zA-Z0-9-]+$ ]] || usage

box_target="ubuntu@${box_name}"
ssh_opts=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new
    -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3)
rsync_ssh='ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3'
kernel=7.0.0-38-generic
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
evidence_dir="${source_dir}/target/boxctl-evidence/${box_name}"

check_host() {
    ssh "${ssh_opts[@]}" "$box_target" 'bash -s' <<'REMOTE'
set -euo pipefail
test "$(uname -r)" = 7.0.0-38-generic || {
    echo 'run prepare first: box is not booted into Linux 7.0.0-38-generic' >&2
    exit 1
}
for flag in svm npt perfctr_core; do
    grep -qw "$flag" /proc/cpuinfo || {
        echo "AMD CPU flag $flag is unavailable" >&2
        exit 1
    }
done
REMOTE
}

prepare_box() {
    ssh "${ssh_opts[@]}" "$box_target" 'bash -s' <<'REMOTE'
set -euo pipefail
packages=(linux-image-7.0.0-38-generic linux-headers-7.0.0-38-generic
    linux-lib-rust-7.0.0-38-generic rustc-1.91 gcc-13 make kexec-tools curl rsync)
for package in "${packages[@]}"; do
    if ! dpkg -s "$package" >/dev/null 2>&1; then
        sudo apt-get update -qq
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
            "${packages[@]}" >/tmp/bedrock-box-packages.log 2>&1
        break
    fi
done
REMOTE

    if [[ $(ssh "${ssh_opts[@]}" "$box_target" 'uname -r') != "$kernel" ]]; then
        # A successful kexec drops SSH before the command can return.
        ssh "${ssh_opts[@]}" "$box_target" \
            'sudo kexec -l /boot/vmlinuz-7.0.0-38-generic --initrd=/boot/initrd.img-7.0.0-38-generic --reuse-cmdline && sudo systemctl kexec' \
            || true
        for attempt in {1..45}; do
            if [[ $(ssh "${ssh_opts[@]}" -o ConnectTimeout=2 "$box_target" \
                'uname -r' 2>/dev/null || true) == "$kernel" ]]; then
                break
            fi
            sleep 2
        done
    fi
    check_host

    ssh "${ssh_opts[@]}" "$box_target" 'bash -s' <<'REMOTE'
set -euo pipefail
if [[ ! -x /home/ubuntu/.cargo/bin/cargo ]]; then
    curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.96.1 \
        >/tmp/bedrock-rustup.log 2>&1
fi
REMOTE
    echo "prepared $box_name: $(ssh "${ssh_opts[@]}" "$box_target" 'uname -r')"
}

collect_logs() {
    mkdir -p "$evidence_dir"
    timeout 30 rsync -az -e "$rsync_ssh" --include 'bedrock-*.log' --exclude '*' \
        "$box_target:/tmp/" "$evidence_dir/"
    ssh "${ssh_opts[@]}" "$box_target" \
        'uname -r; lscpu | grep -E "Vendor ID|Model name|Virtualization:"' \
        >"$evidence_dir/host.txt"
    echo "logs: $evidence_dir"
}

test_box() {
    check_host
    mkdir -p "$evidence_dir"
    rm -f "$evidence_dir"/bedrock-{module-build,cargo-build,svm_smoke,svm_transitions,svm_bench,svm_workload,native,native-branches}.log
    rsync -az -e "$rsync_ssh" --exclude .git --exclude target --exclude '.env*' \
        --exclude .claude --exclude linux --exclude bhyve \
        "$source_dir/" "$box_target:/home/ubuntu/bedrock/"

    test_status=0
    ssh "${ssh_opts[@]}" "$box_target" 'bash -s' <<'REMOTE' || test_status=$?
set -euo pipefail
cd /home/ubuntu/bedrock
rm -f /tmp/bedrock-{module-build,cargo-build,svm_smoke,svm_transitions,svm_bench,svm_workload,native,native-branches}.log
# rsync can restore source timestamps older than an existing module object.
# Always rebuild from clean sources so a box never runs an earlier variant.
echo 'boxctl test: building kernel module'
make -C /lib/modules/7.0.0-38-generic/build M="$PWD/crates/bedrock" \
    RUSTC=/usr/bin/rustc-1.91 CC=x86_64-linux-gnu-gcc-13 \
    KRUSTFLAGS='-L /usr/src/linux-lib-rust-7.0.0-38-generic/rust' \
    SVM_ONLY=1 clean >/tmp/bedrock-module-clean.log 2>&1
make -C /lib/modules/7.0.0-38-generic/build M="$PWD/crates/bedrock" \
    RUSTC=/usr/bin/rustc-1.91 CC=x86_64-linux-gnu-gcc-13 \
    KRUSTFLAGS='-L /usr/src/linux-lib-rust-7.0.0-38-generic/rust' \
    SVM_ONLY=1 modules >/tmp/bedrock-module-build.log 2>&1
echo 'boxctl test: building guest examples'
/home/ubuntu/.cargo/bin/cargo build --release -p bedrock-vm \
    --example svm_smoke --example svm_transitions --example svm_bench \
    --example svm_workload \
    >/tmp/bedrock-cargo-build.log 2>&1
echo 'boxctl test: loading module inside box'
sudo rmmod bedrock 2>/dev/null || true
if [[ -d /sys/module/kvm_amd ]]; then sudo rmmod kvm_amd; fi
if [[ -d /sys/module/kvm ]]; then sudo rmmod kvm; fi
sudo insmod crates/bedrock/bedrock.ko
echo 'boxctl test: running mixed memory workload'
sudo timeout 30 taskset -c 1 target/release/examples/svm_workload 1024 3 \
    >/tmp/bedrock-svm_workload.log 2>&1
for test_case in svm_smoke svm_transitions svm_bench; do
    echo "boxctl test: running $test_case"
    sudo timeout 30 "target/release/examples/$test_case" \
        >"/tmp/bedrock-$test_case.log" 2>&1
done
echo 'boxctl test: running straight native comparison'
sudo timeout 20 taskset -c 1 target/release/examples/svm_bench native \
    >/tmp/bedrock-native.log 2>&1
echo 'boxctl test: running branched native comparison'
sudo timeout 20 taskset -c 1 target/release/examples/svm_bench native-branches \
    >/tmp/bedrock-native-branches.log 2>&1
REMOTE
    collect_logs || true
    for name in svm_workload svm_smoke svm_transitions svm_bench native native-branches; do
        if [[ -f $evidence_dir/bedrock-$name.log ]]; then
            grep 'PASS' "$evidence_dir/bedrock-$name.log" | tail -3 || true
        fi
    done
    return "$test_status"
}

case "$mode" in
    prepare) prepare_box ;;
    test) test_box ;;
    collect) collect_logs ;;
    all) prepare_box; test_box ;;
esac
