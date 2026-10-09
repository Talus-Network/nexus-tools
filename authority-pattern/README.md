# Agent API Authority Pattern

This folder contains the complete Agent API Tool implementation: the coin-specific on-chain Move generator and fixtures, the standalone Rust HTTP Tools and lifecycle worker, bundle-local checks, deployment configuration, and a plan-only deployment helper. The plan helper never publishes a package, registers a Tool or signing key, starts a service, or submits a transaction.

## Layout and local checks

`onchain/agent_api/` contains the generator, MVR-backed package templates, generated SUI and test-coin fixtures, and generator tests. `offchain/` is a standalone Cargo package with the two shared signed HTTP Tools, provider adapter, durable backend, and settlement/refund worker. `deploy.py` validates operator configuration and prints a reviewable plan. `deployment.example.json` contains placeholders only.

Run commands from this directory:

```sh
just test
just check
just clippy
just fmt-check
just build
just verify /path/to/nexus/sui
just demo
```

`just build` builds the standalone `agent-api` binary. `just verify /path/to/nexus/sui` runs the build, Rust checks and tests, strict Clippy, pinned format check, both local Move fixture builds/tests/coverage, Tool metadata output, and lifecycle demo. It passes the explicit Nexus Sui package root to the temporary-copy Move runner. No command publishes a package, registers a Tool, or submits a transaction.

`just test` runs all Rust targets and features, generator tests, deployment-plan tests, and local Move-runner tests. To build and test both Move fixture packages with local Nexus sources, pass the explicit `sui/` package directory from a Nexus checkout:

```sh
just move-test /path/to/nexus/sui
```

The runner copies each fixture to a temporary directory and changes only those copies to use the seven local Nexus Move packages. Checked-in manifests and generated output keep their public MVR coordinates. If the default `~/.move` package cache is read-only, point `MOVE_HOME` at a writable cache directory for the command. The local VM tests and SDK schema interop test do not prove that published MVR bytecode matches or can be fetched; report those as separate evidence.

Generate a concrete coin package with a JSON config that follows `fixtures/config/test-coin.json`:

```sh
just generate ./coin-config.json ./build/agent_api_coin
sui move build --path ./build/agent_api_coin
sui move test --path ./build/agent_api_coin
```

Generation refuses to overwrite an existing output directory. Keep custom coin configs and deployment state outside committed sources.

## Tool contracts

Each generated coin package exposes four concrete on-chain Tools: `xyz.taluslabs.agent_api.<coin-slug>.register@2`, `xyz.taluslabs.agent_api.<coin-slug>.charge@1`, `xyz.taluslabs.agent_api.<coin-slug>.authorize@1`, and `xyz.taluslabs.agent_api.<coin-slug>.revoke@1`. The `register@2` Tool takes `owner_public_key_hex` as one scalar string: use `""` with export disabled or exactly 64 hexadecimal characters without a `0x` prefix for the 32-byte X25519 public key. The `register@2` ABI replaces `register@1`'s vector input; existing deployments keep the old schema and must register `register@2` before using the scalar form. The `register` result `Registered { binding_id: string }` and `authorize` result `Authorized { binding_id: string }` expose the quoted Sui address as named string/Data ports through InlineData JSON. The authorization output lets a DAG pass the non-secret binding ID to the shared HTTP Tools. The Rust SDK interop test reads the production Move template and checks the pinned SDK converter against the registration string contract.

The standalone service exposes `xyz.taluslabs.agent_api.query@<TOOL_FQN_VERSION>` and `xyz.taluslabs.agent_api.retrieve-key@<TOOL_FQN_VERSION>`. The numeric suffix comes from the exact binary build (`TOOL_FQN_VERSION`); record the same value in deployment configuration. Both routes require signed HTTP v3, validate the signed canonical input, and return canonical BCS output. After the on-chain event, their authorize hooks wait up to 10 seconds, polling every 250 ms, for the matching grant to arrive through the worker. The deadline includes time waiting for the Store mutex or SQLite; grant lookups run on Tokio's blocking pool, and any lookup that outlives the deadline remains read-only. A present mismatch or storage error still fails immediately, while an absent or blocked lookup at the deadline returns the existing 403 without storing authorization context. The wait does not write grants or change the 20-second Tool timeout. Query consumes a matching one-use on-chain authorization; key retrieval returns only an authenticated encrypted envelope for the approved owner key.

## Prepare deployment inputs

The Agent API manifest opts out of the generic automatic Offchain Tools deployment and registration flow. That flow configures one HTTP service process, while Agent API also requires externally supplied master/provider credentials, a durable service database, a provider with the key-disable and usage-settlement lifecycle, and a separately supervised event/refund worker with its own signer and Sui gRPC access. Adding environment placeholders does not provide those missing roles or persistence. Use the operator-configured service and worker plan below, and provision those dependencies explicitly. Documentation publication remains enabled separately, so this guide is synchronized under `tools/agent-api/` using its manifest name.

Copy `deployment.example.json` to the ignored local file `deployment.local.json`, then replace every placeholder with operator-selected values. Provide the generated Move package path, exact concrete coin type, event RPC URL, the chosen invocation prices in MIST, final HTTPS service URL, provider URL, Toolkit configuration path, protected signing-key file paths, worker gRPC URL, and the actual `TOOL_FQN_VERSION` embedded in the binary. Keep API credentials and signing material in the external secret manager; configuration stores environment-variable names and mounted file paths only.

The planner resolves relative package, database, Toolkit, allowlist, and signing-key paths against the deployment configuration file's directory and normalizes absolute inputs as absolute paths. It shell-quotes these paths and emits the Cargo manifest as an absolute path derived from this bundle, so generated instructions can be run from any current directory. The local mock-provider database is placed beside the configured Agent API database.

The planner accepts the exact secret variable names read by the binaries: `AGENT_API_MASTER_KEY`, `AGENT_API_PROVIDER_OPERATOR_KEY`, and `AGENT_API_SETTLEMENT_SIGNER_KEY_B64`. A different name cannot be configured because the service and worker do not read aliases.

Before publication, the plan prints the current `sui client publish <package-path>` instruction and reports the post-publication package, cashier, operator-cap, settlement-cap, and four witness IDs still required. The plan does not run that command. After an operator-authorized publish, record the returned IDs and choose a positive per-coin `credit_rate` in the local configuration and validate it:

```sh
python3 deploy.py validate --config deployment.local.json
python3 deploy.py plan --config deployment.local.json
```

A cashier starts without a configured rate. Before Tool registration, the plan prints a `sui client ptb --move-call <package-id>::accounting::set_credit_rate <coin-type> @<operator-cap-id> @<cashier-id> <credit-rate>` instruction using those explicit per-coin values. The matching `OperatorCap` controls rate updates, and `register` has no caller-selected price input: it snapshots the cashier's configured rate, so later updates affect only new bindings. This setup instruction is marked `NOT EXECUTED` like the other plan steps.

A complete plan prints one `nexus tool register onchain` instruction for each of the four concrete module/FQN/witness combinations per coin package, after that coin's operator pricing setup. It also prints one `nexus tool register offchain --batch` instruction for both shared HTTP Tools, separate signing-key preparation and registration instructions for their exact built FQNs, the allowed-Leader export step, and the service, mock-provider, and worker launch environments. The planner prints `NOT EXECUTED` for every instruction and has only `validate` and `plan` modes.

The current CLI requires `--package`, `--module`, `--tool-fqn`, `--description`, and `--tool-witness-id` for each on-chain registration. The plan includes the optional explicit collateral object only when supplied. Registration uses the CLI's default owner-cap save behavior so the later signing-key registration can use the returned off-chain owner capabilities; protect the Nexus CLI config and record returned IDs. The operator must still check the active network, signer, gas, and owned `Coin<US>` collateral immediately before any registration. Those registrations, signing-key registration, package publication, and all settlement/refund transactions remain manual, separately authorized actions.

The event worker configuration contains one route per cashier, with `rpc_url`, package ID, `module: "accounting"`, concrete coin type, cashier ID, and settlement-cap ID. Do not reuse a cashier, settlement cap, or package/module event stream. The worker uses `AGENT_API_SETTLEMENT_SIGNER_KEY_B64` from the external secret manager and checks that signer ownership matches the settlement capability before submitting settlement or refund transactions.

## Signed HTTP runtime configuration

Mount a Toolkit configuration at the operator-selected `NEXUS_TOOLKIT_CONFIG_PATH`. It must use the current Toolkit JSON contract and `signed_http.mode: "required"`; the following is a shape example with placeholder FQNs and values, not a ready-to-use secret file:

```json
{
  "invoke_max_body_bytes": 10485760,
  "signed_http": {
    "mode": "required",
    "allowed_leaders_path": "/run/secrets/allowed-leaders.json",
    "tools": {
      "xyz.taluslabs.agent_api.query@42": {
        "response_signing_key": "<secret-manager-provided-32-byte-key>",
        "replay_cache_ttl_ms": 300000
      },
      "xyz.taluslabs.agent_api.retrieve-key@42": {
        "response_signing_key": "<secret-manager-provided-32-byte-key>",
        "replay_cache_ttl_ms": 300000
      }
    }
  }
}
```

Use the numeric FQN suffix embedded in the binary, keep the private response-signing values and active Leader allowlist outside the repository, and refresh the allowlist after Leader key rotation. `nexus tool auth keygen` writes a file containing a private key; protect that output with restrictive file permissions and never print or commit it. A production service URL must use trusted HTTPS and preserve all Nexus signed HTTP v3 request and response headers.

## Local lifecycle and endpoint checks

`just demo` uses local mock state and performs the reconciled workload: one authorized one-credit query plus 39 owner-direct one-credit calls, for total usage of 40. Its expected balances are wallet `1000 → 900 → 980`, reserve `100 → 0`, provider credit 200 with usage 40 and spendable credit `160` before revoke and `0` after key disable, earned value `20`, and refund `80`. Settlement/refund in this demo is simulated; it is not evidence of a live chain transaction.

For local service checks, use synthetic credentials and an isolated temporary database, start the mock provider on loopback, then start the Toolkit service with its local required-mode config. `just validate-http http://127.0.0.1:<port>` performs only the CLI's health and metadata requests. It does not register a Tool or exercise a chain path. The Rust tests cover signed-body tampering, replay, provider failures, durable lifecycle behavior, and canonical output decoding.

The service reads `AGENT_API_DB_PATH`, `AGENT_API_MASTER_KEY` (64 hex characters), `AGENT_API_PROVIDER_URL`, `AGENT_API_PROVIDER_OPERATOR_KEY`, `AGENT_API_DEPLOYMENTS`, and `NEXUS_TOOLKIT_CONFIG_PATH`. The worker additionally reads `AGENT_API_SUI_GRPC_URL`, `AGENT_API_SETTLEMENT_SIGNER_KEY_B64`, and optionally `AGENT_API_SETTLEMENT_GAS_BUDGET`. Set secret values in the runtime environment from an approved secret manager; do not put them in Tool input, plan files, logs, or this repository.
