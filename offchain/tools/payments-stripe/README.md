# Stripe tools for Nexus

A set of stateless Nexus Tools that wrap the [Stripe REST
API](https://stripe.com/docs/api). Each tool is a single endpoint; pass
your Stripe secret key per request via the `api_key` input port.

## Credential handling

- **Never** put a real Stripe key in a DAG `default_values` entry — DAG
  data is committed to Sui and is public + permanent. Use placeholders
  like `$STRIPE_API_KEY` and have the Leader inject the real value at
  execution time from its secret store.
- **Tests use `sk_test_...` keys only.** Real `sk_live_...` material
  must never enter source, fixtures, or logs.
- The Tool process holds no Stripe credentials between requests. The
  `api_key` lives only in the `Input` struct's scope inside `invoke()`.

## Idempotency

Every write endpoint accepts an optional `idempotency_key`. Generate a
UUID per logical retry-bucket in your DAG. Stripe guarantees identical
responses for identical idempotency keys; reusing the same key on retry
prevents double-charges.

---

# `xyz.taluslabs.payments.stripe.create-payment-intent@1`

Creates a [PaymentIntent](https://stripe.com/docs/api/payment_intents/create).

## Input

- **`api_key`: [`String`]** — Stripe secret key (`sk_test_...` for staging, `sk_live_...` for prod). Sourced by the Leader at execution time.
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

# `xyz.taluslabs.payments.stripe.get-payment-intent@1`

Retrieves a [PaymentIntent](https://stripe.com/docs/api/payment_intents/retrieve) by id.

## Input

- **`api_key`: [`String`]** — Stripe secret key.
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

# `xyz.taluslabs.payments.stripe.confirm-payment-intent@1`

[Confirms](https://stripe.com/docs/api/payment_intents/confirm) a PaymentIntent.

## Input

- **`api_key`: [`String`]**
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

# `xyz.taluslabs.payments.stripe.create-customer@1`

Creates a [Customer](https://stripe.com/docs/api/customers/create).

## Input

- **`api_key`: [`String`]**
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

# `xyz.taluslabs.payments.stripe.get-balance@1`

Reads the platform [Balance](https://stripe.com/docs/api/balance/balance_retrieve).

## Input

- **`api_key`: [`String`]**

## Output Variants & Ports

**`ok`**

- **`ok.available`: [`Vec<{ amount: i64, currency: String }>`]** — Funds available for payout.
- **`ok.pending`: [`Vec<{ amount: i64, currency: String }>`]** — Funds still settling.

**`err`** — See above.

---

# `xyz.taluslabs.payments.stripe.list-charges@1`

Lists [Charges](https://stripe.com/docs/api/charges/list).

## Input

- **`api_key`: [`String`]**
- **`limit`: [`i64`] (optional)** — Page size (1–100, Stripe default 10).
- **`customer`: [`String`] (optional)** — Filter to a specific customer id.
- **`starting_after`: [`String`] (optional)** — Cursor for pagination (charge id from the previous page).

## Output Variants & Ports

**`ok`**

- **`ok.charges`: [`Vec<{ id, amount, currency, status, customer? }>`]**
- **`ok.has_more`: [`bool`]** — More results available with `starting_after = charges.last().id`.

**`err`** — See above.
