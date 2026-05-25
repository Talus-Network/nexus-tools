# Stripe tools for Nexus

A set of stateless Nexus Tools that wrap the [Stripe REST
API](https://stripe.com/docs/api). Each tool is a single endpoint.

## Credential handling

- **The tool reads `STRIPE_API_KEY` once from the process environment at
  startup.** Sources:
  - Production (Cloud Run): mounted via `secretKeyRef` from GCP Secret
    Manager, configured by the operator out-of-band (the deploy pipeline
    does NOT provision the secret).
  - Local dev: copy `.env.example` to `.env` and set
    `STRIPE_API_KEY=sk_test_…`.
- **The credential never appears on any `Input` struct.** Tool inputs flow
  through the Nexus DAG on Sui as plaintext — anything on `Input` is
  effectively published on-chain. The skill's auditor will refuse to mark
  the tool ready if any `Input` field is credential-shaped.
- The credential is wrapped in `zeroize::Zeroizing<String>` (heap-zeroed
  on drop). The struct hand-implements `Debug` to print `<redacted>`.
- The tool exits 1 at startup if `STRIPE_API_KEY` is unset, empty, or does
  not start with one of `sk_test_`, `sk_live_`, `rk_test_`, `rk_live_`.
- **Tests use `sk_test_…` keys only.** Real `sk_live_…` material must
  never enter source, fixtures, or logs.

## Environment variables

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `STRIPE_API_KEY` | **yes** | — | Stripe secret (`sk_test_…` for staging/test, `sk_live_…` for prod). Validated at startup; the tool refuses to boot without it. |
| `RUST_LOG` | no | `info` | env_logger filter. |
| `BIND_ADDR` | no | `127.0.0.1:8080` | The toolkit's HTTP bind address. |

## Idempotency

Every write endpoint accepts an optional `idempotency_key`. Generate a
UUID per logical retry-bucket in your DAG. Stripe guarantees identical
responses for identical idempotency keys; reusing the same key on retry
prevents double-charges. `idempotency_key` is per-call dedup data — NOT a
credential — so it stays on `Input`.

## FQN versioning

All six FQNs are threaded through `env!("TOOL_FQN_VERSION")` in
`build.rs`; CI sets `TOOL_FQN_VERSION` from the tool's subtree git hash,
so any source change auto-bumps the version on the next deploy. Local
builds default to `@1`.

---

# `xyz.taluslabs.payments.stripe.create-payment-intent@<TOOL_FQN_VERSION>`

Creates a [PaymentIntent](https://stripe.com/docs/api/payment_intents/create).

## Input

- **`idempotency_key`: [`String`] (optional)** — `Idempotency-Key` header value. Generate a UUID per retry-bucket.
- **`amount`: [`i64`]** — Amount in the smallest currency unit (e.g. cents for USD).
- **`currency`: [`String`]** — ISO-4217 currency code (lowercase, e.g. `usd`).
- **`customer`: [`String`] (optional)** — Existing Stripe customer id (`cus_…`).
- **`description`: [`String`] (optional)** — Free-form description shown on the Stripe Dashboard.

## Output Variants & Ports

**`ok`** — PaymentIntent created.

- **`ok.id`: [`String`]** — PaymentIntent id (`pi_…`).
- **`ok.client_secret`: [`String`]** — Used by clients to confirm the intent.
- **`ok.status`: [`String`]** — Stripe lifecycle status (`requires_payment_method`, `requires_confirmation`, etc.).
- **`ok.amount`: [`i64`]** — Echo of the requested amount.
- **`ok.currency`: [`String`]** — Echo of the requested currency.

**`err`** — Stripe rejected the request or the network failed.

- **`err.reason`: [`String`]** — Human-readable error description.
- **`err.kind`: [`String`]** — One of `invalid_request`, `card_error`, `validation_error`, `idempotency_error`, `rate_limit_exceeded`, `unauthorized`, etc.
- **`err.status_code`: [`u16`] (optional)** — HTTP status from Stripe, if any.

---

# `xyz.taluslabs.payments.stripe.get-payment-intent@<TOOL_FQN_VERSION>`

Retrieves a [PaymentIntent](https://stripe.com/docs/api/payment_intents/retrieve) by id.

## Input

- **`payment_intent_id`: [`String`]** — `pi_…` to look up.

## Output Variants & Ports

**`ok`**

- **`ok.id`: [`String`]**
- **`ok.status`: [`String`]**
- **`ok.amount`: [`i64`]**
- **`ok.currency`: [`String`]**
- **`ok.client_secret`: [`String`] (optional)** — present unless the intent has been confirmed.

**`err`** — See `create-payment-intent` for the variant shape.

---

# `xyz.taluslabs.payments.stripe.confirm-payment-intent@<TOOL_FQN_VERSION>`

[Confirms](https://stripe.com/docs/api/payment_intents/confirm) a PaymentIntent.

## Input

- **`idempotency_key`: [`String`] (optional)**
- **`payment_intent_id`: [`String`]** — `pi_…` to confirm.
- **`payment_method`: [`String`] (optional)** — `pm_…` to attach (e.g. `pm_card_visa` in test mode).
- **`return_url`: [`String`] (optional)** — Required for redirect-based payment methods.

## Output Variants & Ports

**`ok`**

- **`ok.id`: [`String`]**
- **`ok.status`: [`String`]**
- **`ok.next_action_type`: [`String`] (optional)** — Set when the PaymentIntent requires further user action.

**`err`** — See above.

---

# `xyz.taluslabs.payments.stripe.create-customer@<TOOL_FQN_VERSION>`

Creates a [Customer](https://stripe.com/docs/api/customers/create).

## Input

- **`idempotency_key`: [`String`] (optional)**
- **`email`: [`String`] (optional)**
- **`name`: [`String`] (optional)**
- **`description`: [`String`] (optional)**

## Output Variants & Ports

**`ok`**

- **`ok.id`: [`String`]** — `cus_…`.
- **`ok.email`: [`String`] (optional)**
- **`ok.created`: [`i64`]** — Unix timestamp.

**`err`** — See above.

---

# `xyz.taluslabs.payments.stripe.get-balance@<TOOL_FQN_VERSION>`

Reads the platform [Balance](https://stripe.com/docs/api/balance/balance_retrieve).

## Input

No input ports — credentials come from env.

## Output Variants & Ports

**`ok`**

- **`ok.available`: [`Vec<{ amount: i64, currency: String }>`]** — Funds available for payout.
- **`ok.pending`: [`Vec<{ amount: i64, currency: String }>`]** — Funds still settling.

**`err`** — See above.

---

# `xyz.taluslabs.payments.stripe.list-charges@<TOOL_FQN_VERSION>`

Lists [Charges](https://stripe.com/docs/api/charges/list).

## Input

- **`limit`: [`i64`] (optional)** — Page size (1–100, Stripe default 10).
- **`customer`: [`String`] (optional)** — Filter to a specific customer id.
- **`starting_after`: [`String`] (optional)** — Cursor for pagination (charge id from the previous page).

## Output Variants & Ports

**`ok`**

- **`ok.charges`: [`Vec<{ id, amount, currency, status, customer? }>`]**
- **`ok.has_more`: [`bool`]** — More results available with `starting_after = charges.last().id`.

**`err`** — See above.
