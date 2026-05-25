//! # `xyz.taluslabs.memory.memwal.recall@1`
//!
//! Nexus Tool that performs a semantic search over stored memories using a
//! natural-language query and returns the closest matches ranked by vector
//! distance (cosine similarity, lower = closer).

use {
    crate::client::{MemWalClient, MemoryResult},
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Natural-language query to search for relevant memories.
    query: String,
    /// Maximum number of results to return.
    limit: Option<u32>,
    /// Namespace to search within. Searches the default namespace when omitted.
    namespace: Option<String>,
}

/// A single memory returned by a recall query.
#[derive(Serialize, JsonSchema)]
pub(crate) struct RecalledMemory {
    /// The stored text of the memory.
    pub(crate) text: String,
    /// Walrus blob identifier for the encrypted memory.
    pub(crate) blob_id: String,
    /// Cosine distance from the query vector — lower means more relevant.
    pub(crate) distance: f64,
}

impl From<MemoryResult> for RecalledMemory {
    fn from(r: MemoryResult) -> Self {
        Self {
            text: r.text,
            blob_id: r.blob_id,
            distance: r.distance,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Memories ranked by relevance. May be empty if nothing matched.
    ///
    /// The `namespace` field is the namespace that was searched, hoisted
    /// to the variant since every result in a single recall belongs to
    /// the same namespace (the relayer does not currently support
    /// cross-namespace recall).
    Ok {
        results: Vec<RecalledMemory>,
        namespace: String,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct RecallMemories {
    client: MemWalClient,
}

impl NexusTool for RecallMemories {
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
            "xyz.taluslabs.memory.memwal.recall@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/memory/recall"
    }

    fn description() -> &'static str {
        "Search stored memories by natural-language query and return the closest matches."
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
        let ns = input.namespace.as_deref().unwrap_or("default").to_string();

        match self
            .client
            .recall(&input.query, input.limit, input.namespace.as_deref())
            .await
        {
            Ok(results) => Output::Ok {
                results: results.into_iter().map(RecalledMemory::from).collect(),
                namespace: ns,
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

    fn make_tool(server_url: &str) -> RecallMemories {
        RecallMemories {
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

    /// `invoke` returns ranked results when the server responds with memories.
    /// Failure mode caught: successful recall response incorrectly mapped to `Err`.
    #[tokio::test]
    async fn invoke_returns_results_on_success() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/recall")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "results": [
                        {
                            "text": "Paris is the capital of France",
                            "blob_id": "blob-1",
                            "distance": 0.12
                        }
                    ]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool
            .invoke(Input {
                query: "capital of France".into(),
                limit: None,
                namespace: None,
            })
            .await;
        match output {
            Output::Ok { results, namespace } => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].text, "Paris is the capital of France");
                assert_eq!(results[0].blob_id, "blob-1");
                assert!((results[0].distance - 0.12).abs() < f64::EPSILON);
                assert_eq!(namespace, "default");
            }
            Output::Err { reason } => panic!("unexpected Err: {reason}"),
        }
    }

    /// `invoke` returns `Ok` with an empty list when no memories match.
    /// Failure mode caught: empty result set treated as an error.
    #[tokio::test]
    async fn invoke_returns_empty_results_on_no_match() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/recall")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({"results": []}).to_string())
            .create_async()
            .await;

        let output = tool
            .invoke(Input {
                query: "something obscure".into(),
                limit: None,
                namespace: None,
            })
            .await;
        assert!(
            matches!(output, Output::Ok { results, .. } if results.is_empty()),
            "empty results must produce Ok with empty vec"
        );
    }

    /// `invoke` returns `Err` on a server-side error.
    /// Failure mode caught: HTTP 500 from the server swallowed, returns empty results.
    #[tokio::test]
    async fn invoke_returns_err_on_server_error() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/recall")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;

        let output = tool
            .invoke(Input {
                query: "anything".into(),
                limit: None,
                namespace: None,
            })
            .await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 500 must produce Err variant"
        );
    }
}
