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

Tests inject a `mockito::Server` via `<Service>Client::new(Some(&server.url()))`.

## "Where do I put shared response models?"

`src/tools/models.rs`. Use it for `CoinbaseApiResponse<T>` and similar
wrappers shared by ≥2 endpoint modules. Endpoint-specific shapes can live
in the endpoint file.

## "How do I handle API auth (API key, OAuth)?"

In `<service>_client.rs`. Read the secret from an env var at
`<Service>Client::new`:

```rust
let api_key = std::env::var("COINBASE_API_KEY").ok();
```

Wire it in Cloud Run via Secret Manager (`secretKeyRef` in the service
YAML). Never log the key — `tracing` filters and `Display` impls have to
exclude it.

## "How do I version a tool?"

The `@1` suffix in the FQN is immutable per registered tool. Output-schema
changes ⇒ bump to `@2` and register a new module/endpoint alongside `@1`,
deprecating `@1` once consumers migrate. Non-schema changes reuse `@1`.

## "My tool keeps timing out at 10s in Nexus but works locally"

Override `NexusTool::timeout()`. Default is 10s; pick something below the
Leader's request budget but with enough headroom for upstream latency.

```rust
fn timeout() -> Duration { Duration::from_secs(30) }
```

## "Do I need signed HTTP locally?"

Not during development. Signed HTTP is enforced only when
`signed_http.mode = "required"` in `NEXUS_TOOLKIT_CONFIG_PATH`. The
templated `cloud-run.<env>.yaml` sets that variable; local dev with
`just tools run <crate>` leaves it unset.

## "Workspace `members` line — do I need to update it?"

No. `Cargo.toml` declares `members = ["tools/*"]` so any new directory
under `tools/` is picked up automatically.

## "What about `tools/.just`?"

Yes, you do need to extend the build/check/test/fmt-check/clippy recipes
with `cargo +stable build --package <crate> --release` (and equivalents).
The skill does this for you.

## "On-chain or off-chain?"

On-chain if the logic must be verifiable on Sui and you don't need
Web2 / secrets / heavy compute. Off-chain otherwise. See
`reference/onchain-tools.md`.
