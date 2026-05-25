#!/usr/bin/env bash
#
# new_tool.sh — wrap `nexus tool new` and seed local templates over the crate.
#
# Usage:
#   new_tool.sh off-chain <category>-<service>       # off-chain Rust tool
#   new_tool.sh on-chain  <service>                  # on-chain Move tool
#
# Run from the nexus-tools repo root.
#
# For off-chain tools, this scaffolds at offchain/tools/<crate>/, drops in
# tools.json + build.rs from templates, and gets the crate into the shared
# workspace (offchain/Cargo.toml declares members = ["tools/*"]) so the
# discover workflow (.github/workflows/offchain-tools.discover.yml) picks
# it up on the next push. No per-tool deploy/ dir, no per-tool workflow
# files — deploy is handled by the shared offchain-tools.* pipeline.

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

SKILL_DIR=".claude/skills/nexus-tool-builder"
TEMPLATES_RUST="$SKILL_DIR/templates/rust"

case "$KIND" in
  off-chain)
    DEST="offchain/tools/$NAME"
    if [[ -d "$DEST" ]]; then
      echo "$DEST already exists; refusing to clobber" >&2
      exit 1
    fi
    nexus tool new --name "$NAME" --template rust
    mkdir -p offchain/tools
    mv "$NAME" "$DEST"

    # Drop in tools.json (required by the shared discover workflow).
    # The skill caller may re-render with richer metadata after the fact.
    sed "s/{{crate_name}}/$NAME/g" "$TEMPLATES_RUST/tools.json.tmpl" > "$DEST/tools.json"

    # Drop in build.rs (verbatim — the template is byte-identical to
    # offchain/tools/memory-memwal/build.rs).
    cp "$TEMPLATES_RUST/build.rs.tmpl" "$DEST/build.rs"

    cat >&2 <<EOF
scaffolded $DEST
  - tools.json (RUST_LOG only; secrets via Cloud Run secretKeyRef)
  - build.rs   (validates [[bin]].name == command; threads TOOL_FQN_VERSION)

next steps (the skill caller handles these):
  - render the rust templates over $DEST/src/
  - add [[bin]] and [build-dependencies] to $DEST/Cargo.toml
  - add an .env.example documenting ${NAME//-/_}_API_KEY (or equivalent)
  - run: bash $SKILL_DIR/scripts/verify.sh $NAME
  - audit: invoke the nexus-tool-auditor sub-agent
EOF
    ;;

  on-chain)
    if [[ -d "$NAME" ]]; then
      echo "$NAME already exists; refusing to clobber" >&2
      exit 1
    fi
    nexus tool new --name "${NAME}_onchain" --template move
    echo "scaffolded ${NAME}_onchain" >&2
    ;;

  *)
    echo "unknown kind: $KIND (expected off-chain|on-chain)" >&2
    exit 2
    ;;
esac
