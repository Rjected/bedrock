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
if [ -n "${RETH_REVERT:-}" ]; then
  "${reth[@]}" revert --no-edit "$RETH_REVERT"
  built=buggy pinned=fixed
elif [ -n "${RETH_PATCH:-}" ]; then
  "${reth[@]}" apply --index "$here/$name/$RETH_PATCH"
  built=buggy pinned=fixed
else
  "${reth[@]}" fetch -q origin "pull/$RETH_FIX_PR/head"
  test "$("${reth[@]}" rev-parse FETCH_HEAD)" = "$RETH_FIX_HEAD" ||
    echo "note: PR $RETH_FIX_PR has moved past $RETH_FIX_HEAD; using the pinned head" >&2
  "${reth[@]}" diff "$("${reth[@]}" merge-base "$reth_rev" "$RETH_FIX_HEAD")" "$RETH_FIX_HEAD" |
    "${reth[@]}" apply --3way
  built=fixed pinned=buggy
fi
DOCKER_BUILDKIT=1 "${docker[@]}" build --target tempo-localnet -f "$work/tempo/Dockerfile" \
  --build-arg CHEF_IMAGE="$chef" -t "bedrock/tempo-localnet:$name-$built" "$work/tempo"
"${docker[@]}" tag bedrock/tempo-localnet:pinned "bedrock/tempo-localnet:$name-$pinned"
echo "Built bedrock/tempo-localnet:$name-$built; $name-$pinned is the pinned image"
