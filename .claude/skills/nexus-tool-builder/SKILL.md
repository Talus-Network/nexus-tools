---
name: nexus-tool-builder
description: |-
  Use when the user wants to add a new Nexus Tool to the Talus-Network/nexus-tools
  repo (or any repo that depends on nexus-toolkit / nexus-sdk). Triggers include
  "create a Nexus tool for <API>", "wrap the <X> API as a Talus tool",
  "scaffold a Nexus tool", "add a tool crate for <service>", "automate Nexus
  tool creation", and "on-chain Nexus tool". Supports both off-chain (Rust +
  nexus-toolkit) and on-chain (Sui Move) tools. Off-chain tools deploy via the
  shared offchain-tools.* reusable workflows; no per-tool deploy plumbing is
  emitted. Skip for non-Talus / non-Nexus work.
---

# nexus-tool-builder

Automates the creation of a new Nexus Tool — off-chain Rust HTTP service or
on-chain Sui Move module — by reading an API spec, generating the canonical
Talus tool layout, and verifying it builds. Off-chain tools live under
`offchain/tools/<crate>/`; the repo's shared CI pipeline
(`.github/workflows/offchain-tools.{discover,prepare,deploy,register,readiness}.yml`)
discovers them automatically via each tool's `tools.json`. The skill does
NOT emit per-tool Dockerfiles, Cloud Run YAMLs, GitHub workflows, or
`register.sh` — that plumbing is shared and lives outside any single tool.

Reference material lives in `reference/`; rendered file templates live in
`templates/`. Read the relevant references before generating code:

- `reference/architecture.md` — the canonical tool-crate layout (study before
  any off-chain tool).
- `reference/interface.md` — the `NexusTool` trait contract and `bootstrap!`
  semantics.
- `reference/style-guide.md` — port naming, error variants, flatness rules.
- `reference/onchain-tools.md` — Move-tool variant (read when `--kind
  on-chain`).
- `reference/hosting-options.md` — where deployed tools run.
- `reference/security-checklist.md` — authoritative checklist used by the
  `nexus-tool-auditor` agent. Read it when generating code so you avoid
  findings up front. **C1 is "no credential-shaped fields in any `Input`
  struct" — it gates `ready-for-testnet`.**
- `reference/faq.md` — pitfalls (top-level `oneOf`, dedup paths, `tools.json`,
  `BLOCKED_TOOLS`).

## Pre-flight

1. Confirm the working repo is `nexus-tools` (or another repo that pulls
   `nexus-toolkit`). Read the workspace `Cargo.toml` at `offchain/Cargo.toml`;
   it should declare `nexus-toolkit` and `nexus-sdk`.
1. Confirm the `nexus` CLI is installed: `nexus --version`. If absent, point
   the user at the install instructions in `Talus-Network/nexus-sdk/cli`
   before continuing. Do not silently fall back to local-only templates —
   staying on the official scaffold keeps tools aligned with upstream.
1. Read `reference/architecture.md` and `reference/style-guide.md` (or
   `reference/onchain-tools.md` if the user requested an on-chain tool).

## Required inputs (collect via AskUserQuestion when ambiguous)

- **Kind:** `off-chain` (Rust, default) or `on-chain` (Move).
- **API name + canonical docs URL** (off-chain only).
- **Category prefix:** `exchanges` | `social` | `storage` | `llm` | `data` |
  `defi` | `payments` | `memory` | …
- **Service slug** (kebab-case). Crate name becomes `<category>-<service>`.
- **Auth model** (off-chain): `none` | `api_key` | `oauth2` | `signed`.
  Drives `client.rs` env-var name (e.g. `STRIPE_API_KEY`, `OPENAI_API_KEY`).
- **Endpoint set:** explicit list, or `discover-from-docs`.
- **FQN domain:** default `xyz.taluslabs`.

## Off-chain workflow

1. **Discover endpoints.** Use WebFetch on the docs URL (and any linked
   sub-pages). Produce a table of `{ name, http_method, path, query/body
   params, response schema, error shape }`. Echo it back to the user.
1. **Design ports per endpoint** honoring the style guide: `snake_case`
   names, error variants prefixed `err`, flat outputs, crucial ports
   non-optional, split prompt/context-style fields the way the DAG needs
   them. **Credentials are never ports** — they come from the process
   environment (see step 4 and `reference/security-checklist.md` §C1).
1. **Scaffold via the official CLI:**

   ```sh
   bash .claude/skills/nexus-tool-builder/scripts/new_tool.sh \
     off-chain <category>-<service>
   ```

   The script wraps `nexus tool new --name <name> --template rust`, moves
   the generated crate into `offchain/tools/`, and renders the local
   templates over it. The crate is picked up automatically by the workspace
   (`offchain/Cargo.toml` declares `members = ["tools/*"]`) and by the
   discover workflow (which scans `offchain/tools/*/tools.json`).
1. **Generate per-endpoint code** by rendering `templates/rust/endpoint.rs.tmpl`
   once per endpoint into `offchain/tools/<crate>/src/tools/<endpoint>.rs`.
   Each file contains `Input`, `Output` (enum with `Ok` / `Err`), an
   `impl NexusTool`, and a `mockito` test module. Add `pub(crate) mod
   <endpoint>;` to `src/tools/mod.rs`. Append the tool to the
   `bootstrap!([…])` list in `src/main.rs`.

   **Credential pattern (mandatory):** the rendered `<service>_client.rs`
   reads `<SERVICE>_API_KEY` from the process environment once at startup
   via `from_env()`, wraps the value in `zeroize::Zeroizing<String>`, and
   hand-implements `Debug` to print `<redacted>`. **Never put `api_key`,
   `bearer_token`, `*_secret`, or any credential-shaped field on `Input`.**
   Tool inputs are committed to the Nexus DAG on Sui as plaintext — anything
   you put there is effectively published. Canonical reference:
   `offchain/tools/memory-memwal/src/client.rs`.
1. **Generate `README.md`** with one section per FQN. Include the API docs
   URL. Document the required `<SERVICE>_API_KEY` env var; do NOT list it as
   an Input port. Use placeholders like `$STRIPE_API_KEY` in any example
   payload.
1. **Add `tools.json` and `build.rs`.** These are required by the shared
   pipeline:

   - `tools.json` — `{ "tool_name": "<crate>", "command": "<crate>",
     "environment": { "RUST_LOG": "info" } }`. **Do NOT list secret env vars
     here** — `environment` flows into Cloud Run's `env` block at deploy time
     and is visible to anyone with project read. Secrets are mounted via
     Cloud Run `secretKeyRef` from GCP Secret Manager, configured by the
     operator out-of-band.
   - `build.rs` — verbatim copy of `offchain/tools/memory-memwal/build.rs`.
     It validates `[[bin]].name == tools.json.command` and threads
     `TOOL_FQN_VERSION` from CI's Docker build-arg into the binary via
     `env!("TOOL_FQN_VERSION")`. The scaffold script copies it in for you.
1. **Verify** (stop on first failure):

   ```sh
   bash .claude/skills/nexus-tool-builder/scripts/verify.sh <crate>
   ```

   The script runs `cargo check`, `cargo clippy -- -D warnings`,
   `cargo test`, `cargo fmt --check` (nightly), and a `cargo run` plus
   `/health` and `/meta` smoke test. It boots the binary with
   `<SERVICE>_API_KEY=sk_test_FAKE` so `validate_credentials_at_startup`
   passes.

1. **Audit** by invoking the `nexus-tool-auditor` sub-agent:

   ```text
   Agent({
     subagent_type: "nexus-tool-auditor",
     description: "Security + conformance audit of <crate>",
     prompt: "Audit offchain/tools/<crate>. kind=off-chain. severity_floor=low.
              remediation=report-only. Write AUDIT.md and report the
              CRITICAL/HIGH/MEDIUM counts plus your recommendation
              (ready-for-testnet | ready-for-mainnet | block)."
   })
   ```

   Refuse to mark the tool ready if any CRITICAL findings are open.
   For testnet, HIGH findings can be filed as follow-up issues. **A
   credential-shaped field in any `Input` struct is always CRITICAL — the
   auditor will not promote past `block` until it's removed.**

1. **Hand off** with the new FQN(s), the path `offchain/tools/<crate>/`,
    the path to `offchain/tools/<crate>/AUDIT.md`, and a reminder that
    Cloud Run deployment is automatic once `tools.json` is present (the
    `offchain-tools.discover.yml` workflow picks it up on the next push).
    Tell the user the operator must mount `<SERVICE>_API_KEY` via Cloud Run
    `secretKeyRef` before the tool will pass `/health`. If the user is the
    operator, reference the `BLOCKED_TOOLS` repo variable in
    `reference/faq.md` as the emergency kill switch.

## On-chain workflow

1. **Scaffold:**

   ```sh
   bash .claude/skills/nexus-tool-builder/scripts/new_tool.sh \
     on-chain <service>
   ```

   Wraps `nexus tool new --name <service>_onchain --template move`.
1. **Generate** the Move module from
   `templates/move/sources/tool.move.tmpl` following
   `reference/onchain-tools.md`:
   - `execute(worksheet: &mut ProofOfUID, …, ctx: &mut TxContext) -> TaggedOutput`
   - `Output` enum with `Ok` / `Err` variants
   - witness object + `witness_id()` getter
1. **Test:** `sui move test`.
1. **Audit** by invoking the `nexus-tool-auditor` sub-agent with
   `kind=on-chain` — on-chain tools have the highest blast radius (witness
   bypass, missing authorization, gas grief). Refuse to print the publish
   commands if any CRITICAL findings are open:

   ```text
   Agent({
     subagent_type: "nexus-tool-auditor",
     description: "On-chain security audit of <service>_onchain",
     prompt: "Audit <service>_onchain. kind=on-chain. severity_floor=low.
              remediation=report-only. Write AUDIT.md. Pay extra attention to
              witness stamping on every path, missing AdminCap checks on
              public funs that mutate state, and unbounded loops."
   })
   ```

1. **Print** (don't run) the publish + register commands for the user to
   execute themselves with their own keys:

   ```sh
   sui client publish --gas-budget 200000000 --json
   nexus tool register onchain \
     --module-path "$PACKAGE_ID::<service>_onchain" \
     --tool-fqn "<domain>.<service>@1" \
     --description "<one-liner>" \
     --witness-id "0x..."
   ```

## Hard rules — never do these

- **Don't put credential-shaped fields in `Input`.** No `api_key`,
  `bearer_token`, `*_secret`, `*_token`, `password`, `private_key`,
  `access_token`, `consumer_secret`, `client_secret`. Tool inputs flow
  through the Nexus DAG as plaintext on Sui — anything on `Input` is
  effectively published. Credentials live in the process environment, read
  once at startup. Checklist item **C1**.
- **Read upstream API credentials from env vars at startup, never from
  `Input`.** Wrap the value in `zeroize::Zeroizing<String>`. Hand-write
  `Debug` to print `<redacted>`. Validate the env var once in `main`
  before `bootstrap!`. Pattern: `offchain/tools/memory-memwal/src/client.rs`.
  Checklist items **C7**, **C8**.
- **Don't make `Output` a struct.** It must be an enum so the JSON schema
  emits a top-level `oneOf`. The toolkit and CLI both enforce this; getting
  it wrong fails at runtime.
- **Don't merge prompt + context** (or other DAG-paired ports) into one
  input.
- **Don't put crucial response fields behind `Option<T>`** — return `err`
  instead when data is missing.
- **Don't use camelCase or PascalCase** for port names.
- **Don't hardcode a single API call.** Tools must be generic over the API
  surface — one tool per endpoint, parameterized.
- **Don't write to `~/.nexus`, `~/.config`, or any user-global path** from
  generated tools. The only on-disk state allowed is the optional
  `<cwd>/.env` read once at startup by `dotenvy::from_path` (existing
  exports always win).
- **Don't make the tool stateful.** No `static`, `lazy_static`,
  `OnceLock` (except for shared HTTP clients), `Mutex`, `RwLock`, `Cell`,
  `RefCell` accumulating per-request data. The `nexus-toolkit` runtime
  calls `new()` per request — anything in the struct's fields must be
  cheap to construct and stateless. An `Arc<reqwest::Client>` is fine; a
  `Mutex<HashMap<_,_>>` is not. Checklist item **C9**.
- **Don't list upstream API keys in `tools.json`'s `environment` block.**
  That block becomes Cloud Run's `env`, which is visible to anyone with
  project read. Secrets go through Cloud Run `secretKeyRef` from GCP Secret
  Manager (operator-configured). Checklist item **C10**.
- **Don't put real API keys in README examples or example DAG JSON.**
  DAG `default_values` get committed to Sui permanently. Use
  placeholders like `$STRIPE_API_KEY`. Checklist item **C11**.

## Idempotency

If the user re-runs the skill against an existing crate, treat it as an
update: regenerate only the files the user explicitly named, never blow
away their `invoke()` logic. If unsure, diff before writing.
