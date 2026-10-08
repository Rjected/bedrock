#!/usr/bin/env bash
# Builds the buggy and fixed node images for regression <name> (see README.md):
# bedrock/tempo-localnet:<name>-buggy and bedrock/tempo-localnet:<name>-fixed.
# One of them is the pinned image (bedrock/tempo-localnet:pinned), retagged:
#
# - RETH_REVERT=<commit>: a fix in the pinned reth. buggy = pinned reth with
#   the commit reverted; fixed = pinned.
# - RETH_PATCH=<file in the regression dir>: as RETH_REVERT, for fixes that no
#   longer revert cleanly; buggy = pinned reth with the patch applied.
# - RETH_FIX_PR=<number> RETH_FIX_HEAD=<commit>: an unmerged fix. buggy =
#   pinned; fixed = pinned reth with the PR's changes applied.
#
# BASE_RETH_PRS="<number>:<head commit> ..." applies unmerged fixes for other
# bugs to BOTH images, so a known bug that shares this regression's signature
# can't fire on either side. Neither image is then the pinned one; both are
# built. BASE_RETH_PATCHES="<file in the entry dir> ..." does the same with
# patches.
#
# An entry with only base fixes (no RETH_REVERT/RETH_PATCH/RETH_FIX_PR) builds
# one image, bedrock/tempo-localnet:<name>: e.g. known-fixes, the pinned node
# with every known bug fixed, for campaigns hunting new ones.
#
#   DOCKER='sudo docker' ./workloads/tempo-dst/regressions/build.sh reth-27267
#
# The chef stage (all dependencies, ~17 GB) is built from unpatched reth once
# and shared by every regression; the final stage recompiles only the reverted
# reth crates and their dependents. CHEF_IMAGE overrides it with any cooked
# Tempo chef image at the same revisions.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
name=${1:?usage: build.sh <regression>}
# shellcheck source=/dev/null
. "$here/$name/regression.env"
read -r -a docker <<<"${DOCKER:-docker}"
# Tempo and reth as in the pinned image (bedrock/tempo-localnet:pinned).
tempo_rev=d3f3b28f102946dcdca1a5beb12f328f2f1bbdbd
reth_rev=42fa3c569ad182914d26db410919d6a10c7b4859
chef=${CHEF_IMAGE:-bedrock/tempo-chef:${tempo_rev:0:8}-${reth_rev:0:8}}

work=$(mktemp -d "${TMPDIR:-/tmp}/tempo-regression.XXXXXX")
trap 'rm -rf "$work"' EXIT
git clone -q --filter=blob:none --no-checkout https://github.com/tempoxyz/tempo.git "$work/tempo"
git -C "$work/tempo" checkout -q --detach "$tempo_rev"
git clone -q --filter=blob:none --no-checkout https://github.com/paradigmxyz/reth.git "$work/tempo/reth"
git -C "$work/tempo/reth" checkout -q --detach "$reth_rev"

# Point every reth crate at the local checkout. Tempo's commented patch table
# does not list transitive crates, and Cargo must never mix local and git
# copies of shared traits.
sed -i 's/^exclude = \[/exclude = ["reth", /' "$work/tempo/Cargo.toml"
python3 - "$work/tempo" <<'PY'
import pathlib, sys, tomllib
tempo = pathlib.Path(sys.argv[1])
manifest = tempo / "Cargo.toml"
text = manifest.read_text().split('# [patch."https://github.com/paradigmxyz/reth"]', 1)[0].rstrip()
crates = {}
for path in (tempo / "reth").rglob("Cargo.toml"):
    name = tomllib.loads(path.read_text()).get("package", {}).get("name")
    if name:
        crates[name] = path.parent.relative_to(tempo).as_posix()
text += '\n\n[patch."https://github.com/paradigmxyz/reth"]\n'
text += "".join(f'{n} = {{ path = "{p}" }}\n' for n, p in sorted(crates.items()))
manifest.write_text(text)
PY
# cargo-chef's recipe omits path dependencies' sources; cook needs them.
sed -i '/COPY --from=planner \/app\/recipe.json recipe.json/a COPY reth reth' "$work/tempo/Dockerfile.chef"
sed -i '/COPY --from=planner \/app\/recipe.json recipe.json/i ENV VERGEN_IDEMPOTENT=1' "$work/tempo/Dockerfile.chef"

if ! "${docker[@]}" image inspect "$chef" >/dev/null 2>&1; then
  DOCKER_BUILDKIT=1 "${docker[@]}" build --target builder -f "$work/tempo/Dockerfile.chef" -t "$chef" "$work/tempo"
fi

reth=(git -C "$work/tempo/reth" -c user.name=regression -c user.email=regression@localhost)
# Applies unmerged PR <number> at <head> to the reth checkout and commits it;
# fails unless the tree changed.
apply_pr() {
  local pr=$1 head=$2 before
  "${reth[@]}" fetch -q origin "pull/$pr/head"
  test "$("${reth[@]}" rev-parse FETCH_HEAD)" = "$head" ||
    echo "note: PR $pr has moved past $head; using the pinned head" >&2
  before=$("${reth[@]}" rev-parse HEAD)
  "${reth[@]}" diff "$("${reth[@]}" merge-base "$reth_rev" "$head")" "$head" | "${reth[@]}" apply --index
  "${reth[@]}" commit -q -m "apply paradigmxyz/reth#$pr at $head"
  test -n "$("${reth[@]}" diff --name-only "$before" HEAD)"
  echo "applied reth#$pr: $("${reth[@]}" diff --shortstat "$before" HEAD)"
}
for base in ${BASE_RETH_PRS:-}; do
  apply_pr "${base%%:*}" "${base#*:}"
done
for patch in ${BASE_RETH_PATCHES:-}; do
  "${reth[@]}" apply --index "$here/$name/$patch"
  "${reth[@]}" commit -q -m "apply $patch"
  echo "applied $patch"
done

build() {
  local tag=$name${1:+-$1}
  DOCKER_BUILDKIT=1 "${docker[@]}" build --target tempo-localnet -f "$work/tempo/Dockerfile" \
    --build-arg CHEF_IMAGE="$chef" -t "bedrock/tempo-localnet:$tag" "$work/tempo"
  echo "Built bedrock/tempo-localnet:$tag (reth $("${reth[@]}" log --oneline -1 | cut -c1-80))"
}
# The regression's other side: built from the same base when there is one,
# else the pinned image.
other() {
  if [ -n "${BASE_RETH_PRS:-}${BASE_RETH_PATCHES:-}" ]; then
    "${reth[@]}" checkout -q "$1"
    build "$2"
  else
    "${docker[@]}" tag bedrock/tempo-localnet:pinned "bedrock/tempo-localnet:$name-$2"
    echo "bedrock/tempo-localnet:$name-$2 is the pinned image"
  fi
}
base=$("${reth[@]}" rev-parse HEAD)
if [ -n "${RETH_REVERT:-}" ]; then
  "${reth[@]}" revert --no-edit "$RETH_REVERT"
  build buggy
  other "$base" fixed
elif [ -n "${RETH_PATCH:-}" ]; then
  "${reth[@]}" apply --index "$here/$name/$RETH_PATCH"
  "${reth[@]}" commit -q -m "regression $name"
  build buggy
  other "$base" fixed
elif [ -n "${RETH_FIX_PR:-}" ]; then
  apply_pr "$RETH_FIX_PR" "$RETH_FIX_HEAD"
  build fixed
  other "$base" buggy
else
  build ""
fi
