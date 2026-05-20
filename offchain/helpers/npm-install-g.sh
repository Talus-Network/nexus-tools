#!/usr/bin/env bash

set -euo pipefail

# Resolve the directory containing this script so the npm-install-g.txt
# lookup works regardless of the caller's PWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Install NPM stuff from the sibling helpers/npm-install-g.txt
while read -r tool_at_version; do
  # Split the tool name and version
  tool=$(echo "$tool_at_version" | cut -d'@' -f1)

  if ! command -v "$tool" &> /dev/null; then
    echo "Installing tool: $tool_at_version"
    npm install -g "$tool_at_version"
  else
    echo "Tool already installed: $tool_at_version"
  fi
done < "$SCRIPT_DIR/npm-install-g.txt"
