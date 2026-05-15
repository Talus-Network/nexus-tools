#!/usr/bin/env bash
#
# new_tool.sh — wrap `nexus tool new` and seed local templates over the crate.
#
# Usage:
#   new_tool.sh off-chain <category>-<service>       # off-chain Rust tool
#   new_tool.sh on-chain  <service>                  # on-chain Move tool
#
# Run from the nexus-tools repo root.

set -euo pipefail

KIND="${1:?missing first arg: off-chain|on-chain}"
NAME="${2:?missing second arg: crate or module name}"

if ! command -v nexus >/dev/null 2>&1; then
  echo "error: 'nexus' CLI not found in PATH" >&2
  echo "  install: see https://github.com/Talus-Network/nexus-sdk/tree/main/cli" >&2
  exit 127
fi

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

case "$KIND" in
  off-chain)
    if [[ -d "tools/$NAME" ]]; then
      echo "tools/$NAME already exists; refusing to clobber" >&2
      exit 1
    fi
    nexus tool new --name "$NAME" --template rust
    mkdir -p tools
    mv "$NAME" "tools/$NAME"
    mkdir -p "tools/$NAME/deploy"
    # Caller (the skill) will now render templates over tools/$NAME/.
    echo "scaffolded tools/$NAME"
    ;;

  on-chain)
    if [[ -d "$NAME" ]]; then
      echo "$NAME already exists; refusing to clobber" >&2
      exit 1
    fi
    nexus tool new --name "${NAME}_onchain" --template move
    echo "scaffolded ${NAME}_onchain"
    ;;

  *)
    echo "unknown kind: $KIND (expected off-chain|on-chain)" >&2
    exit 2
    ;;
esac
