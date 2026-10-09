#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
task_stage=$(mktemp -d /tmp/bedrock-fair-initrd.XXXXXX)
trap 'chmod -R u+w "$task_stage"; find "$task_stage" -depth -delete' EXIT
output=$PWD/initrd.gz
source_initrd=$PWD/../initrd.gz
helper=$PWD/fair-bench
runner=$PWD/fair-run.sh
cd "$task_stage"
gzip -dc "$source_initrd" | cpio -id --quiet
mkdir -p workload
cp "$helper" workload/fair-bench
cp "$runner" workload/fair-run.sh
chmod +x workload/fair-bench workload/fair-run.sh
find . -print0 | cpio --null --owner=0:0 --quiet -o -H newc | gzip -1 > "$output"
