#!/usr/bin/env bash
# Hunts for new bugs: sharded campaigns on the known-fixes node (see
# known-fixes/regression.env), SHARDS processes of SEEDS seeds each, every shard
# its own boot. Failures on this image are novel-bug candidates.
#
#   DOCKER='sudo docker' SHARDS=8 SEEDS=25 ./workloads/tempo-dst/regressions/hunt.sh \
#     [--variant masking] [bedrock-dst campaign args, e.g. --load trie --run-secs 180]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
read -r -a docker <<<"${DOCKER:-docker}"
out=${OUT:-$PWD/hunt-$(date +%Y%m%d-%H%M%S)}
shards=${SHARDS:-8} seeds=${SEEDS:-25} start=${SEED_START:-0}
variant=default
if [ "${1:-}" = --variant ]; then variant=$2; shift 2; fi
image=bedrock/tempo-localnet:known-fixes
mkdir -p "$out"
"${docker[@]}" image inspect "$image" >/dev/null 2>&1 || "$here/build.sh" known-fixes
DOCKER="${DOCKER:-docker}" "$here/../build.sh" >/dev/null
"${docker[@]}" save "$image" bedrock/tempo-txgen:latest bedrock/tempo-dst-ready:latest \
  bedrock/tempo-dst-trie:latest > "$out/images.tar"
# Build the Nix inputs once; the shards then only hit the store cache.
nix=${NIX:-/nix/var/nix/profiles/default/bin/nix}
(cd "$here/../../.." && $nix build .#bedrock-dst .#guestKernel .#podmanInitrd --no-link)
for i in $(seq 0 $((shards - 1))); do
  first=$((start + i * seeds))
  RUN_DIR="$out/shard-$i-inputs" IMAGES="$out/images.tar" "$here/../run.sh" --variant "$variant" --node-image "$image" \
    --seed-start "$first" --seeds "$seeds" --out "$out/shard-$i" "$@" > "$out/shard-$i.log" 2>&1 &
done
wait || true
python3 - "$out" <<'PY'
import collections, json, pathlib, sys
out = pathlib.Path(sys.argv[1])
by_sig, total = collections.defaultdict(list), 0
for summary in sorted(out.glob("shard-*/summary.json")):
    for seed, v in json.loads(summary.read_text())["seeds"].items():
        total += 1
        for sig in v.get("failures", {}):
            by_sig[sig].append(f"{summary.parent.name}/seed-{seed}")
print(f"{total} seeds; {len(by_sig)} failing signatures")
for sig, seeds in sorted(by_sig.items(), key=lambda kv: -len(kv[1])):
    print(f"{len(seeds):4d}  {sig}  e.g. {', '.join(seeds[:3])}")
PY
