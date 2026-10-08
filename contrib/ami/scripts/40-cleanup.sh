#!/usr/bin/env bash
# Generalize the instance before the snapshot: drop verification artifacts and
# per-instance identity so every launch gets fresh host keys and machine-id.
set -euo pipefail

rm -rf /tmp/bedrock-src /tmp/bedrock-src.tar.gz /tmp/nix-install
apt-get clean
rm -rf /var/lib/apt/lists/*
cloud-init clean --logs --seed
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id
sync
