#!/usr/bin/env bash
#
# verify.sh — run cargo check / clippy / test / fmt --check, then a smoke
# test of /health and /meta for every tool path the crate exposes.
#
# Usage:
#   verify.sh <crate-name>
#
# Tools must be at offchain/tools/<crate>/. The workspace lives at
# offchain/Cargo.toml.
#
# Paths are discovered at runtime by parsing the binary's --meta output
# (same mechanism .github/workflows/offchain-tools.prepare.yml uses), so
# this script needs no paths.json.

set -euo pipefail

CRATE="${1:?missing first arg: crate name}"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

if [[ ! -d "offchain/tools/$CRATE" ]]; then
  echo "error: offchain/tools/$CRATE not found" >&2
  exit 1
fi

WORKSPACE_MANIFEST="offchain/Cargo.toml"

echo "==> cargo check"
cargo +stable check --manifest-path "$WORKSPACE_MANIFEST" --package "$CRATE"

echo "==> cargo clippy"
cargo +stable clippy --manifest-path "$WORKSPACE_MANIFEST" --package "$CRATE" -- -D warnings

echo "==> cargo test"
cargo +stable test --manifest-path "$WORKSPACE_MANIFEST" --package "$CRATE"

if command -v just >/dev/null 2>&1; then
  NIGHTLY="$(just helpers::get-nightly-version 2>/dev/null || true)"
  if [[ -n "$NIGHTLY" ]]; then
    echo "==> cargo fmt --check (nightly $NIGHTLY)"
    cargo "+$NIGHTLY" fmt --manifest-path "$WORKSPACE_MANIFEST" --package "$CRATE" --check
  else
    echo "==> skipping fmt --check (nightly version not available)"
  fi
else
  echo "==> skipping fmt --check (just not installed)"
fi

# ---- Smoke test --------------------------------------------------------
# A tool that reads a credential from env at startup will exit 1 if the
# env var is unset. Set a fake key so validate_credentials_at_startup
# passes (every per-crate convention is <CRATE_UPPER_WITH_UNDERSCORES>_API_KEY).
SERVICE_UPPER="$(echo "$CRATE" | tr '[:lower:]-' '[:upper:]_')_API_KEY"
echo "==> smoke test (cargo run + curl /meta + /health)"
echo "    using ${SERVICE_UPPER}=sk_test_FAKE_FOR_VERIFY_ONLY (in process env)"

PORT=$(( RANDOM % 1000 + 38080 ))
BIND_ADDR="127.0.0.1:${PORT}" \
  "$SERVICE_UPPER"=sk_test_FAKE_FOR_VERIFY_ONLY_$( head /dev/urandom | tr -dc A-Za-z0-9 | head -c 32 ) \
  cargo +stable run --manifest-path "$WORKSPACE_MANIFEST" --package "$CRATE" --release &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT

# Wait up to 10s for the server to bind.
for i in $(seq 1 50); do
  if (echo > /dev/tcp/127.0.0.1/$PORT) 2>/dev/null; then break; fi
  sleep 0.2
done

# Discover paths via /meta (works for single-path and multi-path crates).
# /meta on the root returns an array of { fqn, url, ... } across all
# registered tools; each entry's url path is the tool's mount point.
META_JSON="$(curl -fsS "http://127.0.0.1:${PORT}/meta" || true)"
if [[ -z "$META_JSON" ]]; then
  echo "error: failed to fetch /meta from http://127.0.0.1:${PORT}/meta" >&2
  exit 1
fi

# Single-tool crates expose /meta at the root and return an object.
# Multi-tool crates may return an array. Handle both.
if echo "$META_JSON" | jq -e 'type == "array"' >/dev/null; then
  mapfile -t PATHS < <(echo "$META_JSON" | jq -r '.[].url | capture("https?://[^/]+(?<path>/.*)").path // "/"' | sort -u)
else
  PATHS=("")
fi

for path in "${PATHS[@]}"; do
  display="${path:-/}"
  echo "    checking ${display}health"
  curl -fsS "http://127.0.0.1:${PORT}${path}/health" >/dev/null
  echo "    checking ${display}meta"
  curl -fsS "http://127.0.0.1:${PORT}${path}/meta" | jq -e '.fqn // .[0].fqn' >/dev/null
done

echo "==> ok"
