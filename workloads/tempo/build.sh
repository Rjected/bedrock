#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
DOCKER=${DOCKER:-docker}
$DOCKER pull ghcr.io/tempoxyz/tempo-localnet@sha256:9ab8e4ffe86cd397106d7462657b41a2723374f2ed554d0aac3afb1b54f24aa6
$DOCKER tag ghcr.io/tempoxyz/tempo-localnet@sha256:9ab8e4ffe86cd397106d7462657b41a2723374f2ed554d0aac3afb1b54f24aa6 bedrock/tempo-localnet:pinned
cp ../../guest/libvmcall.h check/
cp ../bitcoin/shutdown/shutdown.c check/
trap 'rm -f check/libvmcall.h check/shutdown.c' EXIT
$DOCKER build -t bedrock/tempo-check:latest check/
$DOCKER pull ghcr.io/tempoxyz/txgen@sha256:5a13376e67209c218375ec42ae21e83dfca2998491b2c83292a0c722f4a96fbf
# An array, not `printf | head -n1`: under pipefail, head closing early SIGPIPEs
# printf (exit 141) once the store holds enough matching paths.
file_stores=(/nix/store/*bedrock-file-store*/bin/bedrock-file-store)
file_store=${file_stores[0]}
test -f "$file_store"
install -m 755 "$file_store" txgen/bedrock-file-store
$DOCKER build -t bedrock/tempo-txgen:latest txgen/
$DOCKER save bedrock/tempo-localnet:pinned bedrock/tempo-check:latest bedrock/tempo-txgen:latest > images.tar
chmod a+r images.tar
