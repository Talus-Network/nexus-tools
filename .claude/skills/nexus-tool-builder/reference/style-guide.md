# Tool design rules (enforced by the skill)

Distilled from `Talus-Network/nexus-sdk/docs/tool-development.md`.

## Naming

- All port names (Input Ports, Output Variants, Output Ports) are
  `snake_case`. Never `camelCase` / `PascalCase` / `APIKey`.
- Names are descriptive and concise: `api_key`, not `k` or `apk`.
- Erroneous output variants start with `err`: `err`, `err_http`, `err_quota`.
  Never `error`, `failure`, `http_exception`.

## Erroneous variants

Any variant whose name starts with `err` is treated by Nexus as an error
variant. All ports in an error variant are passed on-chain regardless of
DAG edges. Use this:

```rust
Err {
    reason: String,        // human-readable
    kind: <Service>ErrorKind, // machine-readable enum
    #[serde(skip_serializing_if = "Option::is_none")]
    status_code: Option<u16>,
}
```

## Interface design

| ✅ Do | ❌ Don't |
|---|---|
| Build a tool that encapsulates the API's surface (one tool per endpoint, parameterized). | Build a tool that only does one hardcoded call (e.g. "BTC-USD spot price"). |
| Split `prompt` and `context` into separate input ports even if the API merges them. | Merge them into one input port — the DAG can't set defaults for fields combined with edge data. |
| Accept `json_schema` as input and validate generic responses against it (where applicable). | Hardcode the output schema for a single endpoint when the underlying API serves many. |
| Make crucial output ports non-optional. Return `err` if data is missing. | Make crucial output ports `Option<T>` and force every downstream tool to null-check. |
| Keep output ports flat: `ok.id`, `ok.text`. | Nest output ports: `ok.response.tweet.id`. The next tool can't bind to that. |

## Documentation

Every crate has a `README.md` with one section per FQN:

```markdown
# `xyz.taluslabs.<category>.<service>.<endpoint>@1`

<One-paragraph description.> API [reference](https://...).

## Input

**`field`: [`Type`]** — description.
**`optional`: [`Type`] (optional)** — description.

## Output Variants & Ports

**`ok`** — <when this happens>.
- **`ok.field`: [`Type`]** — description.

**`err`** — <when this happens>.
- **`err.reason`: [`String`]**
- **`err.kind`: [`String`]**
- **`err.status_code`: [`u16`] (optional)**

---
```

`main.rs` must include the README via:

```rust
#![doc = include_str!("../README.md")]
```

## Validate first, then call

Inside `invoke`, validate input before making any external call. Surface
validation failures as `Output::Err { kind: <Service>ErrorKind::InvalidRequest, status_code: None, ... }`.
