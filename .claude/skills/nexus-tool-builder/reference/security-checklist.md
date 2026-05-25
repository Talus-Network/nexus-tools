# Security checklist (authoritative)

The `nexus-tool-auditor` agent uses this list as its source of truth. Items
are grouped by severity; the agent must report every applicable item in
its `AUDIT.md` output.

## Off-chain (Rust) — CRITICAL

| # | Check | Why it matters |
| --- | --- | --- |
| C1 | `Output` is a Rust `enum` (not a struct) with `#[serde(rename_all = "snake_case")]` | Nexus runtime rejects non-`oneOf` schemas. |
| C2 | No `unwrap()` / `expect()` / `panic!` on any path reachable from `invoke()` | Panics surface as opaque 500s; Nexus expects typed `Err` variants. |
| C3 | No `danger_accept_invalid_certs` or any TLS bypass | Leader-side TLS verification is part of the Nexus trust model. |
| C4 | No hardcoded API keys, tokens, or signing material in source or test fixtures | Source leaks become public on first push. |
| C5 | `Output::Err::reason` does NOT contain raw upstream bodies, request URLs with secrets, internal file paths, or stack frames | Errors are passed on-chain unredacted. |
| C6 | Tool does NOT call `std::process::exit`, `unsafe`, or spawn child processes | Breaks the toolkit's lifecycle assumptions. |
| C7 | Tool source MUST NOT read any upstream-API credential from env vars, disk, or any persistent store. Credentials live ONLY in `Input` struct fields. | Repo-wide convention; matches `tools/llm-openai-chat-completion`, `tools/social-twitter`. Reading credentials from env breaks multi-tenancy and concentrates secret material on the Tool host. |
| C8 | Tool MUST NOT log, print, format-debug, or include in `Output::Err::reason` any value of a credential field on `Input`. `Input` MUST NOT derive `Debug` automatically — if you need Debug, hand-write one that omits credential fields. | Logs end up in Cloud Logging where many people can read them. |
| C9 | Tool MUST be **stateless across invocations**. No `static`, `lazy_static`, `OnceLock`, `Mutex`, `RwLock`, `Cell`, `RefCell` carrying per-request data. No on-disk writes outside `#[cfg(test)]`. Same input ⇒ same upstream call. | `nexus-toolkit` calls `new()` per request; stateful tools produce non-deterministic outputs, break retries, and turn the Tool host into a trust point. Configuration constants (URLs, version strings) are fine; mutable shared state is not. |
| C10 | Tool MUST NOT mount upstream API keys as Cloud Run secrets. The only secrets bound to the Cloud Run service are `nexus-toolkit-config-<env>-<crate>` (Tool's own Ed25519 signing key) and `nexus-allowed-leaders-<env>` (public keys). | Custodying upstream keys on the Tool host violates the credential model in C7 and creates a juicy exfiltration target. |
| C11 | The crate's `README.md` and any example DAG JSON MUST NOT contain credential-shaped values (`sk_live_`, `sk_test_<64 chars>`, `Bearer <real-token>`). Use placeholder names like `$STRIPE_API_KEY`. | DAG `default_values` get written to Sui permanently. Real keys in READMEs leak via git history. |

## Off-chain — HIGH

| # | Check | Why it matters |
| --- | --- | --- |
| H1 | `Input` has `#[serde(deny_unknown_fields)]` | Stops silent acceptance of typos / injection of unknown control fields. |
| H2 | Every `NexusTool::path()` is unique within the crate | `bootstrap!` will mount duplicates at the same route; behavior is undefined. |
| H3 | `NexusTool::timeout()` is overridden to a value below the Leader request budget but above 2× expected upstream latency | Default 10s is too short for many real APIs and too long for cheap ones. |
| H4 | All `reqwest::Client` instances set a stable user-agent | Some APIs (Coinbase) reject empty user-agents intermittently. |
| H5 | `cargo audit` reports no known advisories | Public CVE; pre-existing. |
| H6 | API keys read via `std::env::var` are never logged with `tracing` / `log` / `println!` | Logs end up in Cloud Logging, accessible to anyone with project read. (Note: per C7 you shouldn't be reading them from env at all.) |
| H7 | Every credential-bearing `Input` field is named on the convention `api_key`, `bearer_token`, `*_secret`, `*_token`, so the auditor's name-pattern detector can find them. | The auditor can't tell a port is sensitive otherwise. |
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
| M5 | No `println!` / `eprintln!` / `dbg!` outside `#[cfg(test)]` code | Pollutes Cloud Logging; can leak data. |
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
| X1 | FQN version (`@1`) is bumped when output schema changes | Existing DAGs break otherwise. |
| X2 | `description` (Rust) / module-level docstrings (Move) accurately describe behavior | Misleading descriptions break DAG composition by humans and agents. |
| X3 | Tool is **idempotent** under retry — same input ⇒ same output, side-effect-free for read tools | The Leader retries on transient failures. |
| X4 | Tool does NOT trust the request body for authentication — only the signed-HTTP layer (off-chain) or the worksheet (on-chain) | Bypass otherwise. |

## How the auditor maps findings to severity

- **CRITICAL**: production-shipping the tool with this finding open exposes
  funds, secrets, or DAG correctness.
- **HIGH**: causes incorrect behavior or significantly weakens defense in
  depth. Block mainnet, allow testnet with a follow-up issue.
- **MEDIUM**: degraded UX, missing test, or non-blocking conformance gap.
  Allow testnet; fix before mainnet.
- **LOW / INFO**: style, polish, doc.
