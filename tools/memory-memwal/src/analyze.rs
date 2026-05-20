//! # `xyz.taluslabs.memory.memwal.analyze@1`
//!
//! Nexus Tool that extracts discrete facts from a text document and stores
//! each fact as an individual memory. The relayer runs an LLM fact-extraction
//! pass internally and enqueues one memory-write job per fact found.
//!
//! This tool returns immediately after the jobs are enqueued — it does not
//! poll for individual job completion. The output indicates how many fact-
//! extraction jobs were submitted, which is useful for downstream monitoring
//! or logging but does not block the DAG on individual Walrus writes.

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
    /// The text from which to extract and store facts.
    text: String,
    /// Namespace to store the extracted facts in. Uses the default namespace
    /// when omitted.
    #[serde(default)]
    namespace: Option<String>,
    /// Override the relayer URL for this invocation.
    #[serde(default)]
    server_url: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Facts were submitted for storage. `job_count` is the number of
    /// individual memory-write jobs enqueued on the server.
    Ok {
        job_count: u32,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct AnalyzeAndRemember {
    default_api_base: String,
    private_key_hex: String,
    account_id: String,
}

impl NexusTool for AnalyzeAndRemember {
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
        fqn!("xyz.taluslabs.memory.memwal.analyze@1")
    }

    fn path() -> &'static str {
        "/memory/analyze"
    }

    fn description() -> &'static str {
        "Extract facts from a text document and store each fact as a separate memory."
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
            .analyze(&input.text, input.namespace.as_deref())
            .await
        {
            Ok(count) => Output::Ok {
                job_count: count as u32,
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

    fn make_tool(server_url: &str) -> AnalyzeAndRemember {
        AnalyzeAndRemember {
            default_api_base: server_url.to_string(),
            private_key_hex: hex::encode([0x42u8; 32]),
            account_id: String::new(),
        }
    }

    fn analyze_input(server: &mockito::ServerGuard, text: &str) -> Input {
        Input {
            text: text.to_string(),
            namespace: None,
            server_url: Some(server.url()),
        }
    }

    /// `invoke` returns `Ok { job_count }` matching the number of job_ids returned
    /// by the server.
    /// Failure mode caught: job_ids list not counted, job_count is always 0 or wrong.
    #[tokio::test]
    async fn invoke_returns_job_count() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/analyze")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_ids": ["j1", "j2", "j3"]}).to_string())
            .create_async()
            .await;

        let output = tool
            .invoke(analyze_input(
                &server,
                "Alice lives in Paris. Bob works at ACME. Paris is in France.",
            ))
            .await;

        assert!(
            matches!(output, Output::Ok { job_count: 3 }),
            "expected Ok{{job_count: 3}}"
        );
    }

    /// `invoke` returns `Ok { job_count: 0 }` when the server returns no jobs
    /// (e.g., no facts extracted from the text).
    /// Failure mode caught: empty job_ids list treated as an error.
    #[tokio::test]
    async fn invoke_returns_zero_for_empty_job_ids() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/analyze")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_ids": []}).to_string())
            .create_async()
            .await;

        let output = tool.invoke(analyze_input(&server, "...")).await;
        assert!(
            matches!(output, Output::Ok { job_count: 0 }),
            "empty job_ids must produce Ok{{job_count: 0}}"
        );
    }

    /// `invoke` returns `Err` on a server-side error.
    /// Failure mode caught: HTTP error from analyze endpoint swallowed.
    #[tokio::test]
    async fn invoke_returns_err_on_server_error() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/analyze")
            .with_status(500)
            .with_body("error")
            .create_async()
            .await;

        let output = tool.invoke(analyze_input(&server, "any text")).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 500 must produce Err variant"
        );
    }
}
