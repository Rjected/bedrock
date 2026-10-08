#!/usr/bin/env bash
# Hunts for new bugs: sharded campaigns on the known-fixes node (see
# known-fixes/regression.env), SHARDS processes of SEEDS seeds each, every shard
# its own boot. Failures on this image are novel-bug candidates.
#
#   DOCKER='sudo docker' SHARDS=8 SEEDS=25 ./workloads/tempo-dst/regressions/hunt.sh \
#     [--variant masking] [bedrock-dst campaign args, e.g. --load trie --run-secs 180]
#
# Every load's image (trie, tip20, chain) is in the shards' images tar, so any
# --load works; --reference (passed through to run.sh) adds the E7 reference
# node, which runs the known-fixes image too.
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
  bedrock/tempo-dst-trie:latest bedrock/tempo-dst-tip20:latest \
  bedrock/tempo-dst-chain:latest > "$out/images.tar"
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
# (feature, value) -> [seeds, failed], from each seed's swarm record.
by_feature = collections.defaultdict(lambda: [0, 0])
for summary in sorted(out.glob("shard-*/summary.json")):
    for seed, v in json.loads(summary.read_text())["seeds"].items():
        total += 1
        for sig in v.get("failures", {}):
            by_sig[sig].append(f"{summary.parent.name}/seed-{seed}")
        swarm = v.get("swarm") or {}
        features = {"preempt.period": (swarm.get("preempt") or {}).get("period")}
        features.update({k: swarm.get(k) for k in ("load", "reference", "nemesis")})
        for k, val in features.items():
            if val is not None:
                by_feature[(k, val)][0] += 1
                by_feature[(k, val)][1] += not v.get("pass", False)
print(f"{total} seeds; {len(by_sig)} failing signatures")
for sig, seeds in sorted(by_sig.items(), key=lambda kv: -len(kv[1])):
    print(f"{len(seeds):4d}  {sig}  e.g. {', '.join(seeds[:3])}")
if by_feature:
    print("failed/seeds by swarm feature:")
    for (k, val), (n, failed) in sorted(by_feature.items(), key=lambda kv: str(kv[0])):
        print(f"  {k}={val}: {failed}/{n}")
PY
