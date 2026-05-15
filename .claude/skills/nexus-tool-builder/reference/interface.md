# `NexusTool` trait contract and `bootstrap!`

Source: `nexus-sdk/toolkit-rust/src/nexus_tool.rs`.

## Required associated types

| Type | Constraints | Purpose |
|---|---|---|
| `Input` | `JsonSchema + DeserializeOwned + Send` | Generates the input schema. Deserialized from the request body. |
| `Output` | `JsonSchema + Serialize + Send`, **must be a Rust `enum`** so the schema gets a top-level `oneOf` | Generates the output schema. Serialized into the response. |

## Required methods

| Method | Signature | Notes |
|---|---|---|
| `fqn` | `fn fqn() -> ToolFqn` | Use the `fqn!("xyz.taluslabs.<category>.<service>.<endpoint>@1")` macro. |
| `new` | `async fn new() -> Self` | Called per request. Inject dependencies here. |
| `invoke` | `async fn invoke(&self, input: Self::Input) -> Self::Output` | Main logic. **No `Result`** — errors go in `Output::Err`. |
| `health` | `async fn health(&self) -> AnyResult<StatusCode>` | Check dependent services. Return `Ok(StatusCode::OK)` when healthy. |

## Optional methods (have defaults)

| Method | Default | When to override |
|---|---|---|
| `path` | `""` (root) | When multiple tools live in one crate — each needs a unique path. |
| `description` | `""` | Always — surfaces in `/meta`. |
| `timeout` | `Duration::from_secs(10)` | Override for slow upstreams; keep below the Leader's request budget. |
| `authorize` | `Ok(())` | Tool-side allowlists / rate-limits using the verified `AuthContext`. |

## Generated endpoints (from `NexusTool` impl)

| Endpoint | Body |
|---|---|
| `GET <path>/health` | Status code returned by `health()`. |
| `GET <path>/meta` | JSON: `{ fqn, url, timeout, description, input_schema, output_schema }`. |
| `POST <path>/invoke` | Deserializes body as `Input`, calls `invoke`, serializes the result as `Output`. |

## `bootstrap!` macro

```rust
// Single tool, default 127.0.0.1:8080 (or $BIND_ADDR).
bootstrap!(MyTool);

// Multiple tools, default address. Each NexusTool::path() must be unique.
bootstrap!([MyTool, MyOtherTool]);

// Single tool, custom address.
bootstrap!(([0, 0, 0, 0], 8081), MyTool);

// Multiple tools, custom address.
bootstrap!(([0, 0, 0, 0], 8081), [MyTool, MyOtherTool]);
```

## Output enum convention

```rust
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        // flat ports; crucial fields non-optional
    },
    Err {
        reason: String,
        kind: <Service>ErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}
```

`#[serde(rename_all = "snake_case")]` turns the `Ok` / `Err` variants into
the lowercase `ok` / `err` Nexus expects.

Variant names beginning with `err` are treated by Nexus as error variants —
their ports are automatically passed on-chain regardless of DAG edges.
