#!/usr/bin/env bash
# Run the pinned one-seed Tempo CI workload on a disposable, prepared box.
# Kernel modules are built and loaded only by run-boxctl-svm.sh on the box.
set -euo pipefail

if (( $# < 2 || $# > 4 )); then
    echo 'usage: run-boxctl-tempo-ci.sh BOX_NAME RUN_NAME [pinned|progress] [RUN_SECS]' >&2
    exit 2
fi
box_name=$1
run_name=$2
driver=${3:-pinned}
run_secs=${4:-180}
[[ $box_name =~ ^[a-zA-Z0-9-]+$ && $run_name =~ ^[a-zA-Z0-9-]+$ ]] || exit 2
[[ $driver == pinned || $driver == progress ]] || exit 2
[[ $run_secs =~ ^[1-9][0-9]*$ ]] || exit 2

source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
box_target="ubuntu@${box_name}"
ssh_opts=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10)

if [[ $driver == progress ]]; then
    # Userland-only build. This driver logs virtual-time checkpoints for
    # diagnostics; final timing comparisons use the pinned CI binary.
    (cd "$source_dir" && cargo build --release -p bedrock-dst)
    rsync -az -e 'ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new' \
        "$source_dir/target/release/bedrock-dst" \
        "$box_target:/home/ubuntu/ci-dst/bedrock-dst-progress"
fi

ssh "${ssh_opts[@]}" "$box_target" bash -s -- \
    "$run_name" "$driver" "$run_secs" <<'REMOTE'
set -uo pipefail
run_name=$1
driver=$2
run_secs=$3
cd /home/ubuntu/ci-dst || exit 1
export PATH="$PWD:$PATH"
test -x ./bedrock-dst && test -f ./images.tar && test -f ./compose-run.yaml || exit 1
grep -q '^bedrock ' /proc/modules || { echo 'Bedrock module is not loaded on the box' >&2; exit 1; }
output="results-ci-${run_name}"
log="campaign-ci-${run_name}.log"
status_file="campaign-ci-${run_name}-status.txt"
test ! -e "$output" && test ! -e "$status_file" || {
    echo "Run name already exists: $run_name" >&2
    exit 2
}
args=(campaign --vmlinux ./vmlinux --initrd ./initrd.gz
    --compose ./compose-run.yaml --images ./images.tar
    --cmdline 'console=hvc0 nopti nokaslr mitigations=off break audit=0 bedrock_ncpus=5 bedrock_loop_delay lpj=1000000'
    --memory-mb 5120 --seed-start 14373434789495652599 --seeds 1
    --workload-cmd 'podman exec tempo-dst /usr/local/bin/tempo-dst'
    --workload-planner ./tempo-dst --workload-config ./workload-args.json
    --out "./$output")
if [[ $run_secs != 180 ]]; then
    args+=(--run-secs "$run_secs")
    echo 'Diagnostic duration changes the workload verdict; do not compare it to Intel CI.' >&2
fi
binary=./bedrock-dst
if [[ $driver == progress ]]; then
    binary=./bedrock-dst-progress
    export BEDROCK_SEED_EXIT_PROGRESS=1
fi
start=$(date +%s)
printf 'started_utc=%s\ndriver=%s\nrun_secs=%s\n' \
    "$(date -u +%FT%TZ)" "$driver" "$run_secs" >"$status_file"
timeout 1250 "$binary" "${args[@]}" >"$log" 2>&1
process_status=$?
failed=unknown
if [[ -f $output/summary.json ]]; then
    failed=$(python3 - "$output/summary.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))['failed'])
PY
)
fi
printf 'process_status=%s\nfailed=%s\nwall_seconds=%s\nfinished_utc=%s\n' \
    "$process_status" "$failed" "$(( $(date +%s) - start ))" \
    "$(date -u +%FT%TZ)" >>"$status_file"
tail -n 12 "$log"
cat "$status_file"
if [[ $process_status != 0 || $failed != 0 ]]; then exit 1; fi
REMOTE
