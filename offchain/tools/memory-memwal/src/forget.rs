//! # `xyz.taluslabs.memory.memwal.forget@1`
//!
//! Nexus Tool that deletes every memory in a MemWal namespace. The call is
//! owner-scoped — the relayer's auth middleware identifies the account from
//! the signed request; only memories belonging to that account are removed,
//! regardless of which namespace string is sent.

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
    namespace: Option<String>,
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
    client: MemWalClient,
}

impl NexusTool for ForgetMemories {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        let client = MemWalClient::from_env().unwrap_or_else(|e| {
            log::error!("relayer configuration invalid: {e}");
            panic!("relayer configuration invalid: {e}")
        });
        Self { client }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.memory.memwal.forget@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/memory/forget"
    }

    fn description() -> &'static str {
        "Delete every memory in a MemWal namespace owned by the authenticated account."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        self.client.validate_key().map_err(|e| anyhow::anyhow!(e))?;
        self.client
            .health_check()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        match self.client.forget(input.namespace.as_deref()).await {
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
            client: MemWalClient::with_test_config(server_url, &hex::encode([0x42u8; 32]), ""),
        }
    }

    /// `health()` returns `Ok` when the relayer reports the matching API version.
    /// Failure mode caught: the tool's NexusTool::health wrapper drops the
    /// validate_key + health_check composition.
    #[tokio::test]
    async fn health_returns_ok_when_relayer_healthy() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/health")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"status": "ok", "version": crate::client::MEMWAL_API_VERSION}).to_string(),
            )
            .create_async()
            .await;
        let tool = make_tool(&server.url());
        assert!(tool.health().await.is_ok());
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

        match tool
            .invoke(Input {
                namespace: Some("scratch".into()),
            })
            .await
        {
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

        match tool.invoke(Input { namespace: None }).await {
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

        let output = tool.invoke(Input { namespace: None }).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 500 must produce Err variant"
        );
    }
}
