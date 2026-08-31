# Nexus tool crate architecture (off-chain, Rust)

Every off-chain tool crate lives at `offchain/tools/<category>-<service>/`
inside the shared workspace at `offchain/Cargo.toml` (which declares
`members = ["tools/*"]`). The canonical examples are
`offchain/tools/memory-memwal` (env-var credentials, the pattern this skill
generates) and `offchain/tools/exchanges-coinbase` (no upstream credentials).
Read both when in doubt.

```text
offchain/tools/<category>-<service>/
├── Cargo.toml                       # workspace inheritance; toolkit + sdk + reqwest + schemars + serde + mockito + zeroize + dotenvy + env_logger + log
├── README.md                        # one section per FQN — Input ports, Output Variants & Ports, docs link, ENV VARS section
├── tools.json                       # { tool_name, command, environment: { RUST_LOG: "info" } } — required for the shared discover workflow
├── build.rs                         # verbatim copy of offchain/tools/memory-memwal/build.rs — validates [[bin]].name == command and threads TOOL_FQN_VERSION
├── .env.example                     # documented env vars; never .env (gitignored)
└── src/
    ├── main.rs                      # explicit tokio runtime; env_logger init; --meta short-circuit; dotenv + credential validation BEFORE bootstrap!([...])
    ├── error.rs                     # <Service>ErrorKind enum + ErrorResponse + status/api-type mappers
    ├── <service>_client.rs          # reqwest Client wrapper; from_env() constructor; Zeroizing<String> bearer; redacting Debug; #[cfg(test)] for_testing()
    └── tools/
        ├── mod.rs                   # const <SERVICE>_API_BASE; pub(crate) mod <endpoint>; shared deserializers
        ├── models.rs                # Shared response types used by ≥2 endpoint modules
        ├── <endpoint_a>.rs          # One file per endpoint = one NexusTool impl. NO api_key on Input.
        └── <endpoint_b>.rs
```

**Deployment lives outside the crate.** There is no per-tool `deploy/`
directory, no per-tool `Dockerfile`, no per-tool `cloud-run.*.yaml`, no
per-tool GitHub workflow. The shared `offchain/Dockerfile` builds any tool
by binary name, and the five reusable workflows
(`.github/workflows/offchain-tools.{discover,prepare,deploy,register,readiness}.yml`)
discover the tool via `tools.json`, build the image, push it, register the
FQN on Sui, and reconcile the toolkit-config secret in GCP Secret Manager.
The repo-level `BLOCKED_TOOLS` variable can flip-off any tool from
deployment without a PR.

## Per-endpoint file template

```rust
//! # `xyz.taluslabs.<category>.<service>.<endpoint>@<TOOL_FQN_VERSION>`
//!
//! Standard Nexus Tool that <does X> from <Service>.
//!
//! ## Credential contract
//! Upstream credentials come from `<SERVICE>_API_KEY` env at startup
//! (see `src/<service>_client.rs::from_env`). They are NEVER fields on
//! `Input` — tool inputs flow through the Nexus DAG on Sui as plaintext.

use {
    crate::{
        <service>_client::<Service>Client,
        error::<Service>ErrorKind,
        tools::{ /* shared deserializers, const <SERVICE>_API_BASE, models */ },
    },
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    // NO api_key / bearer / secret / token / password / private_key here.
    /// <Docstring shows up in input_schema>
    field: String,
    optional_field: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        /// <Docstring shows up in output_schema>
        result: String,
    },
    Err {
        reason: String,
        kind: <Service>ErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct <Endpoint> {
    client: <Service>Client,
}

impl NexusTool for <Endpoint> {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        // Already authed from startup env var. Cheap to construct (Arc clones).
        Self {
            client: <Service>Client::from_env().unwrap_or_else(|e| {
                log::error!("<service> configuration invalid: {e}");
                panic!("<service> configuration invalid: {e}");
            }),
        }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!("xyz.taluslabs.<category>.<service>.<endpoint>@", env!("TOOL_FQN_VERSION")))
    }

    fn path() -> &'static str { "/<endpoint>" }

    fn description() -> &'static str { "<one-liner>" }

    async fn health(&self) -> AnyResult<StatusCode> { Ok(StatusCode::OK) }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        // 1. Validate the input (return Err variant for invalid combinations).
        // 2. Build the endpoint path.
        // 3. Call self.client.get / .post — no .with_auth() needed.
        // 4. Map success/error into Output::Ok / Output::Err.
    }
}

#[cfg(test)]
mod tests {
    use { super::*, mockito::Server, serde_json::json };

    async fn create_server_and_tool() -> (mockito::ServerGuard, <Endpoint>) {
        let server = Server::new_async().await;
        // Bypass env-var validation in tests.
        let client = <Service>Client::for_testing(&server.url(), "sk_test_FAKE_FOR_TESTS_ONLY");
        (server, <Endpoint> { client })
    }

    // happy path, error path, deserialization edge cases…
}
```

## main.rs template

```rust
#![doc = include_str!("../README.md")]
#![allow(clippy::large_enum_variant)]

use nexus_toolkit::bootstrap;

mod <service>_client;
mod error;
mod tools;

fn main() {
    // Install env_logger before anything else so dotenv/credential paths
    // emit through `log::*`; `bootstrap!`'s own try_init becomes a no-op.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // CI's prepare step runs `--meta` from a fresh image with no env, so
    // skip credential validation in that mode. The toolkit handles --meta
    // and exits before any HTTP call is made.
    let meta_only = std::env::args().any(|a| a == "--meta");
    if !meta_only {
        // dotenv + validation run single-threaded — `set_var` is unsound
        // from a multi-threaded process. `main` is the only exit site.
        <service>_client::load_dotenv_if_present();
        if let Err(reason) = <service>_client::validate_credentials_at_startup() {
            log::error!("{} {reason}", <service>_client::ENV_API_KEY);
            std::process::exit(1);
        }
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async {
            bootstrap!([
                tools::<endpoint_a>::<EndpointA>,
                tools::<endpoint_b>::<EndpointB>,
            ])
        });
}
```

## tools/mod.rs template

```rust
//! <Service> endpoints.

pub(crate) const <SERVICE>_API_BASE: &str = "https://api.<service>.com";

pub(crate) mod <endpoint_a>;
pub(crate) mod <endpoint_b>;
pub(crate) mod models;

// Shared custom deserializers go here.
```

## Cargo.toml template

```toml
[package]
name = "<category>-<service>"
description = "<one-line description for crates.io / docs>"

edition.workspace = true
version.workspace = true
repository.workspace = true
homepage.workspace = true
license.workspace = true
readme.workspace = true
authors.workspace = true
keywords.workspace = true
categories.workspace = true

[[bin]]
name = "<category>-<service>"
path = "src/main.rs"

[dependencies]
anyhow.workspace = true
chrono.workspace = true
dotenvy.workspace = true
env_logger.workspace = true
log.workspace = true
reqwest = { workspace = true, features = ["json"] }
schemars.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
zeroize.workspace = true

nexus-toolkit.workspace = true
nexus-sdk.workspace = true

[dev-dependencies]
mockito.workspace = true

[build-dependencies]
serde_json.workspace = true
toml = "0.8"
```

## tools.json template

```json
{
  "tool_name": "<category>-<service>",
  "command": "<category>-<service>",
  "environment": {
    "RUST_LOG": "info"
  }
}
```

Notes:

- `tool_name` and `command` MUST match each other and match `[[bin]].name`
  in `Cargo.toml`. `build.rs` enforces this at compile time.
- `environment` is merged into the Cloud Run service's `env` block at
  deploy time. **Non-secret config only** (`RUST_LOG`, `*_API_BASE`,
  feature flags). Real secrets are mounted via Cloud Run `secretKeyRef`
  from GCP Secret Manager — the operator configures these out-of-band, not
  the deploy pipeline.

## build.rs template

Copy `offchain/tools/memory-memwal/build.rs` verbatim — no template
substitution. It:

- Validates that `[[bin]].name` in `Cargo.toml` matches `command` in
  `tools.json` (compile fails otherwise).
- Reads `TOOL_FQN_VERSION` from the env (set by CI's Docker build-arg from
  the tool's subtree git hash) and re-exports it as a cargo env var so the
  binary can embed it via `env!("TOOL_FQN_VERSION")`.

## Reference examples

- `offchain/tools/memory-memwal/` — env-var credentials (`MEMWAL_DELEGATE_PRIVATE_KEY`),
  `Zeroizing<String>` wrapper, redacting `Debug`, `from_env` constructor,
  `with_test_config` for tests. **This is the credential pattern this skill
  generates.**
- `offchain/tools/storage-walrus/` — env-var fallback for non-secret URLs,
  Cloud Run OIDC for upstream auth (no static secrets).
- `offchain/tools/exchanges-coinbase/` — no upstream credentials; reference
  for the "public API" case (still uses the same shared pipeline).
