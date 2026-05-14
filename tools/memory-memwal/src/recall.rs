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
    #[serde(default)]
    limit: Option<u32>,
    /// Namespace to search within. Searches the default namespace when omitted.
    #[serde(default)]
    namespace: Option<String>,
    /// Override the relayer URL for this invocation.
    #[serde(default)]
    server_url: Option<String>,
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
    /// Namespace the memory belongs to (same as the query namespace).
    ///
    /// The relayer does not echo the namespace per result; this field is
    /// populated from the recall request's `namespace` input (defaulting to
    /// `"default"` when omitted), which is always the namespace searched.
    pub(crate) namespace: String,
}

impl RecalledMemory {
    fn from_result(r: MemoryResult, namespace: &str) -> Self {
        Self {
            text: r.text,
            blob_id: r.blob_id,
            distance: r.distance,
            namespace: namespace.to_string(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Memories ranked by relevance. May be empty if nothing matched.
    Ok {
        results: Vec<RecalledMemory>,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct RecallMemories {
    default_api_base: String,
    private_key_hex: String,
}

impl NexusTool for RecallMemories {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        let client = MemWalClient::from_env(None);
        Self {
            default_api_base: client.api_base,
            private_key_hex: client.private_key_hex,
        }
    }

    fn fqn() -> ToolFqn {
        fqn!("xyz.taluslabs.memory.memwal.recall@1")
    }

    fn path() -> &'static str {
        "/memory/recall"
    }

    fn description() -> &'static str {
        "Search stored memories by natural-language query and return the closest matches."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        let client = MemWalClient::new(self.default_api_base.clone(), self.private_key_hex.clone());
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
        let client = MemWalClient::new(api_base, self.private_key_hex.clone());

        // Resolve the effective namespace so we can populate it on each result
        // (the relayer does not echo it per-item in the response).
        let ns = input
            .namespace
            .as_deref()
            .unwrap_or("default")
            .to_string();

        match client
            .recall(&input.query, input.limit, input.namespace.as_deref())
            .await
        {
            Ok(results) => Output::Ok {
                results: results
                    .into_iter()
                    .map(|r| RecalledMemory::from_result(r, &ns))
                    .collect(),
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
            default_api_base: server_url.to_string(),
            private_key_hex: hex::encode([0x42u8; 32]),
        }
    }

    fn recall_input(server: &mockito::ServerGuard, query: &str) -> Input {
        Input {
            query: query.to_string(),
            limit: None,
            namespace: None,
            server_url: Some(server.url()),
        }
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
                            "distance": 0.12,
                            "namespace": "default"
                        }
                    ]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool
            .invoke(recall_input(&server, "capital of France"))
            .await;
        match output {
            Output::Ok { results } => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].text, "Paris is the capital of France");
                assert_eq!(results[0].blob_id, "blob-1");
                assert!((results[0].distance - 0.12).abs() < f64::EPSILON);
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
            .invoke(recall_input(&server, "something obscure"))
            .await;
        assert!(
            matches!(output, Output::Ok { results } if results.is_empty()),
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

        let output = tool.invoke(recall_input(&server, "anything")).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 500 must produce Err variant"
        );
    }
}
