#!/usr/bin/env bash
# Builds images.tar: the pinned localnet node and txgen load (via the Tempo
# workload's build) plus this workload's ready signaller.
set -euo pipefail
cd "$(dirname "$0")"
DOCKER=${DOCKER:-docker}
../tempo/build.sh
trap 'rm -f ready/libvmcall.h ready/ready.c' EXIT
cp ../../guest/libvmcall.h ready/
cp ../integration-tests/ready/ready.c ready/
$DOCKER build -t bedrock/tempo-dst-ready:latest ready/
rm -f images.tar
$DOCKER save bedrock/tempo-localnet:pinned bedrock/tempo-txgen:latest \
  bedrock/tempo-dst-ready:latest > images.tar
echo "Wrote $(pwd)/images.tar ($(du -h images.tar | cut -f1))"
