#!/usr/bin/env bash
# Idempotent registration of payments-stripe tools with Nexus.
#
# Usage:
#   register.sh <url> <network>
#
# Reads tool paths from ../paths.json, fetches each tool's FQN from
# /meta, then registers (or updates) each FQN against the chosen Nexus
# network.

set -euo pipefail

URL="${1:?missing first arg: tool URL}"
NETWORK="${2:?missing second arg: testnet|mainnet}"

case "$NETWORK" in
  testnet|mainnet) ;;
  *) echo "network must be 'testnet' or 'mainnet'"; exit 2 ;;
esac

PATHS_FILE="$(dirname "$0")/../paths.json"
if [[ ! -f "$PATHS_FILE" ]]; then
  echo "expected $PATHS_FILE to enumerate tool paths" >&2
  exit 2
fi

mapfile -t PATHS < <(jq -r '.[]' "$PATHS_FILE")

for path in "${PATHS[@]}"; do
  meta_url="${URL%/}${path}/meta"
  meta="$(curl -fsS "$meta_url")"
  fqn="$(echo "$meta" | jq -r .fqn)"
  description="$(echo "$meta" | jq -r .description)"

  echo "registering $fqn at $URL$path on $NETWORK"

  if nexus tool list --network "$NETWORK" 2>/dev/null | grep -q "$fqn"; then
    nexus tool update offchain \
      --network "$NETWORK" \
      --tool-fqn "$fqn" \
      --url "${URL%/}${path}"
  else
    nexus tool register offchain \
      --network "$NETWORK" \
      --tool-fqn "$fqn" \
      --url "${URL%/}${path}" \
      --description "$description"
  fi
done
