# Security checklist (authoritative)

The `nexus-tool-auditor` agent uses this list as its source of truth. Items
are grouped by severity; the agent must report every applicable item in
its `AUDIT.md` output.

## Off-chain (Rust) — CRITICAL

| # | Check | Why it matters |
| --- | --- | --- |
| C1 | **No credential-shaped fields in any `Input` struct.** Field names (case-insensitive) MUST NOT contain `api_key`, `apikey`, `secret`, `password`, `private_key`, `bearer`, `access_token`, `consumer_key`, `consumer_secret`, `client_secret`, end with `_token`, or be exactly `token` / `key`. The legitimate per-call exceptions are `idempotency_key`, `pagination_token`, `page_token`, `cursor_token`, `next_token`, `continuation_token`, `refresh_cursor`. | Tool inputs flow through the Nexus DAG as plaintext on Sui. Any credential-shaped field on `Input` is effectively published on-chain. Credentials MUST be sourced from the process environment at startup. Canonical pattern: `offchain/tools/memory-memwal/src/client.rs::from_env`. |
| C2 | `Output` is a Rust `enum` (not a struct) with `#[serde(rename_all = "snake_case")]` | Nexus runtime rejects non-`oneOf` schemas. |
| C3 | No `unwrap()` / `expect()` / `panic!` on any path reachable from `invoke()` | Panics surface as opaque 500s; Nexus expects typed `Err` variants. |
| C4 | No `danger_accept_invalid_certs` or any TLS bypass | Leader-side TLS verification is part of the Nexus trust model. |
| C5 | No hardcoded API keys, tokens, or signing material in source or test fixtures | Source leaks become public on first push. |
| C6 | `Output::Err::reason` does NOT contain raw upstream bodies, request URLs with secrets, internal file paths, or stack frames | Errors are passed on-chain unredacted. |
| C7 | Tool source MUST read upstream-API credentials from environment variables at startup, via a `zeroize::Zeroizing<String>` wrapper, validated once in `main` before `bootstrap!`. The env var name follows `<SERVICE>_API_KEY` (or `<SERVICE>_*` for multi-secret services). | Per-request credentials on `Input` violate C1 (on-chain leak). The startup-env model centralizes the trust boundary on the Cloud Run service identity (Secret Manager `secretKeyRef` + IAM), matches `offchain/tools/memory-memwal`, `offchain/tools/storage-walrus`, and gives a single redactable log site. |
| C8 | Tool MUST NOT log, print, format-debug, or include in `Output::Err::reason` any value of a credential. The struct holding the credential MUST hand-implement `Debug` to print `<redacted>` (never `#[derive(Debug)]`). | Logs end up in Cloud Logging where many people can read them. Heap and stack copies of the secret are wiped via `Zeroizing`. |
| C9 | Tool MUST be **stateless across invocations** w.r.t. request data. The credential / HTTP-client / signing-key fields constructed once at startup and shared via `Arc` are fine and required. No mutable per-request state in `static`, `lazy_static`, `OnceLock`, `Mutex`, `RwLock`, `Cell`, `RefCell`. No on-disk writes outside `#[cfg(test)]`. Same input ⇒ same upstream call. | `nexus-toolkit` calls `new()` per request; mutable shared request state produces non-deterministic outputs, breaks retries, and turns the Tool host into a trust point. |
| C10 | The tool's `tools.json` `environment` block MUST NOT contain upstream API keys, tokens, or any secret. It is rendered into the Cloud Run service's `env` block at deploy time and is visible to anyone with project read. Real secrets are mounted via Cloud Run `secretKeyRef` from GCP Secret Manager, configured out-of-band by the operator (the deploy pipeline does not provision them). | `environment` is for non-secret runtime config (e.g. `RUST_LOG=info`). Putting an API key here makes it readable from the Cloud Run service description, defeating the secret-mount model. |
| C11 | The crate's `README.md` and any example DAG JSON MUST NOT contain credential-shaped values (`sk_live_`, `sk_test_<64 chars>`, `Bearer <real-token>`). Use placeholder names like `$STRIPE_API_KEY`. | DAG `default_values` get written to Sui permanently. Real keys in READMEs leak via git history. |

## Off-chain — HIGH

| # | Check | Why it matters |
| --- | --- | --- |
| H1 | `Input` has `#[serde(deny_unknown_fields)]` | Stops silent acceptance of typos / injection of unknown control fields. |
| H2 | Every `NexusTool::path()` is unique within the crate | `bootstrap!` will mount duplicates at the same route; behavior is undefined. |
| H3 | `NexusTool::timeout()` is overridden to a value below the Leader request budget but above 2× expected upstream latency | Default 10s is too short for many real APIs and too long for cheap ones. |
| H4 | All `reqwest::Client` instances set a stable user-agent | Some APIs (Coinbase) reject empty user-agents intermittently. |
| H5 | `cargo audit` reports no known advisories | Public CVE; pre-existing. |
| H6 | Startup-time env-var reads of secrets are NOT logged with `tracing` / `log` / `println!`. The classification log line (e.g. "STRIPE_API_KEY is not set") is fine; the value itself never appears. | Logs end up in Cloud Logging, accessible to anyone with project read. |
| H7 | The crate's `tools.json` has `tool_name`, `command`, and at minimum `environment.RUST_LOG`. `tool_name` and `command` MUST match `[[bin]].name` in `Cargo.toml` (the shared `build.rs` enforces this at compile time). | The shared discover/prepare workflows key off these fields; missing or mismatched values silently exclude the tool from CI deploys. |
| H8 | Write endpoints (POST/PUT/PATCH/DELETE) include `idempotency_key: Option<String>` in `Input` and pass it through as the `Idempotency-Key` header. | Leaders retry on transient failures; without this, retries double-charge / double-write. |
| H9 | For Stripe-style APIs: tests / fixtures use `sk_test_` prefixed keys only. No `sk_live_`, `pk_live_`, or other production-prefix tokens anywhere in source. | Bricks of fines and reputation damage if a real live key lands in git. |
| H10 | Stripe-style tools cover every documented error class in `from_api_error_type`: `card_error`, `validation_error`, `invalid_request_error`, `idempotency_error`, `rate_limit_error`, `authentication_error`, `api_error`. | Unmapped error types degrade to `Unknown`, hiding the actual failure mode from DAG authors. |

## Off-chain — MEDIUM

| # | Check | Why it matters |
| --- | --- | --- |
| M1 | `NexusTool::description()` is non-empty | `/meta` exposes this to DAG authors. |
| M2 | Every endpoint has a `mockito`-backed happy-path test | Regressions caught at PR time. |
| M3 | Every endpoint has at least one error-variant test | Same. |
| M4 | Crucial response fields are non-optional in `Output::Ok` | Forces `Err` variant when upstream omits required data. |
| M5 | No `println!` / `eprintln!` / `dbg!` outside `#[cfg(test)]` code | Pollutes Cloud Logging; can leak data. Use `log::*` instead. |
| M6 | Workspace dep versions (`workspace = true`) — no per-crate version drift | Hard-to-trace upgrade pain. |

## Off-chain — LOW / INFO

| # | Check | Why it matters |
| --- | --- | --- |
| L1 | Doc comments on every `Input` / `Output` field — they show up in `input_schema` / `output_schema` | DAG authors need this. |
| L2 | `Cargo.toml` `description` is set | Helps `cargo metadata` consumers. |
| L3 | README has one section per FQN | Convention; consumed by tooling. |

## On-chain (Move) — CRITICAL

| # | Check | Why it matters |
| --- | --- | --- |
| C-M1 | `execute` first parameter is `worksheet: &mut ProofOfUID`, last is `ctx: &mut TxContext`, returns `TaggedOutput` | Runtime enforces this signature. |
| C-M2 | `worksheet.stamp_with_data(&witness.id, …)` is called on **every** code path before `execute` returns | Without it the Nexus runtime rejects the proof. Easy to miss in early-return branches. |
| C-M3 | Witness struct does NOT have the `copy` ability | A copyable witness lets anyone forge proofs. |
| C-M4 | Public functions that mutate shared state require an explicit capability (e.g. `AdminCap`) | Anyone can call `public fun` on Sui — without a cap, anyone can drain or alter the tool. |
| C-M5 | No `public entry fun` that calls `transfer::public_transfer(state, recipient)` on the tool's shared state | Lets a random caller hijack the tool. |
| C-M6 | `init` mints any `AdminCap` and transfers it to the publisher (`tx_context::sender(ctx)`) | Otherwise the cap is unminted or stuck in the package. |

## On-chain — HIGH

| # | Check | Why it matters |
| --- | --- | --- |
| H-M1 | Output enum has at least one `err`-prefixed variant | Nexus treats them as error variants. |
| H-M2 | TaggedOutput field types use the right `as_*()` (number/string/bool/address/raw) | Wrong typing breaks downstream tools at the schema layer, after the Move call succeeded. |
| H-M3 | No unbounded loops over caller-controlled `vector<T>` or dynamic field reads | Gas grief / out-of-budget aborts. |
| H-M4 | Arithmetic that may overflow uses `checked_*` (or relies on Move's abort behavior deliberately, with a comment) | Silent abort is fine only when documented. |
| H-M5 | `Move.toml` upgrade policy is explicit (`compatible` or `immutable`) | Default is `compatible`; for production-critical tools, switch to `immutable` after stabilization. |
| H-M6 | `sui move test` covers every error variant | Regression net. |

## On-chain — MEDIUM

| # | Check | Why it matters |
| --- | --- | --- |
| M-M1 | No `friend` modules outside this package | `friend` widens access; reviewers need to follow the trust path. |
| M-M2 | Every shared object is initialized in `init` and not re-creatable | Drift between the published state and the registered witness id breaks Nexus. |
| M-M3 | Test for the witness-stamping flow (positive assertion that the worksheet has the expected stamp after `execute`) | Catches silent misuse of `stamp_with_data`. |

## On-chain — LOW / INFO

| # | Check | Why it matters |
| --- | --- | --- |
| L-M1 | Doc comments on `execute` and on every `Output` variant | Schema generation reads them. |
| L-M2 | Module path matches the FQN convention `<domain>.<category>.<name>@<version>` | Convention; helps discovery. |

## Cross-cutting (both kinds)

| # | Check | Why it matters |
| --- | --- | --- |
| X1 | FQN version (`@N`) is bumped when output schema changes. For off-chain tools, the version comes from `env!("TOOL_FQN_VERSION")` (CI computes it from the tool's subtree git hash), so any source change auto-bumps the version on the next deploy. | Existing DAGs break otherwise. |
| X2 | `description` (Rust) / module-level docstrings (Move) accurately describe behavior | Misleading descriptions break DAG composition by humans and agents. |
| X3 | Tool is **idempotent** under retry — same input ⇒ same output, side-effect-free for read tools | The Leader retries on transient failures. |
| X4 | Tool does NOT trust the request body for authentication — only the signed-HTTP layer (off-chain) or the worksheet (on-chain) | Bypass otherwise. |

## How the auditor maps findings to severity

- **CRITICAL**: production-shipping the tool with this finding open exposes
  funds, secrets, or DAG correctness. **C1 (credential in Input) is always
  CRITICAL and always blocks `ready-for-testnet`** — there is no
  testnet-but-not-mainnet middle ground for an on-chain credential leak.
- **HIGH**: causes incorrect behavior or significantly weakens defense in
  depth. Block mainnet, allow testnet with a follow-up issue.
- **MEDIUM**: degraded UX, missing test, or non-blocking conformance gap.
  Allow testnet; fix before mainnet.
- **LOW / INFO**: style, polish, doc.
