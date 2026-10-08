#!/usr/bin/env bash
# Builds images.tar: the pinned localnet node and txgen load (via the Tempo
# workload's build) plus this workload's ready signaller, trie, TIP-20 and
# chain loads.
set -euo pipefail
cd "$(dirname "$0")"
DOCKER=${DOCKER:-docker}
../tempo/build.sh
trap 'rm -f ready/libvmcall.h ready/ready.c' EXIT
cp ../../guest/libvmcall.h ready/
cp ../integration-tests/ready/ready.c ready/
$DOCKER build -t bedrock/tempo-dst-ready:latest ready/
$DOCKER build -t bedrock/tempo-dst-trie:latest trie/
$DOCKER build -t bedrock/tempo-dst-tip20:latest tip20/
$DOCKER build -t bedrock/tempo-dst-chain:latest chain/
rm -f images.tar
$DOCKER save bedrock/tempo-localnet:pinned bedrock/tempo-txgen:latest \
  bedrock/tempo-dst-ready:latest bedrock/tempo-dst-trie:latest \
  bedrock/tempo-dst-tip20:latest bedrock/tempo-dst-chain:latest > images.tar
echo "Wrote $(pwd)/images.tar ($(du -h images.tar | cut -f1))"
