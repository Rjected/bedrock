#!/usr/bin/env bash
# Runs regression <name> (see README.md): the generic campaign, identical
# seeds, against the buggy and the fixed node image. It passes when some
# buggy-image seed fails with a signature starting with EXPECT and no
# fixed-image seed does.
#
#   DOCKER='sudo docker' ./workloads/tempo-dst/regressions/run.sh reth-27267 --seeds 20 --run-secs 180
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
name=${1:?usage: run.sh <regression> [bedrock-dst campaign args...]}
shift
# shellcheck source=/dev/null
. "$here/$name/regression.env"
read -r -a docker <<<"${DOCKER:-docker}"
out=${OUT:-$PWD/regression-$name}
mkdir -p "$out"

"$here/build.sh" "$name"
DOCKER="${DOCKER:-docker}" "$here/../build.sh"
for side in buggy fixed; do
  image=bedrock/tempo-localnet:$name-$side
  "${docker[@]}" save "$image" bedrock/tempo-txgen:latest bedrock/tempo-dst-ready:latest \
    bedrock/tempo-dst-trie:latest > "$out/images-$side.tar"
  # shellcheck disable=SC2086 # CAMPAIGN_ARGS is a word list
  IMAGES="$out/images-$side.tar" "$here/../run.sh" --variant "$VARIANT" --node-image "$image" \
    $CAMPAIGN_ARGS --out "$out/$side" "$@"
done

python3 - "$out" "$EXPECT" <<'PY'
import json, pathlib, sys
out, expect = pathlib.Path(sys.argv[1]), sys.argv[2]
hits = {}
for side in ("buggy", "fixed"):
    seeds = json.loads((out / side / "summary.json").read_text())["seeds"]
    hits[side] = sorted(s for s, v in seeds.items()
                        if any(f.startswith(expect) for f in v.get("failures", {})))
    print(f"{side}: {len(hits[side])}/{len(seeds)} seeds hit {expect}*: {hits[side]}")
ok = hits["buggy"] and not hits["fixed"]
print("REGRESSION", "REPRODUCED" if ok else "NOT REPRODUCED")
sys.exit(0 if ok else 1)
PY
