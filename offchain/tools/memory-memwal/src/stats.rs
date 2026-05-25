//! # `xyz.taluslabs.memory.memwal.stats@1`
//!
//! Nexus Tool that reports per-namespace usage for the authenticated MemWal
//! account: how many memories are stored and how many bytes they take.

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
    /// Namespace to report on. Defaults to `"default"` on the server when
    /// omitted. Only namespaces owned by the authenticated account return
    /// meaningful data.
    namespace: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Per-namespace usage figures.
    Ok {
        /// Number of memories stored in this namespace.
        memory_count: i64,
        /// Total encrypted byte size of those memories on Walrus.
        storage_bytes: i64,
        /// The resolved namespace (mirrors what the server interpreted).
        namespace: String,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct StatsForAccount {
    client: MemWalClient,
}

impl NexusTool for StatsForAccount {
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
            "xyz.taluslabs.memory.memwal.stats@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/memory/stats"
    }

    fn description() -> &'static str {
        "Report memory count and total stored bytes for a MemWal namespace."
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
        match self.client.stats(input.namespace.as_deref()).await {
            Ok(s) => Output::Ok {
                memory_count: s.memory_count,
                storage_bytes: s.storage_bytes,
                namespace: s.namespace,
            },
            Err(e) => Output::Err {
                reason: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, mockito::Server, serde_json::json};

    fn make_tool(server_url: &str) -> StatsForAccount {
        StatsForAccount {
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

    /// `invoke` echoes the server's count + bytes verbatim on a healthy response.
    /// Failure mode caught: numeric fields silently zeroed or truncated on the boundary.
    #[tokio::test]
    async fn invoke_returns_counts_on_success() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/stats")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "memory_count": 17,
                    "storage_bytes": 5_242_880,
                    "namespace": "people",
                    "owner": "0xowner"
                })
                .to_string(),
            )
            .create_async()
            .await;

        match tool
            .invoke(Input {
                namespace: Some("people".into()),
            })
            .await
        {
            Output::Ok {
                memory_count,
                storage_bytes,
                namespace,
            } => {
                assert_eq!(memory_count, 17);
                assert_eq!(storage_bytes, 5_242_880);
                assert_eq!(namespace, "people");
            }
            Output::Err { reason } => panic!("unexpected Err: {reason}"),
        }
    }

    /// `invoke` returns `Ok` with zeros for a namespace that exists but is empty.
    /// Failure mode caught: zero-memory namespaces misclassified as Err.
    #[tokio::test]
    async fn invoke_handles_zero_counts() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/stats")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "memory_count": 0,
                    "storage_bytes": 0,
                    "namespace": "empty-ns",
                    "owner": "0xowner"
                })
                .to_string(),
            )
            .create_async()
            .await;

        match tool
            .invoke(Input {
                namespace: Some("empty-ns".into()),
            })
            .await
        {
            Output::Ok {
                memory_count,
                storage_bytes,
                ..
            } => {
                assert_eq!(memory_count, 0);
                assert_eq!(storage_bytes, 0);
            }
            Output::Err { reason } => panic!("unexpected Err: {reason}"),
        }
    }

    /// `invoke` returns `Err` on server error.
    /// Failure mode caught: HTTP 500 swallowed, returning misleading zero stats.
    #[tokio::test]
    async fn invoke_returns_err_on_server_error() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/stats")
            .with_status(503)
            .with_body("service unavailable")
            .create_async()
            .await;

        let output = tool.invoke(Input { namespace: None }).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 503 must produce Err variant"
        );
    }
}
