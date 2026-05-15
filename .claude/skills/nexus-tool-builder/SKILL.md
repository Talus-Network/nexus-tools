---
name: nexus-tool-builder
description: |-
  Use when the user wants to add a new Nexus Tool to the Talus-Network/nexus-tools
  repo (or any repo that depends on nexus-toolkit / nexus-sdk). Triggers include
  "create a Nexus tool for <API>", "wrap the <X> API as a Talus tool",
  "scaffold a Nexus tool", "add a tool crate for <service>", "automate Nexus
  tool creation", and "on-chain Nexus tool". Supports both off-chain (Rust +
  nexus-toolkit) and on-chain (Sui Move) tools, and emits GCP Cloud Run
  deployment scaffolding for testnet/mainnet. Skip for non-Talus / non-Nexus
  work.
---

# nexus-tool-builder

Automates the creation of a new Nexus Tool — off-chain Rust HTTP service or
on-chain Sui Move module — by reading an API spec, generating the canonical
Talus tool layout, verifying it builds, and emitting deploy artifacts for GCP
Cloud Run (off-chain) or `sui client publish` (on-chain).

Reference material lives in `reference/`; rendered file templates live in
`templates/`. Read the relevant references before generating code:

- `reference/architecture.md` — the canonical tool-crate layout (study before
  any off-chain tool).
- `reference/interface.md` — the `NexusTool` trait contract and `bootstrap!`
  semantics.
- `reference/style-guide.md` — port naming, error variants, flatness rules.
- `reference/onchain-tools.md` — Move-tool variant (read when `--kind
  on-chain`).
- `reference/hosting-options.md` — GCP vs DePIN, with a clear v1 default.
- `reference/security-checklist.md` — authoritative checklist used by the
  `nexus-tool-auditor` agent. Read it when generating code so you avoid
  findings up front.
- `reference/faq.md` — pitfalls (top-level `oneOf`, dedup paths, workspace).

## Pre-flight

1. Confirm the working repo is `nexus-tools` (or another repo that pulls
   `nexus-toolkit`). Read the workspace `Cargo.toml`; it should declare
   `nexus-toolkit` and `nexus-sdk`.
2. Confirm the `nexus` CLI is installed: `nexus --version`. If absent, point
   the user at the install instructions in `Talus-Network/nexus-sdk/cli`
   before continuing. Do not silently fall back to local-only templates —
   staying on the official scaffold keeps tools aligned with upstream.
3. Read `reference/architecture.md` and `reference/style-guide.md` (or
   `reference/onchain-tools.md` if the user requested an on-chain tool).

## Required inputs (collect via AskUserQuestion when ambiguous)

- **Kind:** `off-chain` (Rust, default) or `on-chain` (Move).
- **API name + canonical docs URL** (off-chain only).
- **Category prefix:** `exchanges` | `social` | `storage` | `llm` | `data` |
  `defi` | …
- **Service slug** (kebab-case). Crate name becomes `<category>-<service>`.
- **Auth model** (off-chain): `none` | `api_key` | `oauth2` | `signed`.
  Drives `client.rs`.
- **Endpoint set:** explicit list, or `discover-from-docs`.
- **FQN domain:** default `xyz.taluslabs`.

## Off-chain workflow

1. **Discover endpoints.** Use WebFetch on the docs URL (and any linked
   sub-pages). Produce a table of `{ name, http_method, path, query/body
   params, response schema, error shape }`. Echo it back to the user.
2. **Design ports per endpoint** honoring the style guide: `snake_case`
   names, error variants prefixed `err`, flat outputs, crucial ports
   non-optional, split prompt/context-style fields the way the DAG needs
   them.
3. **Scaffold via the official CLI:**

   ```sh
   bash .claude/skills/nexus-tool-builder/scripts/new_tool.sh \
     off-chain <category>-<service>
   ```

   The script wraps `nexus tool new --name <name> --template rust`, moves
   the generated crate into `tools/`, and renders the local templates over
   it. (Workspace `Cargo.toml` already declares `members = ["tools/*"]`, so
   the crate is picked up automatically.)
4. **Generate per-endpoint code** by rendering `templates/rust/endpoint.rs.tmpl`
   once per endpoint into `tools/<crate>/src/tools/<endpoint>.rs`. Each file
   contains `Input`, `Output` (enum with `Ok` / `Err`), an `impl NexusTool`,
   and a `mockito` test module. Add `pub(crate) mod <endpoint>;` to
   `src/tools/mod.rs`. Append the tool to the `bootstrap!([…])` list in
   `src/main.rs`.
5. **Generate `README.md`** with one section per FQN matching
   `tools/exchanges-coinbase/README.md`. Include the API docs URL.
6. **Register the crate in the build:** append the new package name to each
   `--package` line in `tools/.just` (recipes: `build`, `check`, `test`,
   `fmt-check`, `clippy`).
7. **Emit deploy scaffolding** into `tools/<crate>/deploy/` and
   `.github/workflows/`:
   - `Dockerfile` (multi-stage Rust → distroless, exposes 8080).
   - `cloud-run.testnet.yaml` and `cloud-run.mainnet.yaml` (service config
     per env, secret references for signed-HTTP keys, allowed-leaders
     ConfigMap).
   - `register.sh` (idempotent `nexus tool register` / update).
   - `.github/workflows/deploy-<crate>-testnet.yml` (push to `main`).
   - `.github/workflows/deploy-<crate>-mainnet.yml` (tag `v<crate>-*`,
     gated on testnet green).
8. **Verify** (stop on first failure):

   ```sh
   bash .claude/skills/nexus-tool-builder/scripts/verify.sh <crate>
   ```

   The script runs `cargo check`, `cargo clippy -- -D warnings`,
   `cargo test`, `cargo fmt --check` (nightly), and a `cargo run`
   + `/health` + `/meta` smoke test.

9. **Audit** by invoking the `nexus-tool-auditor` sub-agent:

   ```
   Agent({
     subagent_type: "nexus-tool-auditor",
     description: "Security + conformance audit of <crate>",
     prompt: "Audit tools/<crate>. kind=off-chain. severity_floor=low.
              remediation=report-only. Write AUDIT.md and report the
              CRITICAL/HIGH/MEDIUM counts plus your recommendation
              (ready-for-testnet | ready-for-mainnet | block)."
   })
   ```

   Refuse to mark the tool ready if any CRITICAL findings are open.
   For testnet, HIGH findings can be filed as follow-up issues.

10. **Hand off** with the new FQN(s), the path `tools/<crate>/`,
    `just tools run <crate>`, the deploy URLs the workflows will create,
    the path to `tools/<crate>/AUDIT.md`, and a reminder to set
    `NEXUS_TOOLKIT_CONFIG_PATH` and `signed_http.mode = "required"` before
    production. Reference `reference/hosting-options.md` if the user asks
    about alternatives to Cloud Run.

## On-chain workflow

1. **Scaffold:**

   ```sh
   bash .claude/skills/nexus-tool-builder/scripts/new_tool.sh \
     on-chain <service>
   ```

   Wraps `nexus tool new --name <service>_onchain --template move`.
2. **Generate** the Move module from
   `templates/move/sources/tool.move.tmpl` following
   `reference/onchain-tools.md`:
   - `execute(worksheet: &mut ProofOfUID, …, ctx: &mut TxContext) -> TaggedOutput`
   - `Output` enum with `Ok` / `Err` variants
   - witness object + `witness_id()` getter
3. **Test:** `sui move test`.
4. **Audit** by invoking the `nexus-tool-auditor` sub-agent with
   `kind=on-chain` — on-chain tools have the highest blast radius (witness
   bypass, missing authorization, gas grief). Refuse to print the publish
   commands if any CRITICAL findings are open:

   ```
   Agent({
     subagent_type: "nexus-tool-auditor",
     description: "On-chain security audit of <service>_onchain",
     prompt: "Audit <service>_onchain. kind=on-chain. severity_floor=low.
              remediation=report-only. Write AUDIT.md. Pay extra attention to
              witness stamping on every path, missing AdminCap checks on
              public funs that mutate state, and unbounded loops."
   })
   ```

5. **Print** (don't run) the publish + register commands for the user to
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
  generated tools. All config goes through `NEXUS_TOOLKIT_CONFIG_PATH`.
- **Don't read upstream API credentials from env vars, files, or any
  persistent store.** Credentials live in the `Input` struct ONLY,
  passed per request by the DAG / Leader. This matches the repo
  convention (see `tools/llm-openai-chat-completion`,
  `tools/social-twitter`). See checklist items C7–C11.
- **Don't make the tool stateful.** No `static`, `lazy_static`,
  `OnceLock`, `Mutex`, `RwLock`, `Cell`, `RefCell` accumulating
  per-request data. No on-disk writes outside `#[cfg(test)]`. The
  `nexus-toolkit` runtime calls `new()` per request — anything in the
  struct's fields must be cheap to construct and stateless (an
  `Arc<reqwest::Client>` is fine; a `Mutex<HashMap<_,_>>` is not).
  Checklist item C9.
- **Don't derive `Debug` on an `Input` that contains credential
  fields.** Hand-write a `Debug` impl that redacts them, or omit Debug
  entirely. Checklist item C8.
- **Don't put real API keys in README examples or example DAG JSON.**
  DAG `default_values` get committed to Sui permanently. Use
  placeholders like `$STRIPE_API_KEY`. Checklist item C11.

## Idempotency

If the user re-runs the skill against an existing crate, treat it as an
update: regenerate only the files the user explicitly named, never blow
away their `invoke()` logic. If unsure, diff before writing.
