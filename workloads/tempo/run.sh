#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
mode=${1:-smoke}
case "$mode" in
  smoke) compose=compose.yaml ;;
  txgen)
    TXGEN_COUNT=${TXGEN_COUNT:-1000} TXGEN_TPS=${TXGEN_TPS:-100} python3 - <<'PY'
from pathlib import Path
import os
count, tps = int(os.environ['TXGEN_COUNT']), int(os.environ['TXGEN_TPS'])
assert count > 0 and tps >= 0
root = Path('workloads/tempo')
s = (root / 'compose-txgen.yaml').read_text()
s = s.replace('TXGEN_COUNT: "1000"', f'TXGEN_COUNT: "{count}"')
s = s.replace('TXGEN_TPS: "100"', f'TXGEN_TPS: "{tps}"')
(root / 'compose-txgen-run.yaml').write_text(s)
PY
    compose=compose-txgen-run.yaml ;;
  *) echo 'Usage: run.sh [smoke|txgen]' >&2; exit 2 ;;
esac
if [ "$#" -gt 0 ]; then shift; fi
NIX=${NIX:-/nix/var/nix/profiles/default/bin/nix}
cli=$($NIX build .#bedrock-cli --no-link --print-out-paths)
kernel=$($NIX build .#guestKernel --no-link --print-out-paths)
initrd=$($NIX build .#tempoInitrd --no-link --print-out-paths)
if [ ! -c /dev/bedrock ]; then
  # The nested VM reads the shared workspace path, not the host's Nix store.
  cp "$initrd" workloads/tempo/initrd.gz
  remote_args=
  if [ "$#" -gt 0 ]; then printf -v remote_args '%q ' "$@"; fi
  sshpass -p root ssh -p 2222 -o StrictHostKeyChecking=accept-new root@127.0.0.1 \
    "mkdir -p /tmp/tempo-workload; cp '/home/dev/bedrock/workloads/tempo/$compose' /tmp/tempo-workload/compose.yaml; cp /home/dev/bedrock/workloads/tempo/images.tar /tmp/tempo-workload/images.tar"
  exec sshpass -p root ssh -p 2222 root@127.0.0.1 \
    "bedrock-cli -m 16384 -c 'console=hvc0 nopti nokaslr mitigations=off break audit=0 bedrock_ncpus=5' -i /home/dev/bedrock/workloads/tempo/initrd.gz --file compose.yaml=/tmp/tempo-workload/compose.yaml --file images.tar=/tmp/tempo-workload/images.tar --wall-clock-timeout 1800 '$kernel/vmlinux' $remote_args"
fi
exec "$cli/bin/bedrock-cli" -m 16384 \
  -c 'console=hvc0 nopti nokaslr mitigations=off break audit=0 bedrock_ncpus=5' \
  -i "$initrd" \
  --file compose.yaml=workloads/tempo/"$compose" \
  --file images.tar=workloads/tempo/images.tar \
  --wall-clock-timeout 1800 "$kernel/vmlinux" "$@"
