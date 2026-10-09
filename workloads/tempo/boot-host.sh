#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
NIX=${NIX:-/nix/var/nix/profiles/default/bin/nix}
vm=$($NIX build --impure --no-link --print-out-paths --expr \
  'builtins.dirOf (builtins.dirOf (builtins.getFlake "git+file:///home/ubuntu/bedrock").apps.x86_64-linux.vm.program)')
exec "$vm/bin/run-bedrock-vm-vm"
