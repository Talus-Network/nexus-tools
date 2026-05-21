# Nexus Tools

[![Contributor Covenant](https://img.shields.io/badge/Contributor%20Covenant-2.1-4baaaa.svg)](CODE_OF_CONDUCT.md)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/Talus-Network/nexus-tools/blob/main/LICENSE)
[![Actions](https://img.shields.io/badge/GitHub_Actions-Active-brightgreen)](https://github.com/Talus-Network/nexus-tools/actions)

A collection of [tools] that the Talus core team maintains.

## Development

We use [just][just-repo] as a command runner. List tasks with `just --list`.

## Layout

~~~text
nexus-tools/
├── offchain/                # Rust workspace + tools (cd here before cargo)
│   ├── Cargo.toml
│   ├── tools/<tool>/
│   │   ├── Cargo.toml
│   │   ├── tools.json       # per-tool deploy config
│   │   ├── build.rs         # validates bin/tool name, threads TOOL_FQN_VERSION
│   │   └── src/...
│   └── Dockerfile           # shared, parameterized by PACKAGE/BINARY/TOOL_FQN_VERSION
├── onchain/                 # reserved for future Move tools
└── .github/
    ├── actions/             # composite actions (install-sui, install-nexus-cli, gcp-auth-*)
    └── workflows/           # CI: discover → deploy → prepare → register → trigger-tf-apply
~~~

All cargo commands run from `offchain/`:

~~~bash
cd offchain
cargo build --workspace
cargo test --workspace
~~~

## Adding a new tool

A "tool" is a single Rust crate under `offchain/tools/<name>/` with a few
conventions the CI pipeline relies on:

1. **`tools.json`** — declares the deploy shape. The crate's name is the
   key under `offchain/tools/`. Minimal example:

   ~~~json
   {
     "environment": { "RUST_LOG": "info" },
     "resources": { "cpu": "1", "memory": "512Mi" },
     "signed_http": { "enabled": true }
   }
   ~~~

1. **`build.rs`** — must compile-time validate that the crate name
   matches the binary and emit `TOOL_FQN_VERSION` from the Docker
   build-arg (defaults to `"1"` for local builds). Copy from an
   existing tool like `offchain/tools/math/build.rs`.

1. **`[[bin]]` in `Cargo.toml`** — binary name must equal the crate
   name (the build.rs assertion enforces this).

1. **`fqn!()` source threading** — every FQN literal in your tool must
   use `concat!()` + `env!("TOOL_FQN_VERSION")` so the content version
   flows from the build args into the registered FQN:

   ~~~rust
   fn fqn() -> ToolFqn {
       fqn!(concat!(
           "xyz.taluslabs.math.i64.add@",
           env!("TOOL_FQN_VERSION")
       ))
   }
   ~~~

1. **Optional per-route URL path** — the toolkit binary's `--meta`
   output reports a per-FQN URL like `http://localhost/i64/add`. The
   register step preserves that path (forcing a trailing slash) when
   composing the on-chain URL, so the leader can reach the right
   endpoint.

Once those four pieces are in place, the CI pipeline discovers the
tool automatically by globbing `offchain/tools/*/tools.json`.

## Branch model

| Branch | Trigger | What runs |
| --- | --- | --- |
| `main` | push | discover + dry-run deploy on changed matrix (no chain ops) |
| `iterate/testnet/<topic>` | push | full pipeline against **testnet** |
| `iterate/mainnet/<topic>` | push | full pipeline against **mainnet** |
| any | `workflow_dispatch` with `target-env=testnet\|mainnet` | full pipeline against the chosen env |
| any | `workflow_dispatch` with `pr-number=<N>` | full pipeline against the PR's base env |

`main` is dev integration — code lands here for review but the
deployer wallet is never touched. To actually deploy:

- **Short-lived feature deploy** — push to an `iterate/testnet/<topic>`
  branch. CI runs the full pipeline; the branch can be deleted once the
  rollout settles.
- **Manual one-off** — go to Actions → CI → Run workflow, pick the
  target env (or pass a PR number to deploy that PR's head).

## What "full pipeline" does

1. **discover** — globs `offchain/tools/*/tools.json` and computes a
   per-tool content version from `git rev-parse HEAD:offchain/tools/<name>/`
   (first 8 hex of sha256 → u32).
1. **deploy** — builds per-tool Docker images and pushes to
   `gcr.io/<infra-project>/nexus-tools/<tool>:sha-<7>`. Build and auth
   are split so token expiry can't kill a slow build mid-push.
1. **prepare** — runs each container's `--meta` to extract the
   ToolMeta JSON, renders a Cloud Run config blob to
   `gs://<bucket>/<network>/offchain/tools/<tool>-v<version>.json`,
   and generates a signed-HTTP keys secret in Secret Manager. The
   `nexus_contracts_tag` is baked from `vars.NEXUS_TAG` into each blob
   so it stays pinned to whichever Nexus version was current at
   prepare time.
1. **register** — for each FQN:
   - skips the CLI call if already on chain (snapshot from `nexus tool list --json`),
   - otherwise pipes the ToolMeta to `nexus tool register offchain --from-meta -`,
   - persists `owner_cap_over_tool` + `owner_cap_over_gas` to
     `gs://<bucket>/<network>/offchain/registration/<tool>/<fqn>.json`,
   - registers signing keys (`nexus tool auth register-key`),
   - reconciles the per-tool `toolkit-config` Secret Manager secret.
   Pre-step: consolidates the deployer wallet's coins so a single coin
   can cover the 1 SUI per-tx budget that heavy schemas require.
1. **trigger-tf-apply** — dispatches `Talus-Network/tf-nexus-tools`'s
   `terraform.yml` (via `the-actions-org/workflow-dispatch`, which
   works with fine-grained PATs). Terraform materializes the Cloud
   Run services, internal ALB, and DNS records based on the per-tool
   blobs in GCS. ci-gate inherits the dispatched run's conclusion, so
   register-then-apply is one atomic gate.

## Operational notes

- **Deployer wallet** — `SUI_DEPLOYER_PK` (env-scoped repo secret) is a
  `suiprivkey...` bech32 string. The register step imports it into the
  sui keychain at the start of the job. The same secret will eventually
  drive on-chain Move publishes for `onchain/` tools.
- **NEXUS_TAG** — env variable (testnet/mainnet). Each tool's deploy
  pins to whichever value was current at prepare time; later bumps to
  `NEXUS_TAG` only affect *new* deploys.
- **Stale Cloud Run services** — terraform iterates over whatever
  config blobs are in `gs://...offchain/tools/`. If a stale blob from
  a prior iteration is left over, terraform will try to create a
  service for it. Either re-prepare to refresh the blob or
  `gsutil rm` the stale one.
- **Costs** — internal ALB stack (URL map + proxy + per-region
  forwarding rules) is independent from `tf-talus-nexus-v2`. Future
  work may consolidate the two; for now they're kept separate so the
  static legacy deploy and the dynamic pipeline can't influence each
  other.

<!-- List of references -->

[tools]: https://docs.talus.network/talus-documentation/developer-docs/index/tool
[just-repo]: https://github.com/casey/just
