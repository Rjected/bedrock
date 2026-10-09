#!/bin/sh
set -eu
rpc() {
  curl -fsS --max-time 5 -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":[]}" \
    http://127.0.0.1:8545 | jq -er '.result'
}
cpus=$(nproc)
echo "Tempo check: reported CPUs=$cpus"
test "$cpus" = 5
for attempt in $(seq 1 300); do
  chain=$(rpc eth_chainId 2>/dev/null) && break
  sleep 1
done
test "${chain:-}" = 0x539
echo "Tempo check: chain ID=$chain (1337)"
first=$(rpc eth_blockNumber)
for attempt in $(seq 1 60); do
  sleep 1
  last=$(rpc eth_blockNumber)
  if [ "$((last))" -gt "$((first))" ]; then
    echo "TEMPO_BEDROCK_PASS: blocks advanced $first -> $last"
    /usr/local/bin/bedrock-shutdown --ready
    sleep 2
    exec /usr/local/bin/bedrock-shutdown
  fi
done
echo 'TEMPO_BEDROCK_FAIL: blocks did not advance' >&2
exit 1
