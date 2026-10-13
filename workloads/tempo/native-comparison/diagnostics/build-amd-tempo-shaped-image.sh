#!/usr/bin/env bash
# Rebuild the measured AMD guest-shaping diagnostic from the pinned OCI input.
set -euo pipefail

if [[ $# != 2 ]]; then
    echo "usage: $0 ORIGINAL_IMAGES_TAR OUTPUT_IMAGES_TAR" >&2
    exit 2
fi

diagnostics_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
source_archive=$(realpath -- "$1")
output_archive=$(realpath -m -- "$2")
if [[ $source_archive == "$output_archive" ]]; then
    echo 'input and output archive must differ' >&2
    exit 2
fi

temporary=$(mktemp -d -t bedrock-amd-tempo-shaped.XXXXXXXX)
trap 'rm -rf -- "$temporary"' EXIT

python3 "$diagnostics_dir/relocate-libc-rep-oci.py" \
    "$source_archive" "$temporary/tempo-two-site.tar"
python3 "$diagnostics_dir/relocate-txgen-libc-rep-four-oci.py" \
    "$temporary/tempo-two-site.tar" "$temporary/tempo-and-txgen.tar"
python3 "$diagnostics_dir/relocate-tempo-libc-rep-third-oci.py" \
    "$temporary/tempo-and-txgen.tar" "$output_archive"
