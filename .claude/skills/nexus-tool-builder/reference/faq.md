# FAQ / pitfalls

## "My tool builds but the runtime rejects the output schema"

`Output` must be a Rust `enum`, not a struct. The Nexus runtime requires a
top-level `oneOf` in the JSON schema, which `schemars` only emits for
enums. Wrap your fields:

```rust
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok { /* ... */ },
    Err { /* ... */ },
}
```

## "Two of my tools collide at the same path"

`bootstrap!([ToolA, ToolB])` mounts each tool at `NexusTool::path()`. The
default is `""` (root). Always override `path()` for every tool when more
than one lives in the same crate:

```rust
fn path() -> &'static str { "/get-spot-price" }
```

## "I see `unknown field 'foo'` deserializing valid input"

Inputs have `#[serde(deny_unknown_fields)]`. Add the field to your `Input`
struct, or relax the attribute (only if upstream callers are trusted).

## "I want my tool to call the same API across multiple endpoints — do I duplicate the client?"

No. Create one `<service>_client.rs` with a `<Service>Client` that has
`.get<T>` / `.post<T>` methods. Each endpoint's `NexusTool` impl holds an
instance:

```rust
pub(crate) struct GetSpotPrice {
    client: CoinbaseClient,
}
```

In tests, inject a `mockito::Server` via the test-only constructor:
`<Service>Client::for_testing(&server.url(), "sk_test_FAKE")`.

## "Where do I put shared response models?"

`src/tools/models.rs`. Use it for `<Service>ApiResponse<T>` and similar
wrappers shared by ≥2 endpoint modules. Endpoint-specific shapes can live
in the endpoint file.

## "How do I handle API auth (API key, OAuth)?"

Read the secret from an env var **at startup**, not per request. Pattern,
from `offchain/tools/memory-memwal/src/client.rs`:

```rust
pub(crate) const ENV_API_KEY: &str = "<SERVICE>_API_KEY";

pub(crate) fn validate_credentials_at_startup() -> Result<(), String> {
    let raw = std::env::var(ENV_API_KEY)
        .ok()
        .map(zeroize::Zeroizing::new);
    match classify_key(raw.as_deref().map(|z| z.as_str())) {
        KeyOk(_) => Ok(()),
        Missing => Err("is not set".into()),
        Invalid(reason) => Err(reason),
    }
}

pub fn from_env() -> Result<Self, String> {
    let bearer = std::env::var(ENV_API_KEY)
        .map_err(|_| format!("{ENV_API_KEY} not set"))?;
    Ok(Self {
        client: shared_http(),
        bearer: zeroize::Zeroizing::new(bearer),
        // ...
    })
}
```

In production the env var is mounted by Cloud Run from a Secret Manager
secret (`secretKeyRef` in the service config — operator-configured, not
emitted by the deploy pipeline).

**Do NOT** put `api_key`, `bearer_token`, `*_secret`, `*_token`,
`password`, `private_key`, `access_token`, `consumer_secret`, or
`client_secret` on any `Input` struct. Tool inputs are committed to the
Nexus DAG on Sui as plaintext. The auditor's `static:input-credential`
check refuses to mark the tool ready if it finds such a field.

## "How do I version a tool?"

The FQN version (`@N` suffix) is computed by CI from the tool's subtree
git hash and injected at build time via `TOOL_FQN_VERSION`. Threaded into
the binary by `build.rs`, embedded into each FQN via
`fqn!(concat!("xyz.taluslabs.<...>", "@", env!("TOOL_FQN_VERSION")))`.

This means any source change auto-bumps the FQN version on the next deploy
— existing on-chain registrations are preserved, the new version registers
alongside. The CLI / `nexus-sdk` handles `EFqnAlreadyExists` as idempotent.

If you want a deterministic version pin for local dev, the `build.rs`
defaults `TOOL_FQN_VERSION=1` when the env var is unset.

## "My tool keeps timing out at 10s in Nexus but works locally"

Override `NexusTool::timeout()`. Default is 10s; pick something below the
Leader's request budget but with enough headroom for upstream latency.

```rust
fn timeout() -> Duration { Duration::from_secs(30) }
```

## "Do I need signed HTTP locally?"

Not during development. Signed HTTP is enforced only when the deployed
service mounts a `toolkit-config.json` from Secret Manager (the prepare
workflow does this in production via `NEXUS_TOOLKIT_CONFIG_PATH`). Local
dev runs without it.

## "Workspace `members` line — do I need to update it?"

No. `offchain/Cargo.toml` declares `members = ["tools/*"]` so any new
directory under `offchain/tools/` is picked up automatically.

## "How does my tool get into the deploy pipeline?"

Add `tools.json` to your crate root and push. The
`.github/workflows/offchain-tools.discover.yml` reusable workflow scans
`offchain/tools/*/tools.json`, builds a matrix, and the downstream
prepare/deploy/register/readiness workflows handle the rest. No per-tool
workflow file is needed. (If you've added one — delete it.)

## "What's `BLOCKED_TOOLS`?"

A repo-level GitHub Actions variable (Settings → Variables) holding a
space-separated list of `tool_name` values to skip. Set on the repo, no PR
required. The discover workflow drops blocked tools from the matrix, so
they're excluded from build, deploy, register, and Terraform apply. Use
this as the emergency kill switch when a tool ships a vulnerability —
flip it off in repo settings, fix in a follow-up PR, flip it back on.

As of writing, `http` is blocked due to a missing SSRF guard (the tool
would let a DAG reach the GCP metadata server and exfiltrate an SA OAuth
token; re-enable after the guard ships).

## "Where do upstream API keys come from in production?"

Cloud Run mounts them from GCP Secret Manager via `secretKeyRef`. The
operator configures the secret + the service's `env.valueFrom` mapping
out-of-band (it's part of the Terraform/console wiring, not the deploy
pipeline). The pipeline only provisions secrets it owns:

- `nexus-tools-<tool>-v<ver>-signed-http-keys` — the tool's own Ed25519
  signing keys (one per FQN).
- `nexus-tools-<tool>-v<ver>-signed-http-toolkit-config` — the toolkit
  config JSON with KID mappings.
- `nexus-allowed-leaders-<network>` — the network's Leader public keys.

Upstream credentials (e.g. `STRIPE_API_KEY`, `OPENAI_API_KEY`) are
**operator-owned secrets**, mounted to the tool's Cloud Run service
through a separate secret + `secretKeyRef` binding that the operator
maintains.

## "On-chain or off-chain?"

On-chain if the logic must be verifiable on Sui and you don't need
Web2 / secrets / heavy compute. Off-chain otherwise. See
`reference/onchain-tools.md`.
