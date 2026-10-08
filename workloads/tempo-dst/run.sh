#!/usr/bin/env bash
# Runs a DST campaign: boot once, warm the node, checkpoint, then one branch per
# seed with crash nemesis, thread-fuzz schedules, txgen load, and oracles.
#
#   ./workloads/tempo-dst/run.sh [--variant default|no-prewarm|parallel] [bedrock-dst campaign args...]
#
# e.g. ./workloads/tempo-dst/run.sh --seeds 20 --run-secs 180 --out /tmp/dst-out
#
# Variants that change node flags change the compose file, hence the boot
# prefix: compare variants by campaign, not by seed within one campaign.
set -euo pipefail
cd "$(dirname "$0")/../.."
variant=default
if [ "${1:-}" = --variant ]; then variant=$2; shift 2; fi
root=workloads/tempo-dst
python3 - "$variant" <<'PY'
import sys
from pathlib import Path
flags = {
    "default": [],
    "no-prewarm": ["--builder.disable-prewarming"],
    "parallel": ["--builder.parallel"],
}[sys.argv[1]]
root = Path("workloads/tempo-dst")
s = (root / "compose.yaml").read_text()
anchor = "      - --engine.state-root-task-compare-updates\n"
s = s.replace(anchor, anchor + "".join(f"      - {f}\n" for f in flags))
(root / "compose-run.yaml").write_text(s)
PY
NIX=${NIX:-/nix/var/nix/profiles/default/bin/nix}
dst=$($NIX build .#bedrock-dst --no-link --print-out-paths)
kernel=$($NIX build .#guestKernel --no-link --print-out-paths)
initrd=$($NIX build .#podmanInitrd --no-link --print-out-paths)
./workloads/tempo/prepare-initrd.sh "$initrd"
images=${IMAGES:-$root/images.tar}
sha256sum "$kernel/vmlinux" workloads/tempo/initrd.gz "$images" "$root/compose-run.yaml" \
  > "$root/inputs.sha256"
if [ ! -c /dev/bedrock ]; then
  echo "no /dev/bedrock: load bedrock.ko on a bare-metal host with EPT-friendly PEBS" >&2
  exit 1
fi
exec "$dst/bin/bedrock-dst" campaign --vmlinux "$kernel/vmlinux" \
  --initrd workloads/tempo/initrd.gz --compose "$root/compose-run.yaml" --images "$images" "$@"
