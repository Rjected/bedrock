#!/usr/bin/env bash
set -euo pipefail

port=${1:-8765}
[[ $port =~ ^[0-9]+$ ]] && (( port >= 1024 && port <= 65535 )) || {
    echo 'usage: serve.sh [port 1024-65535]' >&2
    exit 2
}
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
echo "Serving http://127.0.0.1:${port}/docs/virtualization-guide/"
exec python3 -m http.server "$port" --bind 127.0.0.1 --directory "$repo_root"
