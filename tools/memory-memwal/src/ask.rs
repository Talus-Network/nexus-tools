//! # `xyz.taluslabs.memory.memwal.ask@1`
//!
//! Nexus Tool for memory-augmented Q&A. The relayer retrieves the most
//! relevant stored memories for the question, injects them into an LLM prompt,
//! and returns both the generated answer and the source memories that informed
//! it.

use {
    crate::client::{AskSource, MemWalClient},
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// The question to answer using stored memories as context.
    question: String,
    /// Namespace to retrieve memories from. Uses the default namespace when omitted.
    #[serde(default)]
    namespace: Option<String>,
    /// Maximum number of source memories to inject as context.
    #[serde(default)]
    limit: Option<u32>,
    /// Override the relayer URL for this invocation.
    #[serde(default)]
    server_url: Option<String>,
}

/// A memory that contributed to the answer.
#[derive(Serialize, JsonSchema)]
pub(crate) struct AnswerSource {
    /// Walrus blob identifier for the encrypted memory.
    blob_id: String,
    /// The stored text of the memory.
    text: String,
    /// Cosine distance from the question vector — lower means more relevant.
    distance: f64,
}

impl From<AskSource> for AnswerSource {
    fn from(s: AskSource) -> Self {
        Self {
            blob_id: s.blob_id,
            text: s.text,
            distance: s.distance,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// LLM-generated answer and the memories used as context.
    Ok {
        answer: String,
        sources: Vec<AnswerSource>,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct AskMemory {
    default_api_base: String,
    private_key_hex: String,
    account_id: String,
}

impl NexusTool for AskMemory {
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
        fqn!("xyz.taluslabs.memory.memwal.ask@1")
    }

    fn path() -> &'static str {
        "/memory/ask"
    }

    fn description() -> &'static str {
        "Answer a question by retrieving relevant memories and generating a response with an LLM."
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

        match client
            .ask(&input.question, input.namespace.as_deref(), input.limit)
            .await
        {
            Ok(resp) => Output::Ok {
                answer: resp.answer,
                sources: resp.memories.into_iter().map(AnswerSource::from).collect(),
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

    fn make_tool(server_url: &str) -> AskMemory {
        AskMemory {
            default_api_base: server_url.to_string(),
            private_key_hex: hex::encode([0x42u8; 32]),
            account_id: String::new(),
        }
    }

    fn ask_input(server: &mockito::ServerGuard, question: &str) -> Input {
        Input {
            question: question.to_string(),
            namespace: None,
            limit: None,
            server_url: Some(server.url()),
        }
    }

    /// `invoke` returns the LLM answer and its sources on a successful call.
    /// Failure mode caught: successful ask response incorrectly mapped to `Err`.
    #[tokio::test]
    async fn invoke_returns_answer_and_sources() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/ask")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "answer": "Paris",
                    "memories_used": 1,
                    "memories": [
                        {
                            "blob_id": "blob-1",
                            "text": "Paris is the capital of France",
                            "distance": 0.05
                        }
                    ]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool
            .invoke(ask_input(&server, "What is the capital of France?"))
            .await;
        match output {
            Output::Ok { answer, sources } => {
                assert_eq!(answer, "Paris");
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].text, "Paris is the capital of France");
                assert_eq!(sources[0].blob_id, "blob-1");
            }
            Output::Err { reason } => panic!("unexpected Err: {reason}"),
        }
    }

    /// `invoke` returns `Ok` with an empty sources list when the server returns none.
    /// Failure mode caught: empty sources array causes a parse error or Err variant.
    #[tokio::test]
    async fn invoke_handles_empty_sources() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/ask")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"answer": "I don't know", "memories_used": 0, "memories": []}).to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(ask_input(&server, "unknown topic")).await;
        assert!(
            matches!(output, Output::Ok { sources, .. } if sources.is_empty()),
            "empty sources must produce Ok with empty vec"
        );
    }

    /// `invoke` returns `Err` on a server-side error.
    /// Failure mode caught: HTTP 500 swallowed, returning a garbage answer.
    #[tokio::test]
    async fn invoke_returns_err_on_server_error() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/ask")
            .with_status(503)
            .with_body("service unavailable")
            .create_async()
            .await;

        let output = tool.invoke(ask_input(&server, "anything")).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 503 must produce Err variant"
        );
    }
}
