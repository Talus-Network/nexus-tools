# Offchain Tools CI Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Automate build, publish, and on-chain registration of every offchain tool in this repo, modelled on ava-game's three-phase pipeline (build/push → prepare → register) and adapted for one image per tool, with GCS-backed idempotency and a PR-readiness merge gate.

**Architecture:** Repo reorganises so the Rust workspace lives in `offchain/` (leaving room for a future `onchain/`). Each tool ships a `tools.json` + `build.rs` describing it and content-addressed FQN versioning derived from the per-tool subtree hash. Five new GitHub workflows (`discover`, `deploy`, `prepare`, `register`, `readiness`) wired by a top-level `ci.yml`, plus five composite actions. Two long-lived deploy branches: `testnet` and `mainnet`; `main` is dev integration. `promote/*` PRs targeting `testnet`/`mainnet` plus a `workflow_dispatch` with a PR number are the only paths that fire chain ops, keeping wallets safe during review iteration.

**Tech Stack:** Rust (cargo workspace), Docker (shared multi-stage Dockerfile), GitHub Actions (composite + callable workflows), GCP (GCS, Secret Manager, GCR via workload identity), Sui CLI, Nexus CLI, just.

**Spec:** [docs/superpowers/specs/2026-05-19-offchain-tools-pipeline-design.md](../specs/2026-05-19-offchain-tools-pipeline-design.md)

**Branch:** `feat/offchain-tools-pipeline` (already created off `main`).

---

## Conventions

- **Working directory** for all `git`, `cargo`, `docker`, and `just` commands in this plan is the repo root: `/Users/michalturcan/Work/talus/nexus-tools` (or wherever the user's clone is). Where a command must run elsewhere, the step says so.
- **Crate names** (used as `tool_name` in `tools.json`, image name, etc.):

  | Directory                       | Crate name (= tool_name)    |
  |---------------------------------|-----------------------------|
  | `tools/exchanges-coinbase`      | `exchanges-coinbase`        |
  | `tools/http`                    | `http`                      |
  | `tools/llm-openai-chat-completion` | `llm-openai-chat-completion` |
  | `tools/math`                    | `math`                      |
  | `tools/social-twitter`          | `social-twitter`            |
  | `tools/storage-walrus`          | `walrus`                    |
  | `tools/templating-jinja`        | `templating-jinja`          |

  `storage-walrus` is the only directory whose name differs from the crate name. This is intentional — workbench references `nexus-tools/walrus`, and we preserve that contract.
- **Commit style:** match recent history — short imperative subject prefixed with the area (`offchain:`, `ci:`, `tools/<name>:`, `docs:`), followed by a one-line body if needed. Co-author trailer per existing conventions.
- **Cargo invocation:** all `cargo` calls run from `offchain/` after Phase 1 lands. Workflows set `working-directory: offchain` where needed.

---

## Phase 1 — Repo reorganization

### Task 1.1: Move workspace and per-tool sources into `offchain/`

**Files:**
- Move: `Cargo.toml` → `offchain/Cargo.toml`
- Move: `Cargo.lock` → `offchain/Cargo.lock`
- Move: `rust-toolchain.toml` → `offchain/rust-toolchain.toml`
- Move: `rustfmt.toml` → `offchain/rustfmt.toml`
- Move: `deny.toml` → `offchain/deny.toml`
- Move: `tools/` → `offchain/tools/`
- Move: `helpers/` → `offchain/helpers/`

- [ ] **Step 1: Use `git mv` (preserves history) to move the workspace files.**

```bash
mkdir -p offchain
git mv Cargo.toml offchain/Cargo.toml
git mv Cargo.lock offchain/Cargo.lock
git mv rust-toolchain.toml offchain/rust-toolchain.toml
git mv rustfmt.toml offchain/rustfmt.toml
git mv deny.toml offchain/deny.toml
git mv tools offchain/tools
git mv helpers offchain/helpers
```

- [ ] **Step 2: Verify the new workspace builds end-to-end.**

```bash
cd offchain && cargo check --workspace --locked
```

Expected: every crate compiles cleanly. If a path-dependency lookup fails (none of the current tools have inter-tool path deps, so this should not happen), inspect the offending crate's `Cargo.toml` and adjust the relative path by adding one more `..` (because the workspace is now one directory deeper relative to the repo root).

- [ ] **Step 3: Commit.**

```bash
git add -A
git commit -m "offchain: move workspace, tools, and helpers into offchain/

Prepares the repo for a future onchain/ sibling. Files relocated via
git mv to preserve history. Workspace builds verified with cargo check."
```

---

### Task 1.2: Add an `onchain/` placeholder

**Files:**
- Create: `onchain/README.md`

- [ ] **Step 1: Create the directory with a placeholder README.**

```bash
mkdir -p onchain
cat > onchain/README.md <<'EOF'
# onchain/

Reserved for onchain (Move / Sui package) tools and their CI. Empty
for now — see the offchain CI pipeline spec in
[docs/superpowers/specs/2026-05-19-offchain-tools-pipeline-design.md](../docs/superpowers/specs/2026-05-19-offchain-tools-pipeline-design.md)
for context.
EOF
```

- [ ] **Step 2: Commit.**

```bash
git add onchain/README.md
git commit -m "onchain: add placeholder dir for future Move tools"
```

---

### Task 1.3: Update root `justfile` to delegate into `offchain/`

**Files:**
- Modify: `justfile`

The root justfile currently has `mod tools 'tools/.just'`. Update the module paths so they resolve into the new `offchain/` layout. The `mod pre-commit '.pre-commit/.just'` line keeps its current path (the pre-commit hooks live at repo root because they wrap the `git commit` action, not the build).

- [ ] **Step 1: Rewrite the root justfile.**

Replace the contents of `justfile` with:

```just
#
# just
#
# Command runner for project-specific tasks.
# <https://github.com/casey/just>
#

# Commands concerning native Nexus Tools (offchain workspace)
mod tools 'offchain/tools/.just'

# Pre-commit hooks (still at repo root — they wrap git commit)
mod pre-commit '.pre-commit/.just'

# Helpers (lives under the workspace)
mod helpers 'offchain/helpers/helpers.just'

[private]
_default:
    @just --list
```

- [ ] **Step 2: Verify just can still list recipes.**

```bash
just --list
just tools --list
just helpers --list
```

Expected: no errors. The lists may include private/internal recipes — that's fine.

- [ ] **Step 3: Commit.**

```bash
git add justfile
git commit -m "justfile: point modules into offchain/"
```

---

### Task 1.4: Update `.pre-commit/.just` and `offchain/tools/.just` imports

**Files:**
- Modify: `.pre-commit/.just`
- Modify: `offchain/tools/.just`

After the move, `.pre-commit/.just` previously imported `../helpers/helpers.just`. That helper is now at `../offchain/helpers/helpers.just`. The tools justfile previously imported `../helpers/helpers.just` and that still resolves correctly because tools is now under offchain/ and helpers is its sibling.

- [ ] **Step 1: Update `.pre-commit/.just` import.**

In `.pre-commit/.just`, change the first line:

```just
import '../helpers/helpers.just'
```

to:

```just
import '../offchain/helpers/helpers.just'
```

- [ ] **Step 2: Verify offchain/tools/.just imports still resolve.**

```bash
head -1 offchain/tools/.just
```

Expected output: `import '../helpers/helpers.just'`. This stays as-is — it correctly resolves to `offchain/helpers/helpers.just`.

- [ ] **Step 3: Verify recipes still run.**

```bash
just helpers::get-nightly-version
```

Expected output: `nightly-2025-01-06` (or whatever is in `.nightly-version`). If this fails with "file not found", the recipe in `offchain/helpers/helpers.just` is reading `../.nightly-version`. Since `.nightly-version` is still at repo root, and helpers is now at `offchain/helpers/`, that's two levels up. Edit the recipe to use `../../.nightly-version`:

```just
get-nightly-version:
    #!/usr/bin/env bash
    # This file should contain the version of the nightly toolchain to use.
    cat ../../.nightly-version
```

- [ ] **Step 4: Commit.**

```bash
git add .pre-commit/.just offchain/helpers/helpers.just
git commit -m "just: fix import + .nightly-version paths after offchain/ move"
```

---

### Task 1.5: Repoint existing CI workflows to `offchain/` paths

**Files:**
- Modify: `.github/workflows/coverage.yaml`
- Modify: `.github/workflows/coverage-baseline.yml`
- Modify: `.github/workflows/sync_docs.yml`
- Modify: `.github/actions/pre-commit/dependencies/action.yml`
- Modify: `codecov.yml`

`audit.yml` already uses `**/Cargo.toml` globs and matches in either location, so no change is needed there. `pre-commit_(PR).yml` and `pre-commit_(main).yml` invoke `./.pre-commit/pre-commit` which delegates via just; the just delegations are already fixed in Task 1.4.

- [ ] **Step 1: Update `.github/workflows/coverage.yaml` path filter.**

In `.github/workflows/coverage.yaml`, replace:

```yaml
      files: |
        tools/**
```

with:

```yaml
      files: |
        offchain/tools/**
        offchain/Cargo.toml
        offchain/Cargo.lock
```

- [ ] **Step 2: Same change to `.github/workflows/coverage-baseline.yml`.**

Replace the identical `tools/**` block in that file with the same three-line replacement.

- [ ] **Step 3: Update `.github/workflows/sync_docs.yml`.**

Find every occurrence of `tools/` (path filter and the README-copy loop) and prepend `offchain/`. The shell snippet should read:

```bash
for readme in offchain/tools/*/README.md; do
    tool_name=$(basename "$(dirname "$readme")")
    mkdir -p "gitbook-docs/tools/$tool_name"
    cp "$readme" "gitbook-docs/tools/$tool_name/"
done
```

And the path filter:

```yaml
      files: |
        offchain/tools/*/README.md
```

- [ ] **Step 4: Update `.github/actions/pre-commit/dependencies/action.yml`.**

The last step references `./helpers/npm-install-g.sh`. Change to:

```yaml
    - name: Install NPM stuff from helpers/npm-install-g.txt
      shell: bash
      run: ./offchain/helpers/npm-install-g.sh
```

- [ ] **Step 5: Update `codecov.yml`.**

Replace every occurrence of `tools/` (component path, flag path) with `offchain/tools/`:

```yaml
component_management:
  individual_components:
    - component_id: tools
      name: Tools
      paths:
        - offchain/tools/**

# ... and the flags section:

flags:
  unittests:
    paths:
      - offchain/tools/
    carryforward: true
```

- [ ] **Step 6: Verify YAML syntax for every changed workflow.**

```bash
for f in .github/workflows/coverage.yaml .github/workflows/coverage-baseline.yml .github/workflows/sync_docs.yml .github/actions/pre-commit/dependencies/action.yml; do
  python -c "import yaml; yaml.safe_load(open('$f'))" && echo "OK: $f"
done
```

Expected: `OK: ...` for each file.

- [ ] **Step 7: Commit.**

```bash
git add .github/ codecov.yml
git commit -m "ci: repoint coverage, sync-docs, pre-commit deps to offchain/ paths"
```

---

### Task 1.6: Smoke-test pre-commit hooks against the new layout

**Files:** none modified — verification only.

- [ ] **Step 1: Run cargo-check via the pre-commit recipe.**

```bash
just pre-commit::cargo-check
```

Expected: a `cargo check --locked --workspace --bins --examples` runs successfully against `offchain/`. If `just` complains that the recipe can't find `cargo`, ensure you're in the repo root. If it complains about a missing Cargo.toml, the just recipe is invoking cargo from the wrong dir — the recipe in `.pre-commit/.just` needs an explicit `cd offchain` prefix. Edit it:

```just
cargo-check: _check-cargo
    cd ../offchain && cargo check --locked --workspace --bins --examples
```

(The `../offchain` is relative because the recipe runs with PWD set to the `.pre-commit/` directory.) Adjust similarly for any other cargo-invoking recipes in `.pre-commit/.just` if they fail.

- [ ] **Step 2: Run clippy via the pre-commit recipe.**

```bash
just pre-commit::cargo-clippy
```

Expected: clippy runs clean against the workspace.

- [ ] **Step 3: If any recipe needed editing in step 1 or 2, commit.**

```bash
git add .pre-commit/.just
git commit -m "pre-commit: cd into offchain/ before cargo invocations"
```

If nothing changed, skip the commit.

---

## Phase 2 — Per-tool conventions (`tools.json`, `build.rs`, `[[bin]]`)

Each of the 7 tool crates gets the same three additions, with the only per-tool variation being values in `tools.json` and a `[[bin]]` declaration matching the crate name.

### Task 2.1: Add `tools.json` + `build.rs` to `offchain/tools/math` (canonical example)

**Files:**
- Create: `offchain/tools/math/tools.json`
- Create: `offchain/tools/math/build.rs`
- Modify: `offchain/tools/math/Cargo.toml`

This is the smallest tool (used as the canonical example). The remaining six tools follow the same pattern.

- [ ] **Step 1: Write the `tools.json`.**

Create `offchain/tools/math/tools.json` with exact contents:

```json
{
  "tool_name": "math",
  "command": "math",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

- [ ] **Step 2: Write the `build.rs`.**

Create `offchain/tools/math/build.rs` with exact contents:

```rust
use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let tools_json_path = manifest_dir.join("tools.json");
    let cargo_toml_path = manifest_dir.join("Cargo.toml");

    println!("cargo::rerun-if-changed={}", tools_json_path.display());
    println!("cargo::rerun-if-changed={}", cargo_toml_path.display());

    let tools_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&tools_json_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", tools_json_path.display())),
    )
    .expect("tools.json must be valid JSON");

    let command = tools_json["command"]
        .as_str()
        .expect("tools.json must have command");

    let cargo_toml: toml::Value = toml::from_str(
        &fs::read_to_string(&cargo_toml_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", cargo_toml_path.display())),
    )
    .expect("Cargo.toml must be valid TOML");

    let bin_name = cargo_toml
        .get("bin")
        .and_then(|b| b.as_array())
        .and_then(|bins| bins.first())
        .and_then(|b| b.get("name"))
        .and_then(|n| n.as_str());

    match bin_name {
        Some(name) if name == command => {}
        Some(name) => panic!(
            "Binary name mismatch: Cargo.toml [[bin]] name = \"{name}\" \
             but tools.json command = \"{command}\". They must match."
        ),
        None => panic!(
            "Cargo.toml has no [[bin]] section but tools.json specifies \
             command = \"{command}\". Add: [[bin]]\nname = \"{command}\""
        ),
    }

    // FQN version is set by CI via Docker build-arg; defaults to "1" locally.
    let version = env::var("TOOL_FQN_VERSION").unwrap_or_else(|_| "1".to_string());
    println!("cargo:rustc-env=TOOL_FQN_VERSION={version}");
    println!("cargo:rerun-if-env-changed=TOOL_FQN_VERSION");
}
```

- [ ] **Step 3: Update `offchain/tools/math/Cargo.toml`.**

After the existing `[package]` block (before `[dependencies]`), add a `[[bin]]` entry. The path defaults to `src/main.rs`. Then add `[build-dependencies]`.

Open `offchain/tools/math/Cargo.toml`, locate the line immediately above `[dependencies]`, and insert:

```toml
[[bin]]
name = "math"
path = "src/main.rs"
```

At the end of the file (after `[dev-dependencies]` if any), add:

```toml
[build-dependencies]
serde_json.workspace = true
toml = "0.8"
```

- [ ] **Step 4: Verify the build still succeeds.**

```bash
cd offchain && cargo build -p math --release
```

Expected: build completes. The compiled binary embeds `TOOL_FQN_VERSION=1`. If `build.rs` panics with "Binary name mismatch" or "no [[bin]] section", recheck Step 3 — the `[[bin]] name = "math"` must exactly match `tools.json`'s `command`.

- [ ] **Step 5: Deliberately break the contract to verify the guard.**

Temporarily edit `offchain/tools/math/tools.json` and change `"command": "math"` to `"command": "foo"`. Run:

```bash
cd offchain && cargo build -p math --release 2>&1 | grep "Binary name mismatch"
```

Expected: the build fails with `Binary name mismatch: Cargo.toml [[bin]] name = "math" but tools.json command = "foo"`. Restore `tools.json` to `"command": "math"` and verify `cargo build -p math --release` passes again.

- [ ] **Step 6: Commit.**

```bash
git add offchain/tools/math/{tools.json,build.rs,Cargo.toml}
git commit -m "tools/math: add tools.json, build.rs, [[bin]] declaration

Establishes the per-tool convention used by the offchain CI pipeline.
build.rs validates the bin/tool name match at compile time and injects
TOOL_FQN_VERSION from env (default \"1\" for local builds)."
```

---

### Task 2.2: Apply the same pattern to the remaining six tools

For each of the following six tools, follow the exact steps from Task 2.1 (write `tools.json`, write `build.rs`, add `[[bin]]` and `[build-dependencies]` to `Cargo.toml`, verify build, commit). The `build.rs` file is byte-identical across all tools — copy it verbatim from `offchain/tools/math/build.rs`.

The `tools.json` per tool:

#### `offchain/tools/exchanges-coinbase/tools.json`

```json
{
  "tool_name": "exchanges-coinbase",
  "command": "exchanges-coinbase",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

`[[bin]]` block in Cargo.toml:

```toml
[[bin]]
name = "exchanges-coinbase"
path = "src/main.rs"
```

#### `offchain/tools/http/tools.json`

```json
{
  "tool_name": "http",
  "command": "http",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

```toml
[[bin]]
name = "http"
path = "src/main.rs"
```

#### `offchain/tools/llm-openai-chat-completion/tools.json`

```json
{
  "tool_name": "llm-openai-chat-completion",
  "command": "llm-openai-chat-completion",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

```toml
[[bin]]
name = "llm-openai-chat-completion"
path = "src/main.rs"
```

#### `offchain/tools/social-twitter/tools.json`

```json
{
  "tool_name": "social-twitter",
  "command": "social-twitter",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

```toml
[[bin]]
name = "social-twitter"
path = "src/main.rs"
```

#### `offchain/tools/storage-walrus/tools.json`

```json
{
  "tool_name": "walrus",
  "command": "walrus",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

```toml
[[bin]]
name = "walrus"
path = "src/main.rs"
```

Note: `tool_name` and `command` are `walrus` (the crate name), **not** `storage-walrus` (the directory name). This is deliberate — see the spec's "Naming chain" section.

#### `offchain/tools/templating-jinja/tools.json`

```json
{
  "tool_name": "templating-jinja",
  "command": "templating-jinja",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

```toml
[[bin]]
name = "templating-jinja"
path = "src/main.rs"
```

For each tool:

- [ ] **Step 1: Write `tools.json`** with the exact contents above.
- [ ] **Step 2: Copy `build.rs` from math.**

```bash
cp offchain/tools/math/build.rs offchain/tools/<tool>/build.rs
```

- [ ] **Step 3: Add `[[bin]]` and `[build-dependencies]` to `Cargo.toml`** (same two blocks as Task 2.1 Step 3, but with the per-tool `name` value above).
- [ ] **Step 4: Verify the build.**

```bash
cd offchain && cargo build -p <tool> --release
```

(Use the crate name from the table at the top of this plan.)

- [ ] **Step 5: Commit.**

```bash
git add offchain/tools/<tool>/{tools.json,build.rs,Cargo.toml}
git commit -m "tools/<tool>: add tools.json, build.rs, [[bin]] declaration"
```

Repeat for all six tools. After all are done, run a final workspace check:

- [ ] **Step 6: Workspace-wide check.**

```bash
cd offchain && cargo check --workspace --locked
```

Expected: every crate compiles. If a tool fails its build.rs guard, recheck that tool's `[[bin]] name` matches its `tools.json` `command`.

---

## Phase 3 — Shared Dockerfile

### Task 3.1: Write the shared multi-stage Dockerfile

**Files:**
- Create: `offchain/Dockerfile`

The Dockerfile takes `PACKAGE`, `BINARY`, and `TOOL_FQN_VERSION` build-args. Stages: `builder` (Rust slim, runs `cargo build`), `lib-collector` (extracts runtime libs), and `distroless` (final image).

- [ ] **Step 1: Create `offchain/Dockerfile`.**

```dockerfile
# syntax=docker/dockerfile:1
# Shared Dockerfile for all offchain Nexus tools.
#
# Builds a single tool binary identified by PACKAGE and BINARY build args,
# embeds TOOL_FQN_VERSION (computed by CI from the tool's subtree hash)
# so the tool's FQN strings carry the content-addressed version.

FROM rust:1.93.1-slim-bookworm AS builder

ARG PACKAGE
ARG BINARY
ARG TOOL_FQN_VERSION=1

# Surfaced as a cargo env var via build.rs -> env!("TOOL_FQN_VERSION").
ENV TOOL_FQN_VERSION=${TOOL_FQN_VERSION}

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the entire offchain workspace. Cargo's incremental build + the
# buildkit cache mounts keep this fast.
COPY offchain/ ./offchain/

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/offchain/target,sharing=locked \
    set -e && \
    cd offchain && \
    cargo build --locked --profile release --bin "${BINARY}" -p "${PACKAGE}" && \
    cp "target/release/${BINARY}" "/app/${BINARY}"

# ── Collect runtime libs for the distroless stage ─────────────────────
FROM debian:bookworm-slim AS lib-collector

ARG BINARY
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/${BINARY} /tmp/binary

RUN set -e; \
    case "${TARGETARCH}" in \
        "amd64") LIB_DIR="/lib/x86_64-linux-gnu"; LINKER="/lib64/ld-linux-x86-64.so.2" ;; \
        "arm64") LIB_DIR="/lib/aarch64-linux-gnu"; LINKER="/lib/ld-linux-aarch64.so.1" ;; \
        *) echo "Unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    mkdir -p "/collected${LIB_DIR}" /collected/lib64 && \
    ldd /tmp/binary | awk '/=>/ { print $3 }' | while read -r lib; do \
        if [ -n "$lib" ] && [ -f "$lib" ]; then \
            cp -L "$lib" "/collected${LIB_DIR}/"; \
        fi \
    done && \
    cp -L "${LINKER}" "/collected${LINKER}"

# ── Final distroless image ───────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

ARG BINARY

COPY --from=builder /app/${BINARY} /usr/local/bin/${BINARY}
COPY --from=lib-collector /collected/ /

ENV PORT=8080
EXPOSE 8080
```

- [ ] **Step 2: Build math locally as a smoke test.**

```bash
docker build \
  --file offchain/Dockerfile \
  --build-arg PACKAGE=math \
  --build-arg BINARY=math \
  --build-arg TOOL_FQN_VERSION=42 \
  --tag nexus-tools/math:local-test \
  .
```

Expected: image builds successfully. The build takes a few minutes the first time (no cache). On subsequent runs, cargo cache mounts make it much faster.

- [ ] **Step 3: Verify the embedded version.**

```bash
docker run --rm --entrypoint /usr/local/bin/math nexus-tools/math:local-test --help 2>&1 | head -5 || true
# (we just want to confirm the binary runs; --help may exit non-zero — that's fine)
```

Expected: the binary at least invokes (may print help or an error). The image has `TOOL_FQN_VERSION=42` baked in via build.rs.

- [ ] **Step 4: Commit.**

```bash
git add offchain/Dockerfile
git commit -m "offchain: add shared multi-stage Dockerfile

Build args PACKAGE, BINARY, TOOL_FQN_VERSION. Three stages: rust slim
builder with buildkit cache mounts, debian lib-collector via ldd, and a
distroless cc-debian12 final image. Smoke-tested locally with math."
```

---

## Phase 4 — Composite GitHub Actions

These five composite actions are reusable steps lifted (and adapted) from ava-game. Each lives at `.github/actions/<name>/action.yml`.

### Task 4.1: `install-sui` composite action

**Files:**
- Create: `.github/actions/install-sui/action.yml`

- [ ] **Step 1: Create the action file.**

```yaml
name: Install Sui CLI
description: Install the Sui CLI from suiup, with cache support
inputs:
  sui-channel:
    description: 'Sui release channel (e.g. testnet, mainnet)'
    required: true
  cache-version:
    description: 'Bump to invalidate the cache'
    required: false
    default: 'v1'
runs:
  using: composite
  steps:
    - name: Cache Sui binary
      id: cache
      uses: actions/cache@v4
      with:
        path: ~/.local/bin/sui
        key: sui-${{ runner.os }}-${{ inputs.sui-channel }}-${{ inputs.cache-version }}

    - name: Install suiup and Sui CLI
      if: steps.cache.outputs.cache-hit != 'true'
      shell: bash
      env:
        GITHUB_TOKEN: ${{ github.token }}
        SUI_CHANNEL: ${{ inputs.sui-channel }}
      run: |
        set -euo pipefail
        install_dir="$HOME/.local/bin"
        temp_dir="$(mktemp -d)"
        mkdir -p "$install_dir"
        curl --retry 5 --retry-all-errors -sSfL \
          -o "$temp_dir/suiup.tar.gz" \
          https://github.com/MystenLabs/suiup/releases/latest/download/suiup-Linux-musl-x86_64.tar.gz
        tar -xzf "$temp_dir/suiup.tar.gz" -C "$temp_dir"
        install -m 0755 "$temp_dir/suiup" "$install_dir/suiup"
        "$install_dir/suiup" install "sui@$SUI_CHANNEL" -y

    - name: Add Sui to PATH
      shell: bash
      run: echo "$HOME/.local/bin" >> "$GITHUB_PATH"

    - name: Verify
      shell: bash
      run: sui --version
```

- [ ] **Step 2: Validate YAML.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/actions/install-sui/action.yml'))" && echo OK
```

Expected: `OK`.

- [ ] **Step 3: Commit.**

```bash
git add .github/actions/install-sui/action.yml
git commit -m "ci/actions: add install-sui composite (suiup + cache)"
```

---

### Task 4.2: `install-nexus-cli` composite action

**Files:**
- Create: `.github/actions/install-nexus-cli/action.yml`

- [ ] **Step 1: Create the action file.**

```yaml
name: Install Nexus CLI
description: Extract the nexus CLI from the nexus-sdk shell image
inputs:
  nexus-tag:
    description: 'Tag of gcr.io/.../nexus-sdk/shell to extract from'
    required: true
  infra-registry:
    description: 'Infra GCR registry prefix (e.g. gcr.io/production-tf-talus-infra)'
    required: true
runs:
  using: composite
  steps:
    - name: Extract nexus binary
      shell: bash
      env:
        NEXUS_TAG: ${{ inputs.nexus-tag }}
        INFRA_REGISTRY: ${{ inputs.infra-registry }}
      run: |
        set -euo pipefail
        mkdir -p "$HOME/.local/bin"
        CONTAINER_ID=$(docker create "${INFRA_REGISTRY}/nexus-sdk/shell:${NEXUS_TAG}")
        docker cp "${CONTAINER_ID}:/usr/local/bin/nexus" "$HOME/.local/bin/nexus"
        docker rm "${CONTAINER_ID}"
        chmod +x "$HOME/.local/bin/nexus"
        echo "$HOME/.local/bin" >> "$GITHUB_PATH"

    - name: Verify
      shell: bash
      run: nexus --version
```

- [ ] **Step 2: Validate YAML.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/actions/install-nexus-cli/action.yml'))" && echo OK
```

- [ ] **Step 3: Commit.**

```bash
git add .github/actions/install-nexus-cli/action.yml
git commit -m "ci/actions: add install-nexus-cli composite"
```

---

### Task 4.3: `gcp-auth-protocol` composite action

**Files:**
- Create: `.github/actions/gcp-auth-protocol/action.yml`

Authenticates to the protocol GCP project (owns the GCS bucket and Secret Manager). Mirrors ava-game's separation so token lifetimes don't collide with the infra-project token.

- [ ] **Step 1: Create the action file.**

```yaml
name: Authenticate to GCP (protocol project)
description: Authenticate via workload identity to the GCP project that owns GCS + Secret Manager
inputs:
  project-id:
    description: 'GCP project ID'
    required: true
  project-number:
    description: 'GCP project number'
    required: true
  provider-name:
    description: 'Workload identity pool provider name (e.g. nexus-tools-protocol)'
    required: true
outputs:
  credentials_file_path:
    description: 'Path to the generated credentials file'
    value: ${{ steps.auth.outputs.credentials_file_path }}
runs:
  using: composite
  steps:
    - name: Authenticate
      id: auth
      uses: google-github-actions/auth@v2
      with:
        project_id: ${{ inputs.project-id }}
        workload_identity_provider: "projects/${{ inputs.project-number }}/locations/global/workloadIdentityPools/${{ inputs.project-id }}/providers/${{ inputs.provider-name }}"

    - name: Setup Python (gcloud needs it)
      uses: actions/setup-python@v5
      with:
        python-version: '3.12'

    - name: Setup Cloud SDK
      uses: google-github-actions/setup-gcloud@v2
```

- [ ] **Step 2: Validate YAML and commit.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/actions/gcp-auth-protocol/action.yml'))" && echo OK
git add .github/actions/gcp-auth-protocol/action.yml
git commit -m "ci/actions: add gcp-auth-protocol composite"
```

---

### Task 4.4: `gcp-auth-infra` composite action

**Files:**
- Create: `.github/actions/gcp-auth-infra/action.yml`

Authenticates to the infra GCP project (owns the GCR registry and the nexus-sdk shell image). Returns the access token so subsequent docker login steps can use it.

- [ ] **Step 1: Create the action file.**

```yaml
name: Authenticate to GCP (infra project)
description: Authenticate via workload identity to the GCP project that owns GCR + shared images
inputs:
  project-id:
    description: 'GCP infra project ID'
    required: true
  project-number:
    description: 'GCP infra project number'
    required: true
  provider-name:
    description: 'Workload identity pool provider name (e.g. nexus-tools)'
    required: true
outputs:
  auth_token:
    description: 'OAuth2 access token for GCR login'
    value: ${{ steps.auth.outputs.auth_token }}
runs:
  using: composite
  steps:
    - name: Authenticate
      id: auth
      uses: google-github-actions/auth@v2
      with:
        project_id: ${{ inputs.project-id }}
        workload_identity_provider: "projects/${{ inputs.project-number }}/locations/global/workloadIdentityPools/${{ inputs.project-id }}/providers/${{ inputs.provider-name }}"
        create_credentials_file: false
        token_format: 'access_token'

    - name: Login to GCR
      uses: docker/login-action@v3
      with:
        registry: gcr.io
        username: oauth2accesstoken
        password: ${{ steps.auth.outputs.auth_token }}
```

- [ ] **Step 2: Validate YAML and commit.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/actions/gcp-auth-infra/action.yml'))" && echo OK
git add .github/actions/gcp-auth-infra/action.yml
git commit -m "ci/actions: add gcp-auth-infra composite"
```

---

### Task 4.5: `retrigger-pr` composite action

**Files:**
- Create: `.github/actions/retrigger-pr/action.yml`

Imports a GPG signing key and pushes an empty signed commit to the PR branch so its check suites re-run. Used at the end of `workflow_dispatch` runs to flip the readiness gate green.

- [ ] **Step 1: Create the action file.**

```yaml
name: Retrigger PR checks
description: Push an empty signed commit to a PR branch so its checks re-run
inputs:
  pr-number:
    description: 'PR number to retrigger'
    required: true
  gpg-signing-key:
    description: 'ASCII-armoured GPG private key for signing the empty commit'
    required: true
  github-token:
    description: 'GitHub token with PR contents write permission'
    required: true
runs:
  using: composite
  steps:
    - name: Resolve PR head branch
      id: pr
      shell: bash
      env:
        GH_TOKEN: ${{ inputs.github-token }}
        PR_NUMBER: ${{ inputs.pr-number }}
      run: |
        set -euo pipefail
        HEAD=$(gh api "repos/${{ github.repository }}/pulls/${PR_NUMBER}" --jq '.head.ref')
        echo "head-ref=$HEAD" >> "$GITHUB_OUTPUT"

    - name: Checkout PR branch
      uses: actions/checkout@v4
      with:
        ref: ${{ steps.pr.outputs.head-ref }}
        token: ${{ inputs.github-token }}
        fetch-depth: 0

    - name: Import GPG key
      shell: bash
      env:
        GPG_KEY: ${{ inputs.gpg-signing-key }}
      run: |
        set -euo pipefail
        echo "$GPG_KEY" | gpg --batch --import
        KEYID=$(gpg --list-secret-keys --with-colons | awk -F: '/^sec:/ {print $5; exit}')
        echo "GPG_KEYID=$KEYID" >> "$GITHUB_ENV"
        git config --global user.signingkey "$KEYID"
        git config --global commit.gpgsign true
        git config --global user.email "devops@taluslabs.xyz"
        git config --global user.name "Talus DevOps"

    - name: Push empty signed commit
      shell: bash
      env:
        PR_NUMBER: ${{ inputs.pr-number }}
      run: |
        set -euo pipefail
        git commit --allow-empty -S \
          -m "ci: retrigger PR #${PR_NUMBER} checks" \
          -m "A workflow_dispatch run completed; pushing an empty commit so PR checks (readiness, ci-gate) re-evaluate against the freshly published artifacts."
        git push origin "HEAD:${{ steps.pr.outputs.head-ref }}"
```

- [ ] **Step 2: Validate YAML and commit.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/actions/retrigger-pr/action.yml'))" && echo OK
git add .github/actions/retrigger-pr/action.yml
git commit -m "ci/actions: add retrigger-pr composite (signed empty commit)"
```

---

## Phase 5 — Callable workflows

The five core callable workflows. Each is invoked from the top-level `ci.yml` (Phase 6) and accepts `target-ref` and (where relevant) `matrix-json` or `dry-run`.

### Task 5.1: `offchain-tools.discover.yml`

**Files:**
- Create: `.github/workflows/offchain-tools.discover.yml`

Globs `offchain/tools/*/tools.json`, computes per-tool content versions, and emits two matrices (`all` and `changed`).

- [ ] **Step 1: Create the workflow.**

```yaml
name: Offchain Tools Discover

on:
  workflow_call:
    inputs:
      base-ref:
        description: 'Base ref to compute changed-files diff against'
        required: false
        type: string
        default: ''
    outputs:
      matrix-all:
        description: 'JSON matrix of every discovered tool'
        value: ${{ jobs.discover.outputs.matrix-all }}
      matrix-changed:
        description: 'JSON matrix of tools whose subtree changed since base-ref'
        value: ${{ jobs.discover.outputs.matrix-changed }}
      content-hash:
        description: 'Aggregate hash of all tools.json + their subtree versions'
        value: ${{ jobs.discover.outputs.content-hash }}

jobs:
  discover:
    name: Discover offchain tools
    runs-on: ubuntu-latest
    outputs:
      matrix-all: ${{ steps.build.outputs.matrix-all }}
      matrix-changed: ${{ steps.build.outputs.matrix-changed }}
      content-hash: ${{ steps.build.outputs.content-hash }}
    steps:
      - name: Checkout
        uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Detect changed files
        id: changes
        if: inputs.base-ref != ''
        uses: tj-actions/changed-files@v46
        with:
          base_sha: ${{ inputs.base-ref }}
          files: |
            offchain/tools/**
            offchain/Cargo.toml
            offchain/Cargo.lock
            offchain/Dockerfile
            offchain/rust-toolchain.toml

      - name: Build matrices
        id: build
        shell: bash
        env:
          CHANGED_FILES: ${{ steps.changes.outputs.all_changed_files }}
        run: |
          set -euo pipefail
          ALL='[]'
          CHANGED='[]'
          HASH_INPUT=""

          # Shared files force every tool into the "changed" set.
          SHARED_TOUCHED=false
          if [ -n "${CHANGED_FILES:-}" ]; then
            for f in $CHANGED_FILES; do
              case "$f" in
                offchain/Cargo.toml|offchain/Cargo.lock|offchain/Dockerfile|offchain/rust-toolchain.toml)
                  SHARED_TOUCHED=true
                  ;;
              esac
            done
          fi
          echo "Shared files touched: $SHARED_TOUCHED"

          for TOOLS_JSON in offchain/tools/*/tools.json; do
            [ -f "$TOOLS_JSON" ] || continue
            DIR=$(dirname "$TOOLS_JSON")
            TOOL_NAME=$(jq -r '.tool_name' "$TOOLS_JSON")
            COMMAND=$(jq -r '.command' "$TOOLS_JSON")

            TREE_HASH=$(git rev-parse "HEAD:${DIR}/")
            CONTENT_HASH=$(printf '%s' "$TREE_HASH" | sha256sum | cut -d' ' -f1)
            VERSION=$(printf '%d' "0x${CONTENT_HASH:0:8}")

            ENTRY=$(jq -nc \
              --arg tool "$TOOL_NAME" \
              --arg dir "$DIR" \
              --arg cmd "$COMMAND" \
              --arg ver "$VERSION" \
              '{tool: $tool, dir: $dir, command: $cmd, version: $ver}')

            ALL=$(echo "$ALL" | jq -c ". + [${ENTRY}]")

            # Is this tool changed?
            TOOL_CHANGED=false
            if [ "$SHARED_TOUCHED" = "true" ] || [ -z "${CHANGED_FILES:-}" ]; then
              TOOL_CHANGED=true
            else
              for f in $CHANGED_FILES; do
                case "$f" in
                  ${DIR}/*) TOOL_CHANGED=true ;;
                esac
              done
            fi
            if [ "$TOOL_CHANGED" = "true" ]; then
              CHANGED=$(echo "$CHANGED" | jq -c ". + [${ENTRY}]")
            fi

            HASH_INPUT="${HASH_INPUT}${TOOL_NAME}:${VERSION}\n"
            echo "  - ${TOOL_NAME} v${VERSION} (changed=${TOOL_CHANGED})"
          done

          CONTENT_HASH=$(printf '%s' "$HASH_INPUT" | sha256sum | cut -c1-16)
          echo "matrix-all={\"include\":${ALL}}" >> "$GITHUB_OUTPUT"
          echo "matrix-changed={\"include\":${CHANGED}}" >> "$GITHUB_OUTPUT"
          echo "content-hash=${CONTENT_HASH}" >> "$GITHUB_OUTPUT"

          echo "::group::matrix-all"
          echo "$ALL" | jq .
          echo "::endgroup::"
          echo "::group::matrix-changed"
          echo "$CHANGED" | jq .
          echo "::endgroup::"
```

- [ ] **Step 2: Validate YAML and commit.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/offchain-tools.discover.yml'))" && echo OK
git add .github/workflows/offchain-tools.discover.yml
git commit -m "ci: add offchain-tools.discover callable workflow

Globs offchain/tools/*/tools.json, computes per-tool content versions
(sha256(git rev-parse HEAD:<dir>/) first 8 hex chars as u32), and emits
two matrices: all tools and tools changed since base-ref. A change to a
shared file (Cargo.toml/Lock, Dockerfile, rust-toolchain) treats every
tool as changed."
```

---

### Task 5.2: `offchain-tools.deploy.yml`

**Files:**
- Create: `.github/workflows/offchain-tools.deploy.yml`

Per-tool build job (matrix). Pushes images to GHCR and GCR when `dry-run=false`.

- [ ] **Step 1: Create the workflow.**

```yaml
name: Offchain Tools Deploy

on:
  workflow_call:
    inputs:
      target-ref:
        required: false
        type: string
      matrix-json:
        required: true
        type: string
      dry-run:
        required: false
        type: boolean
        default: false

permissions:
  contents: read
  packages: write
  id-token: write

jobs:
  build:
    name: "Build ${{ matrix.tool }} v${{ matrix.version }}"
    runs-on: ubuntu-latest
    environment: >-
      ${{
        (inputs.target-ref || github.event_name == 'pull_request' && github.base_ref || github.ref_name) == 'mainnet' && 'mainnet' ||
        'testnet'
      }}
    strategy:
      matrix: ${{ fromJson(inputs.matrix-json) }}
      fail-fast: false
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Compute short SHA
        id: sha
        run: echo "short=${GITHUB_SHA::7}" >> "$GITHUB_OUTPUT"

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Login to GHCR
        if: ${{ !inputs.dry-run }}
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Authenticate to GCP (infra)
        if: ${{ !inputs.dry-run }}
        id: auth-infra
        uses: ./.github/actions/gcp-auth-infra
        with:
          project-id: ${{ vars.GCP_INFRA_PROJECT_ID }}
          project-number: ${{ vars.GCP_INFRA_PROJECT_NUMBER }}
          provider-name: nexus-tools

      - name: Compute image tags
        id: tags
        run: |
          set -euo pipefail
          SHA=${{ steps.sha.outputs.short }}
          GHCR="ghcr.io/${{ github.repository }}/${{ matrix.tool }}:sha-${SHA}"
          # Note: repository path is lowercased by GHCR automatically.
          echo "ghcr=${GHCR,,}" >> "$GITHUB_OUTPUT"
          if [ "${{ inputs.dry-run }}" = "false" ]; then
            GCR="gcr.io/${{ vars.GCP_INFRA_PROJECT_ID }}/nexus-tools/${{ matrix.tool }}:sha-${SHA}"
            echo "gcr=${GCR}" >> "$GITHUB_OUTPUT"
          fi

      - name: Build and push
        uses: docker/build-push-action@v6
        with:
          context: .
          file: ./offchain/Dockerfile
          platforms: linux/amd64
          push: ${{ !inputs.dry-run }}
          build-args: |
            PACKAGE=${{ matrix.tool }}
            BINARY=${{ matrix.command }}
            TOOL_FQN_VERSION=${{ matrix.version }}
          tags: |
            ${{ steps.tags.outputs.ghcr }}
            ${{ steps.tags.outputs.gcr }}
          cache-from: type=gha,scope=offchain-${{ matrix.tool }}
          cache-to: type=gha,mode=max,scope=offchain-${{ matrix.tool }}
```

- [ ] **Step 2: Validate YAML.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/offchain-tools.deploy.yml'))" && echo OK
```

- [ ] **Step 3: Commit.**

```bash
git add .github/workflows/offchain-tools.deploy.yml
git commit -m "ci: add offchain-tools.deploy callable workflow

Per-tool matrix build of offchain/Dockerfile. Pushes to GHCR
(ghcr.io/<repo>/<tool>:sha-<7>) and GCR (gcr.io/<infra>/nexus-tools/<tool>:sha-<7>)
when dry-run=false; only builds (no push) when dry-run=true."
```

---

### Task 5.3: `offchain-tools.prepare.yml`

**Files:**
- Create: `.github/workflows/offchain-tools.prepare.yml`

Per-tool prepare matrix: pulls image, runs `--meta`, generates signed-HTTP keys, writes Cloud Run config to GCS. Aggregator job collects per-tool artifacts into a single `offchain-tools-prepare` GitHub artifact for the register workflow.

- [ ] **Step 1: Create the workflow.**

```yaml
name: Offchain Tools Prepare

on:
  workflow_call:
    inputs:
      target-ref:
        required: false
        type: string
      matrix-json:
        required: true
        type: string

permissions:
  contents: read
  packages: read
  id-token: write

jobs:
  prepare:
    name: "Prepare ${{ matrix.tool }} v${{ matrix.version }}"
    runs-on: ubuntu-latest
    environment: >-
      ${{
        (inputs.target-ref || github.event_name == 'pull_request' && github.base_ref || github.ref_name) == 'mainnet' && 'mainnet' ||
        'testnet'
      }}
    strategy:
      matrix: ${{ fromJson(inputs.matrix-json) }}
      fail-fast: false
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Compute short SHA + versioned name
        id: names
        run: |
          SHA=${GITHUB_SHA::7}
          echo "sha=${SHA}" >> "$GITHUB_OUTPUT"
          echo "versioned=${{ matrix.tool }}-v${{ matrix.version }}" >> "$GITHUB_OUTPUT"

      - name: Login to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Pull image
        run: |
          IMAGE="ghcr.io/${{ github.repository }}/${{ matrix.tool }}:sha-${{ steps.names.outputs.sha }}"
          IMAGE="${IMAGE,,}"
          docker pull "$IMAGE"
          echo "IMAGE=$IMAGE" >> "$GITHUB_ENV"

      - name: Extract --meta
        run: |
          set -euo pipefail
          mkdir -p "${RUNNER_TEMP}/prepare/meta"
          docker run --rm --entrypoint /usr/local/bin/${{ matrix.command }} \
            "$IMAGE" --meta \
            > "${RUNNER_TEMP}/prepare/meta/${{ matrix.tool }}.json"
          echo "::group::Meta for ${{ matrix.tool }}"
          jq '.[].fqn' "${RUNNER_TEMP}/prepare/meta/${{ matrix.tool }}.json"
          echo "::endgroup::"

      - name: Render Cloud Run tool config
        id: render
        run: |
          set -euo pipefail
          DIR="${RUNNER_TEMP}/prepare/gcs/tools"
          mkdir -p "$DIR"
          VERSIONED="${{ steps.names.outputs.versioned }}"
          # matrix.dir is the directory path (e.g. offchain/tools/storage-walrus).
          TOOLS_JSON="${{ matrix.dir }}/tools.json"
          EXTRA_ENV=$(jq -c '.environment // {}' "$TOOLS_JSON")
          RESOURCES=$(jq -c '.resources // {}' "$TOOLS_JSON")
          jq -n \
            --arg name "$VERSIONED" \
            --arg network "${{ vars.SUI_NETWORK }}" \
            --arg image "nexus-tools/${{ matrix.tool }}" \
            --arg tag "sha-${{ steps.names.outputs.sha }}" \
            --arg cmd "${{ matrix.command }}" \
            --argjson extra_env "$EXTRA_ENV" \
            --argjson resources "$RESOURCES" \
            '{
              name: $name,
              network: $network,
              image: $image,
              tag: $tag,
              replicas: 1,
              environment: ({
                RUST_LOG: "info",
                BIND_ADDR: "0.0.0.0:8080",
                NEXUS_TOOLKIT_CONFIG_PATH: "/app/secrets/toolkit-config.json"
              } + $extra_env),
              command: $cmd,
              ports: [{containerPort: 8080}],
              signed_http: {enabled: true},
              resources: $resources
            }' > "$DIR/${VERSIONED}.json"
          echo "config=$DIR/${VERSIONED}.json" >> "$GITHUB_OUTPUT"

      - name: Authenticate to GCP (protocol)
        id: auth-gcs
        uses: ./.github/actions/gcp-auth-protocol
        with:
          project-id: ${{ vars.GCP_PROJECT_ID }}
          project-number: ${{ vars.GCP_PROJECT_NUMBER }}
          provider-name: nexus-tools-protocol

      - name: Authenticate to GCP (infra)
        id: auth-infra
        uses: ./.github/actions/gcp-auth-infra
        with:
          project-id: ${{ vars.GCP_INFRA_PROJECT_ID }}
          project-number: ${{ vars.GCP_INFRA_PROJECT_NUMBER }}
          provider-name: nexus-tools

      - name: Generate signed HTTP keys (idempotent)
        run: |
          set -euo pipefail
          VERSIONED="${{ steps.names.outputs.versioned }}"
          PROJECT="${{ vars.GCP_PROJECT_ID }}"
          META="${RUNNER_TEMP}/prepare/meta/${{ matrix.tool }}.json"

          FQNS=$(jq -r '.[].fqn' "$META" | tr '\n' ' ' | xargs)
          echo "FQNs for $VERSIONED: $FQNS"

          FORCE_FLAG=""
          EXISTING=$(gcloud secrets versions access latest \
            --secret="nexus-tools-${VERSIONED}-signed-http-keys" \
            --project="$PROJECT" 2>/dev/null || echo "")
          if [ -n "$EXISTING" ]; then
            EXISTING_FQNS=$(echo "$EXISTING" | jq -r '[.keys_registry[].fqn] | sort | join(" ")')
            WANTED_FQNS=$(echo "$FQNS" | tr ' ' '\n' | sort | tr '\n' ' ' | xargs)
            if [ "$EXISTING_FQNS" != "$WANTED_FQNS" ]; then
              echo "FQNs changed: '$EXISTING_FQNS' -> '$WANTED_FQNS' — forcing new keys"
              FORCE_FLAG="--force"
            else
              echo "FQNs unchanged — skipping keygen"
              exit 0
            fi
          fi

          docker run --rm \
            -e GOOGLE_APPLICATION_CREDENTIALS=/tmp/creds.json \
            -v "${{ steps.auth-gcs.outputs.credentials_file_path }}:/tmp/creds.json:ro" \
            gcr.io/${{ vars.GCP_INFRA_PROJECT_ID }}/nexus-next/generate-signed-http-keys:latest \
            python /app/bin/generate_signed_http_keys.py "nexus-tools" "$VERSIONED" \
              --fqns $FQNS \
              --project "$PROJECT" $FORCE_FLAG

      - name: Upload tool config to GCS
        run: |
          set -euo pipefail
          BUCKET="${{ vars.GCP_PROJECT_ID }}-nexus-tools"
          NETWORK="${{ vars.SUI_NETWORK }}"
          VERSIONED="${{ steps.names.outputs.versioned }}"
          LOCAL="${{ steps.render.outputs.config }}"
          REMOTE="gs://${BUCKET}/${NETWORK}/offchain/tools/${VERSIONED}.json"

          # Preserve existing image tag pinning (only NEW versions adopt build SHA).
          EXISTING_TAG=$(gsutil cat "$REMOTE" 2>/dev/null | jq -r '.tag // empty' || true)
          if [ -n "$EXISTING_TAG" ]; then
            echo "Preserving existing image tag: $EXISTING_TAG"
            jq --arg t "$EXISTING_TAG" '.tag = $t' "$LOCAL" > "${LOCAL}.tmp"
            mv "${LOCAL}.tmp" "$LOCAL"
          fi

          LOCAL_HASH=$(sha256sum "$LOCAL" | cut -c1-16)
          REMOTE_HASH=$(gsutil cat "$REMOTE" 2>/dev/null | sha256sum | cut -c1-16 || echo "")
          if [ "$LOCAL_HASH" = "$REMOTE_HASH" ]; then
            echo "Tool config unchanged — skipping upload"
          else
            gsutil cp "$LOCAL" "$REMOTE"
            echo "Uploaded $REMOTE"
          fi

      - name: Upload per-tool artifact
        uses: actions/upload-artifact@v4
        with:
          name: prepare-${{ matrix.tool }}
          retention-days: 1
          path: |
            ${{ runner.temp }}/prepare/meta/${{ matrix.tool }}.json
            ${{ runner.temp }}/prepare/gcs/tools/${{ steps.names.outputs.versioned }}.json

  aggregate:
    name: Aggregate prepare artifacts
    runs-on: ubuntu-latest
    needs: prepare
    environment: >-
      ${{
        (inputs.target-ref || github.event_name == 'pull_request' && github.base_ref || github.ref_name) == 'mainnet' && 'mainnet' ||
        'testnet'
      }}
    steps:
      - name: Download all prepare artifacts
        uses: actions/download-artifact@v4
        with:
          pattern: prepare-*
          merge-multiple: true
          path: ${{ runner.temp }}/prepare

      - name: Build manifest
        id: manifest
        run: |
          set -euo pipefail
          PREPARE="${{ runner.temp }}/prepare"
          MANIFEST="${PREPARE}/manifest.json"
          echo '{}' > "$MANIFEST"
          MATRIX='${{ inputs.matrix-json }}'
          echo "$MATRIX" | jq -c '.include[]' | while read -r entry; do
            TOOL=$(echo "$entry" | jq -r '.tool')
            VERSION=$(echo "$entry" | jq -r '.version')
            COMMAND=$(echo "$entry" | jq -r '.command')
            jq --arg t "$TOOL" --arg v "$VERSION" --arg c "$COMMAND" \
              '.[$t] = {command: $c, version: $v}' \
              "$MANIFEST" > "${MANIFEST}.tmp" && mv "${MANIFEST}.tmp" "$MANIFEST"
          done
          jq . "$MANIFEST"
          HASH=$(sha256sum "$MANIFEST" | cut -c1-16)
          echo "hash=$HASH" >> "$GITHUB_OUTPUT"

      - name: Authenticate to GCP (protocol)
        uses: ./.github/actions/gcp-auth-protocol
        with:
          project-id: ${{ vars.GCP_PROJECT_ID }}
          project-number: ${{ vars.GCP_PROJECT_NUMBER }}
          provider-name: nexus-tools-protocol

      - name: Upload manifest to GCS (content-addressed)
        run: |
          set -euo pipefail
          BUCKET="${{ vars.GCP_PROJECT_ID }}-nexus-tools"
          NETWORK="${{ vars.SUI_NETWORK }}"
          HASH="${{ steps.manifest.outputs.hash }}"
          REMOTE="gs://${BUCKET}/${NETWORK}/offchain/manifest/${HASH}.json"
          if gsutil -q stat "$REMOTE" 2>/dev/null; then
            echo "Manifest ${HASH}.json already exists — skipping"
          else
            gsutil cp "${{ runner.temp }}/prepare/manifest.json" "$REMOTE"
            echo "Uploaded $REMOTE"
          fi

      - name: Upload bundled prepare artifact
        uses: actions/upload-artifact@v4
        with:
          name: offchain-tools-prepare
          retention-days: 1
          path: ${{ runner.temp }}/prepare/
```

- [ ] **Step 2: Validate YAML and commit.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/offchain-tools.prepare.yml'))" && echo OK
git add .github/workflows/offchain-tools.prepare.yml
git commit -m "ci: add offchain-tools.prepare callable workflow

Per-tool matrix: pulls the pushed image, runs <command> --meta, renders
the Cloud Run config, generates signed-HTTP keys (idempotent on FQN set),
uploads tool config to GCS. Aggregator job builds the manifest and the
offchain-tools-prepare GitHub artifact consumed by register."
```

---

### Task 5.4: `offchain-tools.register.yml`

**Files:**
- Create: `.github/workflows/offchain-tools.register.yml`

Single-job (serialized Sui state) registration. Mirrors ava-game's register flow with `nexus-tools-` prefix and bucket name.

- [ ] **Step 1: Create the workflow.**

```yaml
name: Offchain Tools Register

on:
  workflow_call:
    inputs:
      target-ref:
        required: false
        type: string

permissions:
  contents: read
  id-token: write

env:
  NEXUS_TAG: ${{ vars.NEXUS_TAG }}
  SUI_CHANNEL: ${{ vars.SUI_CHANNEL }}

jobs:
  register:
    name: Register offchain tools
    runs-on: ubuntu-latest
    environment: >-
      ${{
        (inputs.target-ref || github.event_name == 'pull_request' && github.base_ref || github.ref_name) == 'mainnet' && 'mainnet' ||
        'testnet'
      }}
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Install Sui CLI
        uses: ./.github/actions/install-sui
        with:
          sui-channel: ${{ vars.SUI_CHANNEL }}
          cache-version: ${{ vars.SUI_CACHE_VERSION }}

      - name: Download prepare artifacts
        uses: actions/download-artifact@v4
        with:
          name: offchain-tools-prepare
          path: ${{ runner.temp }}/prepare

      - name: Show artifacts
        id: artifacts
        run: |
          PREPARE="${{ runner.temp }}/prepare"
          echo "manifest=${PREPARE}/manifest.json" >> "$GITHUB_OUTPUT"
          echo "meta_dir=${PREPARE}/meta" >> "$GITHUB_OUTPUT"
          jq . "${PREPARE}/manifest.json"

      - name: Authenticate to GCP (protocol)
        id: auth-gcs
        uses: ./.github/actions/gcp-auth-protocol
        with:
          project-id: ${{ vars.GCP_PROJECT_ID }}
          project-number: ${{ vars.GCP_PROJECT_NUMBER }}
          provider-name: nexus-tools-protocol

      - name: Authenticate to GCP (infra)
        uses: ./.github/actions/gcp-auth-infra
        with:
          project-id: ${{ vars.GCP_INFRA_PROJECT_ID }}
          project-number: ${{ vars.GCP_INFRA_PROJECT_NUMBER }}
          provider-name: nexus-tools

      - name: Install Nexus CLI
        uses: ./.github/actions/install-nexus-cli
        with:
          nexus-tag: ${{ env.NEXUS_TAG }}
          infra-registry: gcr.io/${{ vars.GCP_INFRA_PROJECT_ID }}

      - name: Configure wallet and Nexus
        run: |
          set -euo pipefail
          yes "" | sui client new-env --alias tool-register --rpc "${{ vars.NEXT_PUBLIC_SUI_RPC_URL }}" || true
          sui client switch --env tool-register

          IMPORT=$(sui keytool import "${{ secrets.SUI_DEPLOYER_MNEMONIC }}" ed25519)
          ADDR=$(echo "$IMPORT" | grep -oE '0x[0-9a-fA-F]{64}')
          sui client switch --address "$ADDR"
          echo "DEPLOYER_ADDR=$ADDR" >> "$GITHUB_ENV"

          KEY_INDEX=$(sui keytool list --json | jq -r --arg addr "$ADDR" 'to_entries[] | select(.value.suiAddress == $addr) | .key')
          SUI_PK=$(jq -r --argjson idx "$KEY_INDEX" '.[$idx]' "$HOME/.sui/sui_config/sui.keystore")

          NETWORK="${{ vars.SUI_NETWORK }}"
          wget -q -O "$RUNNER_TEMP/objects.toml" \
            "https://storage.googleapis.com/production-talus-sui-objects/${{ env.NEXUS_TAG }}/objects.${NETWORK}.toml?t=$(date +%s)"

          mkdir -p "$HOME/.nexus"
          cp "$RUNNER_TEMP/objects.toml" "$HOME/.nexus/objects.${NETWORK}.toml"
          nexus conf set \
            --nexus.objects "$HOME/.nexus/objects.${NETWORK}.toml" \
            --sui.rpc-url "${{ vars.NEXT_PUBLIC_SUI_RPC_URL }}" \
            --sui.pk "$SUI_PK"

      - name: Load Nexus objects
        run: |
          set -euo pipefail
          WORKFLOW_PKG=$(grep '^workflow_pkg_id' "$RUNNER_TEMP/objects.toml" | cut -d'"' -f2)
          TOOL_REGISTRY=$(grep -A1 '^\[tool_registry\]' "$RUNNER_TEMP/objects.toml" | grep 'object_id' | cut -d'"' -f2)
          GAS_SERVICE=$(grep -A1 '^\[gas_service\]' "$RUNNER_TEMP/objects.toml" | grep 'object_id' | cut -d'"' -f2)
          echo "WORKFLOW_PKG=$WORKFLOW_PKG" >> "$GITHUB_ENV"
          echo "TOOL_REGISTRY=$TOOL_REGISTRY" >> "$GITHUB_ENV"
          echo "GAS_SERVICE=$GAS_SERVICE" >> "$GITHUB_ENV"

      - name: Register offchain tools
        shell: bash
        run: |
          set -euo pipefail
          MANIFEST="${{ steps.artifacts.outputs.manifest }}"
          META_DIR="${{ steps.artifacts.outputs.meta_dir }}"
          BUCKET="${{ vars.GCP_PROJECT_ID }}-nexus-tools"
          NETWORK="${{ vars.SUI_NETWORK }}"

          REGISTERED=$(nexus tool list --json 2>/dev/null | jq -r '.[].fqn' 2>/dev/null || true)
          echo "::group::Already registered"; echo "$REGISTERED"; echo "::endgroup::"

          to_bytes() { printf '%s' "$1" | xxd -i -c 9999 | grep '0x' | tr -d ' \n' | sed 's/,$//'; }
          COIN_ID=$(sui client gas --json | jq -r '.[0].gasCoinId')
          echo "Using coin: $COIN_ID"

          for TOOL in $(jq -r 'keys[]' "$MANIFEST"); do
            META="${META_DIR}/${TOOL}.json"
            VERSION=$(jq -r --arg t "$TOOL" '.[$t].version' "$MANIFEST")
            COUNT=$(jq 'length' "$META")
            for i in $(seq 0 $((COUNT - 1))); do
              FQN=$(jq -r ".[$i].fqn" "$META")
              TOOL_URL="http://${NETWORK}-${TOOL}-v${VERSION}.tools.internal"

              if echo "$REGISTERED" | grep -qF "$FQN"; then
                echo "  ✓ $FQN already registered, skipping"
                continue
              fi

              DESC=$(jq -r ".[$i].description" "$META")
              TIMEOUT=$(jq -r ".[$i].timeout" "$META")
              INPUT_SCHEMA=$(jq -c ".[$i].input_schema" "$META")
              OUTPUT_SCHEMA=$(jq -c ".[$i].output_schema" "$META")

              FQN_VEC="vector[$(to_bytes "$FQN")]"
              URL_VEC="vector[$(to_bytes "$TOOL_URL")]"
              DESC_VEC="vector[$(to_bytes "$DESC")]"
              IN_VEC="vector[$(to_bytes "$INPUT_SCHEMA")]"
              OUT_VEC="vector[$(to_bytes "$OUTPUT_SCHEMA")]"

              PTB_ARGS="\
                --move-call 0x1::ascii::string \"${FQN_VEC}\" \
                --assign fqn \
                --move-call ${WORKFLOW_PKG}::tool_registry::register_off_chain_tool \
                  @${TOOL_REGISTRY} fqn \
                  \"${URL_VEC}\" \"${DESC_VEC}\" \"${IN_VEC}\" \"${OUT_VEC}\" \
                  ${TIMEOUT}u64 @${COIN_ID} @0x6 \
                --assign r \
                --move-call ${WORKFLOW_PKG}::gas::deescalate r.0 r.1 \
                --assign g \
                --move-call ${WORKFLOW_PKG}::gas::create_tool_gas_and_share \
                  @${GAS_SERVICE} r.0 g 0u64 \
                --move-call 0x2::transfer::public_share_object \
                  \"<${WORKFLOW_PKG}::tool_registry::Tool>\" r.0 \
                --transfer-objects \"[r.1, g]\" @${DEPLOYER_ADDR} \
                --gas-budget 100000000 \
                --json"

              OUT=$(eval "sui client ptb $PTB_ARGS")
              STATUS=$(echo "$OUT" | jq -r '.effects.status.status')
              if [ "$STATUS" != "success" ]; then
                echo "::error::PTB failed for $FQN"
                echo "$OUT" | jq '.effects.status'
                exit 1
              fi

              CAP_ID=$(echo "$OUT" | jq -r '[.objectChanges[] | select(.objectType | test("OverTool")) | .objectId] | .[0]')
              if [ -z "$CAP_ID" ] || [ "$CAP_ID" = "null" ]; then
                echo "::error::No OwnerCap<OverTool> for $FQN"
                exit 1
              fi

              jq -n --arg fqn "$FQN" --arg cap "$CAP_ID" \
                '{fqn: $fqn, owner_cap_over_tool: $cap}' \
                | gsutil cp - "gs://${BUCKET}/${NETWORK}/offchain/registration/${TOOL}/${FQN}.json"
              echo "  + $FQN registered (cap=$CAP_ID)"
            done
          done

      - name: Register signing keys
        shell: bash
        run: |
          set -euo pipefail
          MANIFEST="${{ steps.artifacts.outputs.manifest }}"
          META_DIR="${{ steps.artifacts.outputs.meta_dir }}"
          PROJECT="${{ vars.GCP_PROJECT_ID }}"
          BUCKET="${{ vars.GCP_PROJECT_ID }}-nexus-tools"
          NETWORK="${{ vars.SUI_NETWORK }}"

          for TOOL in $(jq -r 'keys[]' "$MANIFEST"); do
            VERSION=$(jq -r --arg t "$TOOL" '.[$t].version' "$MANIFEST")
            VERSIONED="${TOOL}-v${VERSION}"

            KEYS=$(gcloud secrets versions access latest \
              --secret="nexus-tools-${VERSIONED}-signed-http-keys" \
              --project="$PROJECT")
            KEYS_HASH=$(echo "$KEYS" | jq -c '[.keys_registry[] | {fqn, public_key_hex}]' | sha256sum | cut -c1-16)

            for FQN in $(jq -r '.[].fqn' "${META_DIR}/${TOOL}.json"); do
              REG=$(gsutil cat "gs://${BUCKET}/${NETWORK}/offchain/registration/${TOOL}/${FQN}.json")
              OWNER_CAP=$(echo "$REG" | jq -r '.owner_cap_over_tool')
              REGISTERED_HASH=$(echo "$REG" | jq -r '.signing_key_hash // ""')
              if [ "$REGISTERED_HASH" = "$KEYS_HASH" ]; then
                echo "  ✓ $FQN signing key unchanged — skipping"
                continue
              fi

              PRIVATE_KEY=$(echo "$KEYS" | jq -re --arg f "$FQN" '.keys_registry[] | select(.fqn == $f) | .private_key_hex')
              REG_OUT=$(nexus tool auth register-key \
                --json \
                --tool-fqn "$FQN" \
                --owner-cap "$OWNER_CAP" \
                --signing-key "$PRIVATE_KEY" \
                --description "ci-managed")
              KID=$(echo "$REG_OUT" | jq -r '.tool_kid')

              echo "$REG" | jq --arg h "$KEYS_HASH" --argjson kid "$KID" \
                '. + {signing_key_hash: $h, tool_kid: $kid}' \
                | gsutil cp - "gs://${BUCKET}/${NETWORK}/offchain/registration/${TOOL}/${FQN}.json"
              echo "  + $FQN registered with tool_kid=$KID"
            done
          done

      - name: Reconcile toolkit-config secret
        shell: bash
        run: |
          set -euo pipefail
          MANIFEST="${{ steps.artifacts.outputs.manifest }}"
          META_DIR="${{ steps.artifacts.outputs.meta_dir }}"
          PROJECT="${{ vars.GCP_PROJECT_ID }}"
          BUCKET="${{ vars.GCP_PROJECT_ID }}-nexus-tools"
          NETWORK="${{ vars.SUI_NETWORK }}"

          for TOOL in $(jq -r 'keys[]' "$MANIFEST"); do
            VERSION=$(jq -r --arg t "$TOOL" '.[$t].version' "$MANIFEST")
            VERSIONED="${TOOL}-v${VERSION}"
            SECRET="nexus-tools-${VERSIONED}-signed-http-toolkit-config"

            gcloud secrets describe "$SECRET" --project="$PROJECT" >/dev/null 2>&1 \
              || gcloud secrets create "$SECRET" --project="$PROJECT" --replication-policy=automatic

            SKELETON=$(gcloud secrets versions access latest \
              --secret="nexus-tools-${VERSIONED}-signed-http-keys" \
              --project="$PROJECT" \
              | jq -c '.toolkit_config')

            DESIRED="$SKELETON"
            for FQN in $(jq -r '.[].fqn' "${META_DIR}/${TOOL}.json"); do
              REG=$(gsutil cat "gs://${BUCKET}/${NETWORK}/offchain/registration/${TOOL}/${FQN}.json")
              KID=$(echo "$REG" | jq -r '.tool_kid // "MISSING"')
              if [ "$KID" = "MISSING" ] || [ "$KID" = "null" ]; then
                echo "::error::No tool_kid for $FQN — refusing to write toolkit-config"
                exit 1
              fi
              DESIRED=$(echo "$DESIRED" | jq -c --arg fqn "$FQN" --argjson kid "$KID" \
                '.signed_http.tools[$fqn].tool_kid = $kid')
            done

            CURRENT=$(gcloud secrets versions access latest \
              --secret="$SECRET" --project="$PROJECT" 2>/dev/null | jq -c . 2>/dev/null || echo "")
            if [ "$DESIRED" = "$CURRENT" ]; then
              echo "  ✓ toolkit-config for $VERSIONED unchanged"
            else
              echo -n "$DESIRED" | gcloud secrets versions add "$SECRET" --project="$PROJECT" --data-file=-
              echo "  + Wrote new toolkit-config for $VERSIONED"
            fi
          done
```

- [ ] **Step 2: Validate YAML and commit.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/offchain-tools.register.yml'))" && echo OK
git add .github/workflows/offchain-tools.register.yml
git commit -m "ci: add offchain-tools.register callable workflow

Single-job (serialized Sui state) registration. Skips already-registered
FQNs; for new ones, runs the PTB, extracts OwnerCap<OverTool>, saves
registration JSON to GCS, registers signing key, reconciles the
toolkit-config secret with the on-chain tool_kid (sole-writer rule)."
```

---

### Task 5.5: `offchain-tools.readiness.yml`

**Files:**
- Create: `.github/workflows/offchain-tools.readiness.yml`

PR-only merge gate. Per-tool matrix checks GCS for expected artifacts.

- [ ] **Step 1: Create the workflow.**

```yaml
name: Offchain Tools Readiness

on:
  workflow_call:
    inputs:
      matrix-json:
        required: true
        type: string

permissions:
  contents: read
  id-token: write

jobs:
  check:
    name: "Readiness ${{ matrix.tool }} v${{ matrix.version }}"
    runs-on: ubuntu-latest
    environment: >-
      ${{
        (github.base_ref || github.ref_name) == 'mainnet' && 'mainnet' ||
        'testnet'
      }}
    strategy:
      matrix: ${{ fromJson(inputs.matrix-json) }}
      fail-fast: false
    steps:
      - name: Authenticate to GCP (protocol)
        uses: ./.github/actions/gcp-auth-protocol
        with:
          project-id: ${{ vars.GCP_PROJECT_ID }}
          project-number: ${{ vars.GCP_PROJECT_NUMBER }}
          provider-name: nexus-tools-protocol

      - name: Check GCS artifacts
        run: |
          set -euo pipefail
          BUCKET="${{ vars.GCP_PROJECT_ID }}-nexus-tools"
          NETWORK="${{ vars.SUI_NETWORK }}"
          TOOL="${{ matrix.tool }}"
          VERSION="${{ matrix.version }}"
          VERSIONED="${TOOL}-v${VERSION}"

          MISSING=()

          CFG="gs://${BUCKET}/${NETWORK}/offchain/tools/${VERSIONED}.json"
          gsutil -q stat "$CFG" 2>/dev/null || MISSING+=("Cloud Run config: $CFG")

          REG_DIR="gs://${BUCKET}/${NETWORK}/offchain/registration/${TOOL}/"
          REG_COUNT=$(gsutil ls "$REG_DIR" 2>/dev/null | wc -l || echo 0)
          if [ "$REG_COUNT" = "0" ]; then
            MISSING+=("FQN registrations under $REG_DIR")
          fi

          if [ ${#MISSING[@]} -gt 0 ]; then
            echo "::error::Missing artifacts for ${VERSIONED}:"
            for m in "${MISSING[@]}"; do echo "  - $m"; done
            echo ""
            echo "Trigger the Offchain Tools publish workflow with this PR number to seed them."
            exit 1
          fi

          echo "✓ All artifacts present for ${VERSIONED}"
```

- [ ] **Step 2: Validate YAML and commit.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/offchain-tools.readiness.yml'))" && echo OK
git add .github/workflows/offchain-tools.readiness.yml
git commit -m "ci: add offchain-tools.readiness PR merge gate

Per-tool matrix that checks GCS for the Cloud Run config + at least one
FQN registration JSON. Fails red if either is missing, with a message
pointing the author at the publish workflow_dispatch."
```

---

## Phase 6 — Top-level orchestrator + bootstrap

### Task 6.1: `ci.yml` orchestrator

**Files:**
- Create: `.github/workflows/ci.yml`

Wires the discover/deploy/prepare/register/readiness workflows according to the trigger matrix. Coexists with the existing `pre-commit_(PR).yml`, `pre-commit_(main).yml`, `audit.yml`, `coverage.yaml`, `coverage-baseline.yml`, `sync_docs.yml` — those keep firing on their own triggers; this `ci.yml` is the new orchestrator for the chain pipeline.

- [ ] **Step 1: Create the workflow.**

```yaml
name: CI

on:
  workflow_dispatch:
    inputs:
      pr-number:
        description: 'PR number to publish for (chain ops fire against the PR HEAD)'
        required: false
        type: string
  pull_request:
    types: [opened, synchronize, reopened]
  push:
    branches: [main, testnet, mainnet]

permissions:
  contents: write
  packages: write
  id-token: write
  pull-requests: write
  checks: write

jobs:
  # ── Resolve target ref for workflow_dispatch with PR# ────────
  resolve-target:
    if: github.event_name == 'workflow_dispatch' && inputs.pr-number != ''
    runs-on: ubuntu-latest
    outputs:
      ref: ${{ steps.r.outputs.ref }}
      head-sha: ${{ steps.r.outputs.head-sha }}
      base-ref: ${{ steps.r.outputs.base-ref }}
    steps:
      - name: Resolve
        id: r
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          set -euo pipefail
          BASE=$(gh api "repos/${{ github.repository }}/pulls/${{ inputs.pr-number }}" --jq '.base.ref')
          HEAD_REF=$(gh api "repos/${{ github.repository }}/pulls/${{ inputs.pr-number }}" --jq '.head.ref')
          HEAD_SHA=$(gh api "repos/${{ github.repository }}/pulls/${{ inputs.pr-number }}" --jq '.head.sha')
          echo "base-ref=$BASE" >> "$GITHUB_OUTPUT"
          echo "ref=$HEAD_REF" >> "$GITHUB_OUTPUT"
          echo "head-sha=$HEAD_SHA" >> "$GITHUB_OUTPUT"

  # ── Discover tools (always) ──────────────────────────────────
  discover:
    uses: ./.github/workflows/offchain-tools.discover.yml
    with:
      base-ref: ${{ github.event_name == 'pull_request' && github.event.pull_request.base.sha || '' }}

  # ── Build / push ─────────────────────────────────────────────
  # PR (any base): dry-run on changed matrix.
  # Push main/testnet/mainnet: push on all matrix.
  # workflow_dispatch with PR#: push on all matrix.
  deploy:
    needs: [discover, resolve-target]
    if: always() && !cancelled() && needs.discover.result == 'success'
    uses: ./.github/workflows/offchain-tools.deploy.yml
    with:
      target-ref: ${{ needs.resolve-target.outputs.base-ref || github.ref_name }}
      matrix-json: >-
        ${{
          (github.event_name == 'pull_request') && needs.discover.outputs.matrix-changed
          || needs.discover.outputs.matrix-all
        }}
      dry-run: ${{ github.event_name == 'pull_request' }}
    secrets: inherit

  # ── Prepare (only on push to testnet/mainnet or workflow_dispatch) ──
  prepare:
    needs: [discover, deploy, resolve-target]
    if: >-
      always() && !cancelled() && needs.deploy.result == 'success' &&
      (github.event_name == 'workflow_dispatch' ||
       (github.event_name == 'push' && (github.ref_name == 'testnet' || github.ref_name == 'mainnet')))
    uses: ./.github/workflows/offchain-tools.prepare.yml
    with:
      target-ref: ${{ needs.resolve-target.outputs.base-ref || github.ref_name }}
      matrix-json: ${{ needs.discover.outputs.matrix-all }}
    secrets: inherit

  # ── Register (after prepare) ────────────────────────────────
  register:
    needs: [prepare, resolve-target]
    if: always() && !cancelled() && needs.prepare.result == 'success'
    uses: ./.github/workflows/offchain-tools.register.yml
    with:
      target-ref: ${{ needs.resolve-target.outputs.base-ref || github.ref_name }}
    secrets: inherit

  # ── Readiness (PRs targeting testnet/mainnet only) ──────────
  readiness:
    needs: discover
    if: >-
      github.event_name == 'pull_request' &&
      (github.base_ref == 'testnet' || github.base_ref == 'mainnet') &&
      fromJson(needs.discover.outputs.matrix-changed).include[0] != null
    uses: ./.github/workflows/offchain-tools.readiness.yml
    with:
      matrix-json: ${{ needs.discover.outputs.matrix-changed }}
    secrets: inherit

  # ── Retrigger PR after dispatched run (flips readiness green) ──
  retrigger-pr:
    needs: [register, resolve-target]
    if: always() && !cancelled() && needs.register.result == 'success' && github.event_name == 'workflow_dispatch' && inputs.pr-number != ''
    runs-on: ubuntu-latest
    steps:
      - uses: ./.github/actions/retrigger-pr
        with:
          pr-number: ${{ inputs.pr-number }}
          gpg-signing-key: ${{ secrets.GPG_DEVOPS_SIGNING_KEY }}
          github-token: ${{ github.token }}

  # ── ci-gate: single required status check for branch protection ──
  ci-gate:
    if: always()
    needs: [discover, deploy, prepare, register, readiness]
    runs-on: ubuntu-latest
    steps:
      - name: Check job results
        run: |
          set -euo pipefail
          declare -A r=(
            [discover]="${{ needs.discover.result }}"
            [deploy]="${{ needs.deploy.result }}"
            [prepare]="${{ needs.prepare.result }}"
            [register]="${{ needs.register.result }}"
            [readiness]="${{ needs.readiness.result }}"
          )
          for j in "${!r[@]}"; do
            v="${r[$j]}"
            case "$v" in
              success|skipped) echo "✓ $j: $v" ;;
              *) echo "✗ $j: $v"; FAIL=1 ;;
            esac
          done
          [ -z "${FAIL:-}" ] || exit 1
```

- [ ] **Step 2: Validate YAML.**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo OK
```

- [ ] **Step 3: Commit.**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add top-level orchestrator ci.yml

Wires discover/deploy/prepare/register/readiness per the trigger matrix:
- PR (any base): discover, dry-run deploy on changed matrix.
- PR with base testnet/mainnet: also readiness gate.
- Push to main: discover, full deploy push (no chain ops).
- Push to testnet/mainnet: discover, deploy, prepare, register on full matrix.
- workflow_dispatch with PR#: resolves PR head, runs full chain, then
  retrigger-pr flips readiness green.
- ci-gate aggregates job results for branch protection."
```

---

### Task 6.2: Update repo README with the new flow

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Read the existing README.**

```bash
cat README.md
```

- [ ] **Step 2: Add a "Development" section near the top documenting the offchain/ layout and the new workflow.**

Insert this block after the existing introduction / above the existing tool list (or wherever fits the existing structure):

```markdown
## Layout

```
nexus-tools/
├── offchain/          # Rust workspace + tools (cd here before cargo)
│   ├── Cargo.toml
│   ├── tools/
│   └── Dockerfile     # shared, parameterized
├── onchain/           # reserved for future Move tools
└── .github/workflows/ # CI: pre-commit, offchain tools build+publish+register
```

All cargo commands run from `offchain/`:

```bash
cd offchain
cargo build --workspace
cargo test --workspace
```

## Deploying a tool

Each tool ships `tools.json` + `build.rs` + a `[[bin]]` declaration. The
CI pipeline discovers tools by globbing `offchain/tools/*/tools.json` and:

1. Builds per-tool Docker images on every push, tagging
   `ghcr.io/<owner>/nexus-tools/<tool>:sha-<7>` and
   `gcr.io/<infra>/nexus-tools/<tool>:sha-<7>`.
2. On pushes to `testnet`/`mainnet` or a `workflow_dispatch` with a PR
   number, generates signed-HTTP keys, uploads Cloud Run configs to GCS,
   and registers FQNs on Nexus.

Branches:
- `main` — development integration; CI runs but no chain ops.
- `testnet` — push fires the full chain pipeline against testnet.
- `mainnet` — push fires the full chain pipeline against mainnet.

To deploy work from `main` to testnet, open a `promote/<topic>` PR with
base `testnet` and head `main`. The readiness check is red until you
trigger the `CI` workflow with the PR number; the chain ops run and
push a signed empty commit back to your PR, flipping readiness green.
Merge once green. See the design spec for details:
[docs/superpowers/specs/2026-05-19-offchain-tools-pipeline-design.md](docs/superpowers/specs/2026-05-19-offchain-tools-pipeline-design.md).
```

- [ ] **Step 3: Commit.**

```bash
git add README.md
git commit -m "docs: README — document offchain/ layout and deploy flow"
```

---

## Phase 7 — Bootstrap & first deploy notes

### Task 7.1: Add bootstrap doc

**Files:**
- Create: `docs/superpowers/notes/2026-05-19-offchain-bootstrap.md`

- [ ] **Step 1: Create the bootstrap note.**

```bash
mkdir -p docs/superpowers/notes
cat > docs/superpowers/notes/2026-05-19-offchain-bootstrap.md <<'EOF'
# Offchain Tools — Bootstrap Notes

After this branch (`feat/offchain-tools-pipeline`) merges to `main`, the
chain pipeline has never run anywhere. The first deploy requires extra
care because the readiness gate compares against artifacts that do not
yet exist.

## Prerequisites (one-time)

Per environment (`testnet`, `mainnet`):

1. **GCP project** (`GCP_PROJECT_ID`): owns the GCS bucket and Secret
   Manager. Bucket name: `<project-id>-nexus-tools`.
2. **Workload identity pool provider** (`nexus-tools-protocol`) bound
   to this repo's `id-token`. Permissions:
   - `roles/storage.objectAdmin` on the bucket.
   - `roles/secretmanager.admin` on the project.
3. **Infra project** (`GCP_INFRA_PROJECT_ID`): hosts GCR + the
   `nexus-sdk/shell` and `generate-signed-http-keys` images.
4. **Workload identity pool provider** (`nexus-tools`) bound to this
   repo's `id-token`. Permissions:
   - `roles/artifactregistry.writer` on the GCR registry.
   - `roles/artifactregistry.reader` on `gcr.io/<infra>/nexus-sdk/*` and
     `gcr.io/<infra>/nexus-next/generate-signed-http-keys`.
5. **GitHub environments** named `testnet` and `mainnet` with these vars:
   - `GCP_PROJECT_ID`, `GCP_PROJECT_NUMBER`
   - `GCP_INFRA_PROJECT_ID`, `GCP_INFRA_PROJECT_NUMBER`
   - `SUI_NETWORK` (`testnet` or `mainnet`)
   - `NEXT_PUBLIC_SUI_RPC_URL`
   - `NEXUS_TAG`, `SUI_CHANNEL`, `SUI_CACHE_VERSION`
6. **GitHub environment secrets**:
   - `SUI_DEPLOYER_MNEMONIC`
   - `GPG_DEVOPS_SIGNING_KEY`

## First deploy

1. Confirm `main` is at the desired commit.
2. Cut a long-lived `testnet` branch from `main`. `git checkout -b testnet && git push -u origin testnet`.
3. The push triggers `CI`, which runs the full chain pipeline against
   the `testnet` environment. Watch the run.
4. On success, every tool has:
   - An image at both registries.
   - A Cloud Run config in
     `gs://<bucket>/testnet/offchain/tools/<tool>-v<version>.json`.
   - Signed-HTTP keys + toolkit-config secrets in Secret Manager.
   - At least one FQN registered on-chain.
5. Repeat for `mainnet` once that branch is cut.

## Subsequent deploys

Use the promote flow described in the spec — `promote/<topic>` PRs +
`workflow_dispatch` with PR number.
EOF
```

- [ ] **Step 2: Commit.**

```bash
git add docs/superpowers/notes/2026-05-19-offchain-bootstrap.md
git commit -m "docs: add offchain bootstrap notes for first deploy"
```

---

## Final verification

### Task 8.1: Full repo sanity check

**Files:** none modified.

- [ ] **Step 1: All tests + lint pass on the new layout.**

```bash
just pre-commit::cargo-check
just pre-commit::cargo-clippy
just pre-commit::cargo-nextest-build
```

Expected: each succeeds.

- [ ] **Step 2: All workflow YAML parses.**

```bash
for f in .github/workflows/*.yml .github/workflows/*.yaml .github/actions/*/action.yml; do
  python -c "import yaml; yaml.safe_load(open('$f'))" && echo "OK: $f" || echo "FAIL: $f"
done
```

Expected: every file reports `OK`.

- [ ] **Step 3: Discover step works locally (smoke test of the bash logic).**

Run the body of the discover workflow's "Build matrices" step manually:

```bash
ALL='[]'
for TOOLS_JSON in offchain/tools/*/tools.json; do
  [ -f "$TOOLS_JSON" ] || continue
  DIR=$(dirname "$TOOLS_JSON")
  TOOL_NAME=$(jq -r '.tool_name' "$TOOLS_JSON")
  TREE_HASH=$(git rev-parse "HEAD:${DIR}/")
  CONTENT_HASH=$(printf '%s' "$TREE_HASH" | sha256sum | cut -d' ' -f1)
  VERSION=$(printf '%d' "0x${CONTENT_HASH:0:8}")
  echo "${TOOL_NAME} v${VERSION}"
done
```

Expected: prints seven lines, one per tool, each with a numeric version. The version is stable across runs as long as the subtree is unchanged.

- [ ] **Step 4: Push the branch and open a PR to `main` for review.**

```bash
git push -u origin feat/offchain-tools-pipeline
gh pr create --base main --title "Offchain tools CI pipeline" \
  --body "$(cat <<'EOF'
## Summary
Adds end-to-end CI for the seven offchain tools in this repo. Per the
spec in `docs/superpowers/specs/2026-05-19-offchain-tools-pipeline-design.md`:

- Repo reorganised: workspace now lives in `offchain/`.
- Each tool has `tools.json` + `build.rs` + `[[bin]]` declaration.
- New shared `offchain/Dockerfile` (PACKAGE/BINARY/TOOL_FQN_VERSION args).
- Five callable workflows: discover, deploy, prepare, register, readiness.
- Five composite actions: install-sui, install-nexus-cli, gcp-auth-{protocol,infra}, retrigger-pr.
- Top-level `ci.yml` wires it all together by trigger.

This PR runs in no-chain mode (PR to main). Once merged, follow the
bootstrap notes to seed `testnet` and later `mainnet`.

## Test plan
- [ ] CI passes: pre-commit, audit, coverage, ci.yml dry-run.
- [ ] discover emits matrices that include all 7 tools.
- [ ] deploy job builds (no push) for each tool.
- [ ] All seven tools' images succeed to build.
EOF
)"
```

Expected: the PR opens. Watch CI; expect `pre-commit_(PR)`, `Audit Dependencies`, `CI Coverage Check`, and the new `CI` workflow to run. The new `CI` runs `discover` + dry-run `deploy` only (no prepare/register/readiness because base is `main`).

---

## Self-review against the spec

Run through this checklist mentally before executing:

- **Spec § Repo reorganization** → Phase 1.
- **Spec § Per-tool conventions** (`tools.json` + `build.rs` + naming chain) → Phase 2.
- **Spec § Shared Dockerfile** → Phase 3.
- **Spec § Composite actions** (install-sui, install-nexus-cli, gcp-auth-*, retrigger-pr) → Phase 4.
- **Spec § Discover workflow** → Task 5.1.
- **Spec § Deploy workflow** → Task 5.2.
- **Spec § Prepare workflow** → Task 5.3.
- **Spec § Register workflow** → Task 5.4.
- **Spec § Readiness workflow** → Task 5.5.
- **Spec § Top-level ci.yml + trigger matrix** → Task 6.1.
- **Spec § Branch model** → reflected in env-selection expressions in 5.2/5.3/5.4/5.5 and the trigger conditions in 6.1.
- **Spec § State backend (GCS paths, secret names with `nexus-tools-` prefix)** → 5.3 (uploads) and 5.4 (reads/writes).
- **Spec § Bootstrap** → Task 7.1.

All spec sections have at least one corresponding task. The plan calls
out the only known workbench-back-compat oddity (`storage-walrus`
directory vs `walrus` crate name) and preserves it. The GCS bucket
naming (`<project>-nexus-tools`) is a concrete choice not yet locked in
the spec — if the operator wants a different name, search-replace
across workflows before the first deploy.
