#!/usr/bin/env bash
# Runs a DST campaign: boot once, warm the node, checkpoint, then one branch per
# seed with crash nemesis, thread-fuzz schedules, txgen load, and oracles.
#
#   ./workloads/tempo-dst/run.sh [--variant default|no-prewarm|parallel|masking]
#     [--node-image IMAGE] [bedrock-dst campaign args...]
#
# --node-image runs another Tempo image (e.g. a regression's buggy build; see
# regressions/) in place of bedrock/tempo-localnet:pinned; it must be in the
# images tar (IMAGES).
#
# e.g. ./workloads/tempo-dst/run.sh --seeds 20 --run-secs 180 --out /tmp/dst-out
#
# Variants that change node flags change the compose file, hence the boot
# prefix: compare variants by campaign, not by seed within one campaign.
#
# RUN_DIR (default workloads/tempo-dst) holds this campaign's generated inputs:
# compose-run.yaml, initrd.gz and inputs.sha256. Concurrent campaigns need
# distinct RUN_DIRs; replay reads them from there.
set -euo pipefail
cd "$(dirname "$0")/../.."
variant=default
node_image=bedrock/tempo-localnet:pinned
while true; do
  case "${1:-}" in
    --variant) variant=$2; shift 2 ;;
    --node-image) node_image=$2; shift 2 ;;
    *) break ;;
  esac
done
root=workloads/tempo-dst
mkdir -p "${RUN_DIR:-$root}"
run_dir=$(cd "${RUN_DIR:-$root}" && pwd)
python3 - "$variant" "$node_image" "$run_dir" <<'PY'
import sys
from pathlib import Path
flags = {
    "default": [],
    "no-prewarm": ["--builder.disable-prewarming"],
    "parallel": ["--builder.parallel"],
    # Persist every 3 blocks with the state-trie frontier 1 block behind the
    # persisted tip: trie reverts and historical overlays run constantly.
    "masking": [
        "--engine.persistence-threshold", "3",
        "--engine.num-state-masking-blocks", "1",
        "--engine.memory-block-buffer-target", "0",
    ],
}[sys.argv[1]]
root = Path("workloads/tempo-dst")
s = (root / "compose.yaml").read_text()
anchor = "      - --engine.state-root-task-compare-updates\n"
s = s.replace(anchor, anchor + "".join(f"      - {f}\n" for f in flags))
s = s.replace("image: bedrock/tempo-localnet:pinned", f"image: {sys.argv[2]}")
(Path(sys.argv[3]) / "compose-run.yaml").write_text(s)
PY
NIX=${NIX:-/nix/var/nix/profiles/default/bin/nix}
dst=$($NIX build .#bedrock-dst --no-link --print-out-paths)
kernel=$($NIX build .#guestKernel --no-link --print-out-paths)
initrd=$($NIX build .#podmanInitrd --no-link --print-out-paths)
./workloads/tempo/prepare-initrd.sh "$initrd" "$run_dir/initrd.gz"
images=${IMAGES:-$root/images.tar}
sha256sum "$kernel/vmlinux" "$run_dir/initrd.gz" "$images" "$run_dir/compose-run.yaml" \
  > "$run_dir/inputs.sha256"
if [ ! -c /dev/bedrock ]; then
  echo "no /dev/bedrock: load bedrock.ko on a bare-metal host with EPT-friendly PEBS" >&2
  exit 1
fi
exec "$dst/bin/bedrock-dst" campaign --vmlinux "$kernel/vmlinux" \
  --initrd "$run_dir/initrd.gz" --compose "$run_dir/compose-run.yaml" --images "$images" "$@"
