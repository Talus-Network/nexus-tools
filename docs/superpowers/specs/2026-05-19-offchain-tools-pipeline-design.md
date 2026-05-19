# Offchain Tools CI Pipeline — Design

Status: draft
Author: brainstormed with @tuky191
Date: 2026-05-19

## Motivation

`nexus-tools` ships seven offchain Nexus tools as independent Rust crates
(`exchanges-coinbase`, `http`, `llm-openai-chat-completion`, `math`,
`social-twitter`, `storage-walrus`, `templating-jinja`). Today the repo
has no automated build, no Docker publish, and no on-chain registration
pipeline. `nexus-workbench` clones the repo and builds each tool via its
own `Dockerfile`, but there is no canonical source of pre-built images and
no automated FQN registration on Nexus.

The Talus `ava-game` repo solves the same problem for game-specific
offchain tools with a three-phase pipeline (build/push → prepare →
register) keyed off per-tool `tools.json` files. This design ports that
pattern to `nexus-tools`, adapted for two structural differences:

1. **Tools are independent crates**, not facets of one shared workspace.
   ava-game's "one image per game, many binaries" pattern collapses to
   **one image per tool** here.
2. **No DAGs.** ava-game's `protocol-dag-publish` flow (DAG hydration,
   commit-back to branch, `promote/*` for auto-deploy) does not apply.
   `promote/*` is repurposed to mean "explicit promote-to-env request".

The pipeline must also stay compatible with `nexus-workbench`'s existing
image refs (`<registry>/nexus-tools/<crate>:<short-sha>`) so local
development environments keep working without changes.

## Goals

- Automated build, publish, and on-chain registration of every offchain
  tool in this repo.
- Per-tool change isolation: editing one tool only rebuilds and
  re-registers that tool.
- Content-addressed versioning: same source ⇒ same FQN version ⇒ no
  on-chain churn on no-op rebuilds.
- Cost-controlled chain operations: PR iteration is free; on-chain
  registration is a deliberate, gated action.
- Backward-compatible with `nexus-workbench` image refs.
- Two environments (`testnet`, `mainnet`); `main` is the development
  integration branch.

## Non-goals (out of scope)

1. Terraform / Cloud Run resource creation. The pipeline produces GCS
   state for Terraform to reconcile; the Terraform stack lives elsewhere.
2. Changes to `nexus-workbench`. Image refs are preserved; workbench's
   build flow keeps working.
3. Onchain tools. `onchain/` is reserved as a placeholder; its layout
   and CI are a separate effort.
4. DAG publishing. `nexus-tools` does not ship DAGs.
5. End-to-end tests against deployed tool URLs.
6. Garbage collection of old `<tool>-v<old-version>` services or
   on-chain FQNs. Old versions stay registered forever.
7. Per-tool Dockerfile customization. Single shared `offchain/Dockerfile`
   for v1.
8. Cross-environment promotion automation.
9. Workspace-level `offchain-versions.json` file. Version flows via
   Docker build-arg per tool.

## Repo reorganization

```
nexus-tools/
├── offchain/
│   ├── Cargo.toml          # workspace (members = ["tools/*"])
│   ├── Cargo.lock
│   ├── rust-toolchain.toml
│   ├── rustfmt.toml
│   ├── deny.toml
│   ├── Dockerfile          # shared, parameterized
│   ├── tools/
│   │   ├── storage-walrus/
│   │   │   ├── Cargo.toml
│   │   │   ├── tools.json          # NEW
│   │   │   ├── build.rs            # NEW
│   │   │   └── src/
│   │   └── ... (6 other tools, all retrofitted)
│   ├── helpers/
│   └── .just
├── onchain/                 # reserved, placeholder README
├── .github/
│   ├── workflows/
│   └── actions/
├── justfile                 # delegates to offchain (and later onchain)
├── README.md
└── ... (LICENCE, CONTRIBUTING, etc.)
```

- Pre-commit, coverage, audit, and sync_docs workflows are repointed at
  `offchain/` (working-directory + path filters). No code changes inside
  tool crates.
- Workspace `Cargo.toml` `[workspace.dependencies.nexus-sdk]` keeps the
  git ref. Any local path overrides documented in a sibling comment add
  one more `..` to compensate for the deeper nesting.

## Per-tool conventions

### `offchain/tools/<tool>/tools.json`

Schema (matches ava-game):

```json
{
  "tool_name": "walrus",
  "command": "walrus",
  "environment": { "RUST_LOG": "info" },
  "resources": { "cpu": "1", "memory": "512Mi" }
}
```

- `tool_name` = Cargo `package.name` = `[[bin]].name`. Picked
  deliberately = **crate name**, not directory name, to preserve
  workbench's `nexus-tools/<crate>` image refs.
- `command` = binary name. Equal to `tool_name` today; the field exists
  for the future case of one crate hosting multiple binaries.
- `environment` is baseline; production layers more on top via Cloud
  Run + secret mounts.
- `resources` is optional; Terraform applies defaults if omitted.

### `offchain/tools/<tool>/build.rs`

Lifted from ava-game with two responsibilities:

1. Validate `Cargo.toml [[bin]].name == tools.json.command`; panic at
   compile time if they drift.
2. Read `TOOL_FQN_VERSION` from env (set by Docker build-arg in CI),
   default `"1"` for local builds. Emit
   `cargo:rustc-env=TOOL_FQN_VERSION=…` so tool source can
   `env!("TOOL_FQN_VERSION")` to build its FQN strings.

`[build-dependencies]`: `serde_json`, `toml`.

### Versioning rule

```
VERSION = u32(first 8 hex chars of sha256(git rev-parse HEAD:offchain/tools/<tool>/))
```

Per-tool, content-addressed, no manual bumps. Computed by CI's discover
step, passed as `TOOL_FQN_VERSION=<n>` Docker build-arg.

### Naming chain (single identifier flows through everything)

| What                       | Pattern                                                          | Example                                              |
|----------------------------|------------------------------------------------------------------|------------------------------------------------------|
| Cargo package + bin        | `<tool_name>`                                                    | `walrus`                                             |
| GHCR image                 | `ghcr.io/talus-network/nexus-tools/<tool_name>:sha-<7>`          | `ghcr.io/talus-network/nexus-tools/walrus:sha-815a8c1` |
| GCR image                  | `<infra-registry>/nexus-tools/<tool_name>:sha-<7>`               | `gcr.io/<infra>/nexus-tools/walrus:sha-815a8c1`      |
| Cloud Run service          | `<network>-<tool_name>-v<version>`                               | `testnet-walrus-v3712044891`                         |
| Internal URL               | `http://<network>-<tool_name>-v<version>.tools.internal`         | `http://testnet-walrus-v3712044891.tools.internal`   |
| Signed-HTTP keys secret    | `nexus-tools-<tool_name>-v<version>-signed-http-keys`            | `nexus-tools-walrus-v3712044891-signed-http-keys`    |
| Toolkit-config secret      | `nexus-tools-<tool_name>-v<version>-signed-http-toolkit-config`  | (same pattern)                                       |
| GCS state path             | `gs://<bucket>/<network>/offchain/{tools,registration,manifest}/...` | (same as ava-game)                              |

Known oddity preserved for back-compat: `tools/storage-walrus/`
ships `[package] name = "walrus"`. Workbench references
`nexus-tools/walrus`; we keep that. Directory name is human-readable
only.

Note on image-tag format: this design uses `sha-<7>` (matching
ava-game's `sha-${GITHUB_SHA::7}`). Workbench currently builds tools
locally from source rather than pulling pre-built images, so its
`SDK_TOOLS_TAG: "<7>"` (unprefixed) does not collide with the published
tags. If workbench later switches to pulling our pre-built images, it
updates its tag format then — not our concern here.

## CI pipeline

### Approach

**Approach A: matrix-driven, fully per-tool.** A dynamic matrix
discovered from `offchain/tools/*/tools.json` drives build, prepare, and
readiness in parallel. Register stays single-job to serialize Sui state.

### Workflows

#### `.github/workflows/offchain-tools.discover.yml`

Globs `offchain/tools/*/tools.json`, computes per-tool subtree versions
(sha256 of `git rev-parse HEAD:offchain/tools/<tool>/`, first 8 hex
chars as u32), and emits two matrices:

- `matrix-all`: every discovered tool.
- `matrix-changed`: only tools whose subtree-version changed since the
  merge base (intersect with `tj-actions/changed-files` against the
  tool's dir, its `Cargo.toml`, plus shared files: `offchain/Cargo.toml`,
  `offchain/Cargo.lock`, `offchain/Dockerfile`,
  `offchain/rust-toolchain.toml`).

Downstream workflows pick which matrix to consume:

| Trigger                                | Build/push | Prepare       | Register      | Readiness     |
|----------------------------------------|------------|---------------|---------------|---------------|
| PR (any base)                          | changed    | —             | —             | changed       |
| Push to `main`                         | changed    | —             | —             | —             |
| Push to `testnet`/`mainnet`            | all        | all           | all           | —             |
| `workflow_dispatch` (PR # input)       | all        | all           | all           | —             |

`matrix-all` is used wherever fleet convergence matters (a deploy must
make every tool's state match the source tree, even if some tools were
not touched in this commit — idempotent re-uploads are cheap). `matrix-changed`
is used wherever we are purely validating or seeking incremental build
speed.

#### `.github/workflows/offchain-tools.deploy.yml`

`workflow_call` taking `target-ref`, `matrix-json`, `dry-run`. One job
per matrix tool:

- Builds via `offchain/Dockerfile` with `--build-arg PACKAGE=<tool>
  --build-arg BINARY=<command> --build-arg TOOL_FQN_VERSION=<version>`,
  where `<version>` is the per-tool subtree hash from the discover
  matrix. The Dockerfile sets it as `ENV TOOL_FQN_VERSION=...` so
  `build.rs` reads it during `cargo build` and bakes it into each FQN
  via `env!("TOOL_FQN_VERSION")`.
- If `dry-run=false`, pushes to:
  - `ghcr.io/talus-network/nexus-tools/<tool>:sha-<7>`
  - `<infra-gcr>/nexus-tools/<tool>:sha-<7>`
- Cargo cache via `buildkit-cache-dance` keyed on
  `hashFiles('offchain/Cargo.lock')`.

#### `.github/workflows/offchain-tools.prepare.yml`

`workflow_call` taking `target-ref`, `matrix-json`. One job per matrix
tool, then a single aggregator job:

Per tool:
1. Pull the just-pushed image.
2. `docker run --rm <image> <command> --meta` → `meta/<tool>.json`
   artifact (array of FQNs + schemas + description + timeout).
3. Render the per-tool Cloud Run config (`tools/<versioned>.json`)
   matching ava-game's schema.
4. GCP auth (workload identity) to infra project + protocol project.
5. Generate signed-HTTP keys using workbench's
   `generate-signed-http-keys` image with `app=nexus-tools`,
   `name=<versioned>`. Idempotency: if the secret exists and its FQN
   set matches the binary's `--meta`, skip; if FQNs changed, force regen.
6. Upload `tools/<versioned>.json` to
   `gs://<bucket>/<network>/offchain/tools/`. Preserves existing image
   tag pinning (only new versions adopt the build SHA).

Aggregator job:
- Concatenates all `meta/*.json` and the manifest of tool versions.
- Uploads content-hashed manifest to
  `gs://<bucket>/<network>/offchain/manifest/<hash>.json`.
- Uploads the bundled `offchain-tools-prepare` GitHub artifact for the
  register workflow.

#### `.github/workflows/offchain-tools.register.yml`

`workflow_call` taking `target-ref`. Single job (serialized for Sui
state). Steps mirror ava-game's register workflow:

1. Install Sui CLI (via `./.github/actions/install-sui`).
2. Download `offchain-tools-prepare` artifact.
3. GCP auth (protocol + infra projects, split for token lifetime).
4. Install `nexus` CLI from `nexus-sdk/shell:<NEXUS_TAG>` image.
5. Configure wallet from `SUI_DEPLOYER_MNEMONIC` and Nexus from
   `objects.<network>.toml`.
6. Load Nexus object IDs (`WORKFLOW_PKG`, `TOOL_REGISTRY`, `GAS_SERVICE`).
7. Register each FQN not already in `nexus tool list`:
   - PTB → `register_off_chain_tool` with
     `tool_url = http://<network>-<tool>-v<version>.tools.internal`.
   - Extract `OwnerCap<OverTool>` from `objectChanges`.
   - Save `{fqn, owner_cap_over_tool}` to
     `gs://<bucket>/<network>/offchain/registration/<tool>/<fqn>.json`.
8. Register signing keys via `nexus tool auth register-key`. Hash-gate
   to skip when key set is unchanged. Save `{tool_kid, signing_key_hash}`
   back to the GCS reg JSON.
9. Reconcile `nexus-tools-<versioned>-signed-http-toolkit-config`
   secret. Sole-writer rule: only this step writes it, because only
   this step knows `tool_kid`. Hash-gated convergence.

#### `.github/workflows/offchain-tools.readiness.yml`

PR-only merge gate. Matrix over changed tools. For each tool:

- Resolves network from `github.base_ref`.
- Computes current subtree version.
- Checks GCS for:
  - `gs://<bucket>/<network>/offchain/tools/<tool>-v<version>.json`
  - At least one FQN registration JSON in
    `gs://<bucket>/<network>/offchain/registration/<tool>/` for this
    version.
- Fails with a "run the publish workflow with PR #N" message if either
  is missing.

GCS is authoritative because the register workflow writes to GCS only
after successful on-chain registration. Chain state leading GCS by
design is impossible.

#### `.github/workflows/ci.yml`

Top-level orchestrator. Wires the above plus existing pre-commit /
coverage / audit workflows.

- Always runs: discover, lint/test (existing pre-commit / coverage /
  audit), docker build validation.
- Conditional on trigger (see Trigger matrix below): image push,
  prepare, register, readiness.
- `ci-gate` job is the single required status check for branch
  protection; succeeds iff every needed job passed or was legitimately
  skipped.

#### `.github/actions/`

New composite actions:
- `install-sui` — pinned channel + cache (lifted from ava-game).
- `install-nexus-cli` — extracts `nexus` from `nexus-sdk/shell` image.
- `gcp-auth-infra` / `gcp-auth-protocol` — split so token lifetimes
  don't collide.
- `retrigger-pr` — GPG-signed empty commit + push back to the PR branch
  after a dispatched run.

### Shared `offchain/Dockerfile`

Multi-stage, matching workbench's pattern:

1. **Builder** (Rust slim): installs `pkg-config`, `libssl-dev`. Build
   args `PACKAGE`, `BINARY`, `TOOL_FQN_VERSION`. Runs
   `cargo build --profile release --bin $BINARY -p $PACKAGE`. Cache
   mounts on `/usr/local/cargo/registry` + `/app/target` keyed
   `type=gha`. Copies `target/release/$BINARY` to `/app/`.
2. **lib-collector** (debian slim): `ldd`s the binary, collects runtime
   libs to `/collected/`. Same logic as workbench.
3. **distroless** (gcr.io/distroless/cc-debian12): final image. Copies
   binary + collected libs. `ENV TOOL_FQN_VERSION=$TOOL_FQN_VERSION` and
   `PORT=8080`.

Workbench's `Dockerfile` keeps working independently (it clones the
repo and builds out-of-tree). Long-term, workbench can switch to
pulling our pre-built images, but that's a follow-up.

### Trigger matrix

| Trigger                                            | discover | docker build | image push | prepare | register | readiness |
|----------------------------------------------------|:--------:|:------------:|:----------:|:-------:|:--------:|:---------:|
| PR with base `main`                                | ✓        | ✓ (no push)  | ✗          | ✗       | ✗        | ✗         |
| PR with base `testnet`/`mainnet` (incl. `promote/*`)| ✓        | ✓ (no push)  | ✗          | ✗       | ✗        | ✓         |
| `workflow_dispatch` with `pr-number`               | ✓        | ✓            | ✓          | ✓       | ✓        | —         |
| Push to `main`                                     | ✓        | ✓            | ✓          | ✗       | ✗        | ✗         |
| Push to `testnet`                                  | ✓        | ✓            | ✓          | ✓       | ✓        | ✗         |
| Push to `mainnet`                                  | ✓        | ✓            | ✓          | ✓       | ✓        | ✗         |

Notes:
- Push to `main` builds and pushes images (workbench can pull them) but
  does not run prepare/register — `main` has no deploy environment.
- A regular PR to `main` runs only validation (no push).
- A PR to `testnet` or `mainnet` is treated as a deploy request. The
  readiness gate is the merge blocker; the dispatched workflow is the
  only way to make readiness green. ava-game's promote/* convention is
  preserved as the branch-naming hint for such PRs.

### Promotion flow

1. Work lands on `main` via normal PRs.
2. To deploy to testnet: open `promote/<topic>` PR with base `testnet`,
   head `main` (or a snapshot of it). Readiness is red.
3. Reviewer / author runs the publish workflow with the PR number.
   Chain ops fire against the `testnet` environment. `retrigger-pr`
   pushes an empty signed commit. Readiness re-evaluates green.
4. Merge.
5. Repeat for `mainnet` with base `mainnet`, head `testnet`.

## Branch model

| Branch    | Role                       | GH environment | Deploys to | Chain ops on push? |
|-----------|----------------------------|----------------|------------|--------------------|
| `main`    | Development integration    | (none)         | nothing    | ✗ (builds + pushes images only) |
| `testnet` | Testnet deploy target      | `testnet`      | testnet    | ✓                  |
| `mainnet` | Mainnet deploy target      | `mainnet`      | mainnet    | ✓                  |

Environment selection expression in jobs that need an env:

```yaml
environment: >-
  ${{
    (inputs.target-ref || github.event_name == 'pull_request' && github.base_ref || github.ref_name) == 'mainnet' && 'mainnet' ||
    'testnet'
  }}
```

Chain jobs are gated by `github.base_ref != 'main' && github.ref_name != 'main'`
so anything resolving to `main` simply doesn't reach a job that requires
an environment.

## State backend

### GCS (per-environment bucket in `GCP_PROJECT_ID`)

| Path                                                                  | Contents                                                | Idempotency      |
|-----------------------------------------------------------------------|---------------------------------------------------------|------------------|
| `gs://<bucket>/<network>/offchain/tools/<tool>-v<version>.json`       | Cloud Run config for Terraform                          | Hash-keyed       |
| `gs://<bucket>/<network>/offchain/manifest/<content-hash>.json`       | Full prepare manifest                                   | Content-addressed |
| `gs://<bucket>/<network>/offchain/registration/<tool>/<fqn>.json`    | `{owner_cap_over_tool, tool_kid, signing_key_hash}`     | Keyed by FQN     |

### GCP Secret Manager (in `GCP_PROJECT_ID`)

| Secret name                                                            | Owner               | Contents                                |
|------------------------------------------------------------------------|---------------------|-----------------------------------------|
| `nexus-tools-<tool>-v<version>-signed-http-keys`                       | prepare workflow    | Signing keypair JSON                    |
| `nexus-tools-<tool>-v<version>-signed-http-toolkit-config`             | register workflow   | Toolkit config (kid-bound, sole writer) |

Content-addressed isolation: a given `<tool>-v<n>` secret pair is
forever-pinned to that content hash. New content ⇒ new version ⇒ new
secret. Old secrets stay valid until manually torn down.

### Per-environment `vars` and `secrets`

| Name                                       | Source   | Notes                                              |
|--------------------------------------------|----------|----------------------------------------------------|
| `GCP_PROJECT_ID` / `GCP_PROJECT_NUMBER`    | env vars | Owns GCS bucket + Secret Manager                   |
| `GCP_INFRA_PROJECT_ID` / `GCP_INFRA_PROJECT_NUMBER` | env vars | Owns GCR registry + nexus-sdk/shell image (shared across envs) |
| `SUI_NETWORK`                              | env vars | `testnet` or `mainnet`                             |
| `NEXT_PUBLIC_SUI_RPC_URL`                  | env vars | RPC endpoint                                       |
| `NEXUS_TAG`                                | env vars | Pin for `nexus-sdk/shell` image                    |
| `SUI_CHANNEL` / `SUI_CACHE_VERSION`        | env vars | Sui CLI install pins                               |
| `SUI_DEPLOYER_MNEMONIC`                    | secret   | Deployer wallet                                    |
| `GPG_DEVOPS_SIGNING_KEY`                   | secret   | For `retrigger-pr` signed empty commits            |

Values are filled in after the greenfield Terraform allocates them.

## Bootstrap

1. Land spec + scaffolded workflows on a `main` PR. CI runs in no-deploy
   mode, validates Dockerfile + build.rs across retrofitted tools.
2. First deploy ever: open `promote/initial-bootstrap` from `main` →
   `testnet`. Readiness is red (no GCS artifacts yet). Run workflow
   dispatch with the PR number. Chain seeds GCS. Readiness green. Merge.
3. Repeat for `mainnet` once that branch is cut.

## What this gives us

- New tool: drop `offchain/tools/<name>/{Cargo.toml, src/, tools.json,
  build.rs}`. Push. CI discovers it; promote PR deploys it. No workflow
  YAML edits.
- Tool content change: subtree hash changes → new version → new image
  tag → new Cloud Run service → new FQN registration. Old service stays
  registered. No flag flipping.
- Shared-file change (e.g. `Cargo.lock`, `Dockerfile`): all tools
  rebuild under the same content version (same FQN), images updated, no
  on-chain churn.
