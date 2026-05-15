#!/usr/bin/env bash
#
# verify.sh — run cargo check / clippy / test / fmt --check, then a smoke
# test of /health and /meta for every tool path the crate exposes.
#
# Usage:
#   verify.sh <crate-name>

set -euo pipefail

CRATE="${1:?missing first arg: crate name}"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

echo "==> cargo check"
cargo +stable check --package "$CRATE"

echo "==> cargo clippy"
cargo +stable clippy --package "$CRATE" -- -D warnings

echo "==> cargo test"
cargo +stable test --package "$CRATE"

if command -v just >/dev/null 2>&1; then
  NIGHTLY="$(just helpers::get-nightly-version 2>/dev/null || true)"
  if [[ -n "$NIGHTLY" ]]; then
    echo "==> cargo fmt --check (nightly $NIGHTLY)"
    cargo "+$NIGHTLY" fmt --package "$CRATE" --check
  else
    echo "==> skipping fmt --check (nightly version not available)"
  fi
else
  echo "==> skipping fmt --check (just not installed)"
fi

echo "==> smoke test (cargo run + curl /health + /meta)"
PORT=$(( RANDOM % 1000 + 38080 ))
BIND_ADDR="127.0.0.1:${PORT}" cargo +stable run --package "$CRATE" --release &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT

# Wait up to 10s for the server to bind.
for i in $(seq 1 50); do
  if (echo > /dev/tcp/127.0.0.1/$PORT) 2>/dev/null; then break; fi
  sleep 0.2
done

PATHS_FILE="tools/$CRATE/paths.json"
if [[ -f "$PATHS_FILE" ]]; then
  mapfile -t PATHS < <(jq -r '.[]' "$PATHS_FILE")
else
  PATHS=("")
fi

for path in "${PATHS[@]}"; do
  echo "    checking ${path}/health"
  curl -fsS "http://127.0.0.1:${PORT}${path}/health" >/dev/null
  echo "    checking ${path}/meta"
  curl -fsS "http://127.0.0.1:${PORT}${path}/meta" | jq -e .fqn >/dev/null
done

echo "==> ok"
