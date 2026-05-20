//! # `xyz.taluslabs.memory.memwal.forget@1`
//!
//! Nexus Tool that deletes every memory in a MemWal namespace. The call is
//! owner-scoped — the relayer's auth middleware identifies the account from
//! the signed request; only memories belonging to that account are removed,
//! regardless of which namespace string is sent.
//!
//! ## Use cases
//!
//! Lifecycle complement to `remember`: a DAG that produces scratch memories
//! while a workflow is in progress can call `forget` at the end to keep the
//! account's storage quota clean. The relayer enforces a 1 GB per-account
//! storage cap, so long-running pipelines that ingest data without periodic
//! cleanup will eventually start failing with HTTP 402 (Quota Exceeded).
//!
//! ## Configuration
//!
//! Same as `remember`: `MEMWAL_DELEGATE_PRIVATE_KEY`, `MEMWAL_ACCOUNT_ID`,
//! `MEMWAL_SERVER_URL`. See `client.rs` for the resolution order.

use {
    crate::client::MemWalClient,
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Namespace to clear. When omitted the server interprets it as
    /// `"default"`. The relayer scopes the delete to the authenticated
    /// account — you cannot delete other accounts' memories.
    #[serde(default)]
    namespace: Option<String>,
    /// Override the relayer URL for this invocation. Falls back to
    /// `MEMWAL_SERVER_URL` env var, then the compiled-in default.
    #[serde(default)]
    server_url: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Namespace was cleared. `deleted` is the number of memories removed
    /// (zero is a valid success — the namespace existed but was empty, or
    /// did not exist at all).
    Ok {
        deleted: u64,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct ForgetMemories {
    default_api_base: String,
    account_id: String,
    private_key_hex: String,
}

impl NexusTool for ForgetMemories {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        let client = MemWalClient::from_env(None);
        Self {
            default_api_base: client.api_base,
            private_key_hex: client.private_key_hex,
            account_id: client.account_id,
        }
    }

    fn fqn() -> ToolFqn {
        fqn!("xyz.taluslabs.memory.memwal.forget@1")
    }

    fn path() -> &'static str {
        "/memory/forget"
    }

    fn description() -> &'static str {
        "Delete every memory in a MemWal namespace owned by the authenticated account."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        let client = MemWalClient::new(
            self.default_api_base.clone(),
            self.private_key_hex.clone(),
            self.account_id.clone(),
        );
        client.validate_key().map_err(|e| anyhow::anyhow!(e))?;
        client
            .health_check()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        let api_base = input
            .server_url
            .unwrap_or_else(|| self.default_api_base.clone());
        let client = MemWalClient::new(api_base, self.private_key_hex.clone(), self.account_id.clone());

        match client.forget(input.namespace.as_deref()).await {
            Ok(deleted) => Output::Ok { deleted },
            Err(e) => Output::Err {
                reason: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, mockito::Server, serde_json::json};

    fn make_tool(server_url: &str) -> ForgetMemories {
        ForgetMemories {
            default_api_base: server_url.to_string(),
            private_key_hex: hex::encode([0x42u8; 32]),
            account_id: String::new(),
        }
    }

    fn forget_input(server: &mockito::ServerGuard, namespace: Option<&str>) -> Input {
        Input {
            namespace: namespace.map(|s| s.to_string()),
            server_url: Some(server.url()),
        }
    }

    /// `invoke` surfaces the server's `deleted` count on a successful call.
    /// Failure mode caught: server's deleted count is silently dropped or replaced with a constant.
    #[tokio::test]
    async fn invoke_returns_deleted_count_on_success() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/forget")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "deleted": 42,
                    "namespace": "scratch",
                    "owner": "0xowner"
                })
                .to_string(),
            )
            .create_async()
            .await;

        match tool.invoke(forget_input(&server, Some("scratch"))).await {
            Output::Ok { deleted } => assert_eq!(deleted, 42),
            Output::Err { reason } => panic!("unexpected Err: {reason}"),
        }
    }

    /// `invoke` returns `Ok { deleted: 0 }` for an empty namespace — a valid success.
    /// Failure mode caught: empty namespace mapped to Err, masking idempotent retries.
    #[tokio::test]
    async fn invoke_returns_zero_for_empty_namespace() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/forget")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"deleted": 0, "namespace": "default", "owner": "0xowner"}).to_string(),
            )
            .create_async()
            .await;

        match tool.invoke(forget_input(&server, None)).await {
            Output::Ok { deleted } => assert_eq!(deleted, 0),
            Output::Err { reason } => panic!("unexpected Err: {reason}"),
        }
    }

    /// `invoke` returns `Err` on a server-side error.
    /// Failure mode caught: HTTP 500 swallowed, returning a misleading zero count.
    #[tokio::test]
    async fn invoke_returns_err_on_server_error() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/forget")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;

        let output = tool.invoke(forget_input(&server, None)).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 500 must produce Err variant"
        );
    }
}
