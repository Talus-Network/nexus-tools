# Nexus Tools

[![Contributor Covenant](https://img.shields.io/badge/Contributor%20Covenant-2.1-4baaaa.svg)](CODE_OF_CONDUCT.md)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/Talus-Network/nexus-tools/blob/main/LICENSE)
[![Actions](https://img.shields.io/badge/GitHub_Actions-Active-brightgreen)](https://github.com/Talus-Network/nexus-tools/actions)

This is a collection of [tools] that the Talus core team maintains.

## Development

We use [just][just-repo], a straightforward command runner similar to `make`.

To explore the available tasks, run `just --list`.

## Layout

~~~text
nexus-tools/
├── offchain/          # Rust workspace + tools (cd here before cargo)
│   ├── Cargo.toml
│   ├── tools/
│   └── Dockerfile     # shared, parameterized
├── onchain/           # reserved for future Move tools
└── .github/workflows/ # CI: pre-commit, offchain tools build+publish+register
~~~

All cargo commands run from `offchain/`:

~~~bash
cd offchain
cargo build --workspace
cargo test --workspace
~~~

## Deploying a tool

Each tool ships `tools.json` + `build.rs` + a `[[bin]]` declaration. The
CI pipeline discovers tools by globbing `offchain/tools/*/tools.json` and:

1. Builds per-tool Docker images on every push, tagging
   `ghcr.io/<owner>/nexus-tools/<tool>:sha-<7>` and
   `gcr.io/<infra>/nexus-tools/<tool>:sha-<7>`.
1. On pushes to `testnet`/`mainnet` or a `workflow_dispatch` with a PR
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
Merge once green.

<!-- List of references -->

[tools]: https://docs.talus.network/talus-documentation/developer-docs/index/tool
[just-repo]: https://github.com/casey/just
