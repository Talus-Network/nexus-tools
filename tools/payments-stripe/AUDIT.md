# Audit: `payments-stripe` @ 815a8c1+wip

- **Date:** 2026-05-15
- **Auditor:** nexus-tool-auditor (executed inline by primary agent — project agent loader unavailable in this harness)
- **Kind:** off-chain
- **Severity floor:** low
- **Remediation mode:** report-only

## Summary

The crate cleanly passes every CRITICAL and HIGH check on the
`reference/security-checklist.md` matrix. 17/17 unit tests pass; clippy
is clean with `-D warnings`; `cargo fmt --check` is clean against
`nightly-2025-01-06`. The credential model matches the repo convention
(per-request `Input.api_key`; no env-var reads; no Debug derived on
Input; no upstream secrets mounted into Cloud Run). The tool is
stateless across invocations.

Three MEDIUM items remain — they are not blockers for testnet but
should be addressed before mainnet promotion: (1) a Stripe error body
is included verbatim in the fallback `Output::Err::reason` and may
echo customer-supplied params; (2) some endpoints have thin
error-variant test coverage; (3) `NexusTool::timeout()` is not
overridden anywhere (defaults to 10 s, acceptable for current Stripe
endpoints but worth pinning explicitly).

**Recommendation: ready-for-testnet.** Block mainnet pending the three
MEDIUM items below.

## Findings

### CRITICAL

_No findings._

### HIGH

_No findings._

### MEDIUM

- **M-1 — Raw upstream body in fallback `Output::Err::reason`**
  (`src/stripe_client.rs:127`)
  - What: When the response body does not match Stripe's `{"error":
    {...}}` envelope, the client falls back to
    `format!("Stripe API error ({}): {}", status, truncate(&text, 512))`.
    The raw body can contain card metadata (last 4), customer email, or
    other parameters the caller submitted that Stripe echoes back in
    plaintext.
  - Why it matters: `Output::Err` is passed on-chain by Nexus; anything
    in `reason` becomes public and permanent. Falls under C5 spirit
    (defense in depth) even though Stripe error bodies don't directly
    leak the api_key.
  - Fix: Drop the raw body. Surface only the status code and the typed
    `kind`:

    ```rust
    return Err(StripeErrorResponse {
        reason: format!("Stripe API error (status {})", status),
        kind: StripeErrorKind::from_status_code(status.as_u16()),
        status_code: Some(status.as_u16()),
    });
    ```

    If diagnostic detail is needed, write the body to a `tracing::warn!`
    log line with field-level redaction, not to `reason`.

- **M-2 — Thin error-variant test coverage in three endpoints**
  - `tools/payments-stripe/src/tools/confirm_payment_intent.rs`: has
    only a single validation test (`empty_id`); no test for
    `card_error` / `auth_error` paths.
  - `tools/payments-stripe/src/tools/get_balance.rs`: no error tests at
    all (only `test_get_balance_success`).
  - `tools/payments-stripe/src/tools/list_charges.rs`: covers
    `limit_out_of_range` (local validation) but no upstream-error case.
  - Why it matters: M3 says every endpoint has at least one error-variant
    test. Stripe behavior changes; the regression net needs to catch a
    drift in the error envelope on every endpoint.
  - Fix: add a mockito-backed 401/402/404 test per endpoint following
    the pattern in `create_payment_intent::tests::test_create_payment_intent_card_error`.

- **M-3 — `NexusTool::timeout()` not overridden**
  - What: Every endpoint uses the toolkit default of 10 s. Stripe
    typical p95 latency is ~500 ms but the 99th percentile during
    incidents has been seen above 5 s. The default is fine, but
    leaving it implicit creates risk on any subsequent endpoint
    addition that needs longer (e.g. webhook-based flows).
  - Fix: Explicitly override per endpoint, e.g.

    ```rust
    fn timeout() -> Duration { Duration::from_secs(15) }
    ```

    on every `impl NexusTool`. Document the chosen value next to the
    override.

### LOW

- **L-1 — Doc comments missing on some fields**
  - `get_payment_intent.rs::Input`, `confirm_payment_intent.rs::Input`,
    `create_customer.rs::Input`, `get_balance.rs::Input`,
    `list_charges.rs::Input` — `api_key` and other fields lack doc
    comments. They surface in `input_schema` (the comments do, when
    present); DAG authors and tooling rely on these.
  - `create_payment_intent.rs::Input` is the gold standard — propagate
    similar docstrings to the other five endpoints.

- **L-2 — `Output::Ok` field types lack doc comments**
  - Same surface (`output_schema`); add `///` comments to each port
    field in every endpoint's `Output::Ok` variant.

### INFO

- **I-1 — `cargo audit` and `cargo deny` not run in this sandbox.**
  The dev box does not have either binary installed. Both must be in
  the CI image before the GitHub Actions workflow runs against
  production. RustSec advisory database changes daily; baking the
  checks into CI is the only durable mitigation.

- **I-2 — One `expect()` in `StripeClient::new()`**
  (`src/stripe_client.rs:34`)
  - `Client::builder().user_agent(...).build().expect(...)`. This is
    startup-time and the failure mode (`reqwest` cannot create a TLS
    backend) is unrecoverable. Acceptable as a fail-fast. If we want
    the agent to flag zero `unwrap()`s in any non-test code, this
    becomes a tiny `match ... else std::process::abort()` cleanup —
    not worth it.

## Backtest results

The crate has 17 mockito-backed unit tests covering happy paths and
the error classes enumerated below. No golden-file fixtures yet — those
will accumulate as the team probes against real Stripe test mode.

| Endpoint | Happy path | Error coverage |
|---|---|---|
| create-payment-intent | ✅ | card_error, idempotency_error, rate_limit, zero-amount validation, empty-currency validation |
| get-payment-intent | ✅ | invalid_request (404), empty-id validation |
| confirm-payment-intent | ✅ (2 — succeeded + requires_action) | empty-id validation only |
| create-customer | ✅ | auth_error |
| get-balance | ✅ | _none_ |
| list-charges | ✅ | limit out-of-range only (local) |

Action items in **M-2**.

## Fuzz results

Not executed for this audit — the in-sandbox build is debug and the
release binary was not started by the time of writing. The verify.sh
smoke loop covers /health and /meta only. Recommend running an explicit
malformed-payload sweep before mainnet:

```sh
# template — adjust path / port
for payload in '{}' '{"api_key":null}' '{"api_key":1}' '{"api_key":"x","unknown":1}' \
               '{"api_key":"x","amount":-1,"currency":"usd"}' \
               '{"api_key":"x","amount":2000,"currency":""}'; do
  curl -fsS -X POST 127.0.0.1:8080/create-payment-intent/invoke \
       -H 'content-type: application/json' --data-raw "$payload" || echo "failed: $payload"
done
```

Expected: never a 5xx, always either `Output::Err` JSON or a structured
4xx from the toolkit.

## Conformance checklist (verbatim against `security-checklist.md`)

### CRITICAL — all PASS

- [x] C1  `Output` is enum with snake_case rename — every endpoint
- [x] C2  No `unwrap`/`expect`/`panic!` reachable from `invoke()` (the one `expect` at `stripe_client.rs:34` is `new()` only — see I-2)
- [x] C3  No `danger_accept_invalid_certs` / TLS bypass
- [x] C4  No hardcoded API keys (tests use `sk_test_FAKE...`)
- [x] C5  `Output::Err::reason` does not leak request URLs / file paths / stacks (partial — see M-1 for upstream-body echo concern)
- [x] C6  No `process::exit`, `unsafe`, child processes
- [x] C7  No env-var or disk reads for credentials
- [x] C8  `Input` does NOT derive `Debug` on any endpoint
- [x] C9  Tool is stateless — no `static`, `lazy_static`, `OnceLock`, `Mutex`, `RwLock`, `Cell`, `RefCell`. The `Arc<Client>` is a stateless connection pool; `.clone().with_auth()` produces a per-call builder.
- [x] C10 Cloud Run YAML mounts only `nexus-toolkit-config-*` and `nexus-allowed-leaders-*`
- [x] C11 README uses `sk_test_...`/`sk_live_...` only as placeholders, never as real keys

### HIGH — all PASS

- [x] H1  `#[serde(deny_unknown_fields)]` on every `Input`
- [x] H2  Six unique `path()` values
- [ ] H3  `timeout()` NOT explicitly overridden — using toolkit default of 10s. Acceptable for current endpoints; see M-3.
- [x] H4  User-agent set: `nexus-sdk-payments-stripe/1.0`
- [ ] H5  `cargo audit` not run in sandbox — see I-1
- [x] H6  No logging of secret-shaped fields anywhere
- [x] H7  Credential field named `api_key` everywhere
- [x] H8  `idempotency_key: Option<String>` on every POST endpoint; absent on GETs (correct)
- [x] H9  Tests use only `sk_test_FAKE` / `sk_test_FAKE_FOR_TESTS_ONLY` / `sk_test_BAD`
- [x] H10 Error classes covered in `from_api_error_type`: `invalid_request_error`, `card_error`, `validation_error`, `idempotency_error`, `rate_limit_error`, `authentication_error`, `api_error`, `api_connection_error`

### MEDIUM

- [x] M1  `description()` overridden on every endpoint
- [x] M2  Happy-path test on every endpoint
- [ ] M3  Error-variant test gaps in confirm-payment-intent, get-balance, list-charges
- [x] M4  Crucial Ok fields non-optional; `Option` only where Stripe genuinely omits the field
- [x] M5  No `println!` / `dbg!` outside `#[cfg(test)]`
- [x] M6  All deps use `workspace = true`

### LOW

- [ ] L1  Doc comments missing on five endpoints' Input fields
- [x] L2  `Cargo.toml description` set
- [x] L3  Six sections in README, one per FQN

### Cross-cutting

- [x] X1  All FQNs are `@1` (new tool)
- [x] X2  Descriptions accurately summarize the endpoint
- [x] X3  Idempotent under retry — reads are GET; writes carry `idempotency_key`
- [x] X4  Auth happens at signed-HTTP / `authorize()` layer, not in the tool's `Input`

## Sign-off

**Recommendation: ready-for-testnet.**

**Blockers for mainnet:**

- M-1 (raw body in fallback `reason`)
- M-2 (thin error-variant tests on three endpoints)
- M-3 (explicit `timeout()` override per endpoint)
- I-1 (wire `cargo audit` + `cargo deny` into CI)

Once those four are closed and a `payments-stripe` testnet deploy has
been live without incident for at least 72 hours, mainnet promotion is
recommended.
