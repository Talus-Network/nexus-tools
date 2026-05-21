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

## Branch & tag model

Two roles for branches, one for tags:

- **`main`** — dev integration. Tip of `main` is the source of truth
  for what code exists. Pushes to `main` build + push images to GCR but
  never touch the deployer wallet and never register FQNs on chain.
- **`iterate/testnet/<topic>` / `iterate/mainnet/<topic>`** — short-
  lived branches for iterating on the deploy pipeline itself. Push
  fires the full chain. Delete the branch once the rollout settles.
- **`testnet-*` / `mainnet-*` tags** — the canonical deploy path. Tag
  a commit on `main` (for testnet) or any prior testnet-deployed
  commit (for mainnet), push the tag, and CI runs the full chain
  against the corresponding env. Tags are immutable, so each
  deployment is attributable to an exact ref — no merge-strategy
  gotchas, no long-lived deploy branches to keep clean.

| Trigger | Matrix | Build | Push images | Chain ops | TF apply |
| --- | --- | --- | --- | --- | --- |
| PR (any base) | changed | ✓ | dry-run | — | — |
| Push to `main` | changed | ✓ | ✓ | — | — |
| Push to `iterate/testnet/**` / `iterate/mainnet/**` | all | ✓ | ✓ | ✓ (env) | ✓ |
| Push tag `testnet-*` / `mainnet-*` | all | ✓ | ✓ | ✓ (tag prefix) | ✓ |
| `workflow_dispatch` `target-env=testnet\|mainnet` | all | ✓ | ✓ | ✓ (chosen) | ✓ |
| `workflow_dispatch` `pr-number=<N>` | all | ✓ | ✓ | ✓ (PR's base) | ✓ |

## Lifecycle: PR → main → deploy

1. **Open a PR with base `main`.** CI runs the dry-run path: discover
   the changed-tool matrix, build each image, but don't push to the
   registry and don't touch the chain. Reviewers verify the build is
   clean and tests pass.
1. **Merge to `main`.** Same as the PR run, but images now get pushed
   to GCR (`gcr.io/<infra-project>/nexus-tools/<tool>:sha-<7>`). The
   image is "available" but no Cloud Run service points at it yet and
   no FQN is registered.
1. **Promote to a deploy env.** Pick one of:
   - **Tag deploy (canonical)** — tag a commit and push the tag:

     ~~~bash
     git tag testnet-v0.1.0 <commit-on-main>
     git push origin testnet-v0.1.0
     ~~~

     Tag prefix selects the env (`testnet-*` → testnet, `mainnet-*`
     → mainnet). The full chain runs against that exact commit, and
     the tag is an immutable record of what was deployed. Best for
     anything you'd want to roll back to or audit later.
   - **Iterate branch** — push to `iterate/testnet/<topic>` (or
     `iterate/mainnet/<topic>`). Fires the same pipeline. Best for
     fast iteration on the deploy itself (debugging the register
     step, tf-nexus-tools side, etc.) where you don't want a tag for
     every attempt.
   - **Manual one-off** — Actions → CI → "Run workflow". Pick
     `target-env=testnet|mainnet`, or pass `pr-number=<N>` to deploy a
     specific PR's head against the PR's base env. Best for ad-hoc
     deploys without cutting a branch or tag.

   Convention for mainnet promotions: tag should sit on a commit that
   has already been deployed to testnet (i.e. is reachable from a
   prior `testnet-*` tag). Not enforced in CI yet — keep it as a
   review discipline.

After the chosen trigger fires, the full pipeline runs: discover →
deploy → prepare → register → trigger-tf-apply. `ci-gate` only goes
green if the dispatched `tf-nexus-tools` terraform run also succeeded,
so a green pipeline means Cloud Run + ALB + DNS are reconciled and the
FQN URLs on chain point at the right endpoints.

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
