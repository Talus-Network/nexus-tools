# Nexus tool crate architecture (off-chain, Rust)

Every off-chain tool crate in `tools/<category>-<service>/` follows the
layout below. The canonical example is `tools/exchanges-coinbase` — read it
when in doubt.

```text
tools/<category>-<service>/
├── Cargo.toml                       # workspace inheritance; toolkit + sdk + reqwest + schemars + serde + mockito
├── README.md                        # one section per FQN — Input ports, Output Variants & Ports, docs link
├── deploy/                          # added by this skill (not in upstream layout)
│   ├── Dockerfile
│   ├── cloud-run.testnet.yaml
│   ├── cloud-run.mainnet.yaml
│   ├── register.sh
│   └── allowed_leaders.testnet.json   # placeholder; real one mounted via Secret Manager
└── src/
    ├── main.rs                      # #![doc = include_str!("../README.md")] + bootstrap!([...])
    ├── error.rs                     # <Service>ErrorKind enum + ErrorResponse + status/api-type mappers
    ├── <service>_client.rs          # reqwest Client wrapper; .get<T>/.post<T> with typed error
    └── tools/
        ├── mod.rs                   # const <SERVICE>_API_BASE; pub(crate) mod <endpoint>; shared deserializers
        ├── models.rs                # Shared response types used by ≥2 endpoint modules
        ├── <endpoint_a>.rs          # One file per endpoint = one NexusTool impl
        └── <endpoint_b>.rs
```

## Per-endpoint file template

```rust
//! # `xyz.taluslabs.<category>.<service>.<endpoint>@1`
//!
//! Standard Nexus Tool that <does X> from <Service>.

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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
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
        Self { client: <Service>Client::new(None) }
    }

    fn fqn() -> ToolFqn {
        fqn!("xyz.taluslabs.<category>.<service>.<endpoint>@1")
    }

    fn path() -> &'static str { "/<endpoint>" }

    fn description() -> &'static str { "<one-liner>" }

    async fn health(&self) -> AnyResult<StatusCode> { Ok(StatusCode::OK) }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        // 1. Validate the input (return Err variant for invalid combinations).
        // 2. Build the endpoint path.
        // 3. Call self.client.get / .post.
        // 4. Map success/error into Output::Ok / Output::Err.
    }
}

#[cfg(test)]
mod tests {
    use { super::*, mockito::Server, serde_json::json };

    async fn create_server_and_tool() -> (mockito::ServerGuard, <Endpoint>) {
        let server = Server::new_async().await;
        let client = <Service>Client::new(Some(&server.url()));
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

#[tokio::main]
async fn main() {
    bootstrap!([
        tools::<endpoint_a>::<EndpointA>,
        tools::<endpoint_b>::<EndpointB>,
    ]);
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

[dependencies]
chrono.workspace = true
thiserror.workspace = true
tokio.workspace = true
reqwest = { workspace = true, features = ["json"] }
serde_json.workspace = true
serde.workspace = true
schemars.workspace = true

nexus-toolkit.workspace = true
nexus-sdk.workspace = true

[dev-dependencies]
mockito.workspace = true
```

## Reference example

`tools/exchanges-coinbase/` — 3 endpoints (`get-spot-price`,
`get-product-ticker`, `get-product-stats`), shared `coinbase_client.rs`,
shared `error.rs`, shared `models.rs`, shared deserializer
(`deserialize_trading_pair`) in `tools/mod.rs`. Copy from it freely.
