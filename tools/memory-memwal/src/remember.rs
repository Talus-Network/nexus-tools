//! # `xyz.taluslabs.memory.memwal.remember@1`
//!
//! Nexus Tool that stores a single piece of text as a persistent memory in
//! MemWal. The call blocks until the memory is durably stored on Walrus and
//! returns the resulting blob ID, making it safe to chain in a Nexus DAG —
//! the next vertex will not activate until the write is confirmed.
//!
//! ## Configuration
//!
//! The Ed25519 delegate private key is read from `MEMWAL_DELEGATE_PRIVATE_KEY`
//! (hex-encoded, 32 bytes). The relayer URL follows the three-level resolution
//! described in `client.rs`.

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
    /// The text to store as a memory.
    text: String,
    /// Namespace used to scope this memory. Defaults to `"default"` on the
    /// server when omitted.
    #[serde(default)]
    namespace: Option<String>,
    /// Override the relayer URL for this invocation. Falls back to
    /// `MEMWAL_SERVER_URL` env var, then `https://relayer.memwal.ai`.
    #[serde(default)]
    server_url: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Memory was durably stored. `blob_id` is its Walrus blob identifier.
    Ok {
        blob_id: String,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct RememberMemory {
    /// Relayer URL resolved from env at startup; can be overridden per-call.
    default_api_base: String,
    account_id: String,
    /// Hex-encoded Ed25519 delegate private key.
    private_key_hex: String,
}

impl NexusTool for RememberMemory {
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
        fqn!("xyz.taluslabs.memory.memwal.remember@1")
    }

    fn path() -> &'static str {
        "/memory/remember"
    }

    fn description() -> &'static str {
        "Store a text memory in MemWal and return its blob ID once durably written."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        let client = MemWalClient::new(
            self.default_api_base.clone(),
            self.private_key_hex.clone(),
            self.account_id.clone(),
        );
        // Key must be present and valid before any call can succeed.
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

        let job_id = match client
            .remember(&input.text, input.namespace.as_deref())
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return Output::Err {
                    reason: e.to_string(),
                }
            }
        };

        match client.poll_job(&job_id).await {
            Ok(blob_id) => Output::Ok { blob_id },
            Err(e) => Output::Err {
                reason: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        mockito::{Matcher, Server},
        serde_json::json,
    };

    fn make_tool(server_url: &str) -> RememberMemory {
        RememberMemory {
            default_api_base: server_url.to_string(),
            private_key_hex: hex::encode([0x42u8; 32]),
            account_id: String::new(),
        }
    }

    fn remember_input(server: &mockito::ServerGuard, text: &str) -> Input {
        Input {
            text: text.to_string(),
            namespace: None,
            server_url: Some(server.url()),
        }
    }

    /// `invoke` returns `Ok { blob_id }` when the job completes successfully.
    /// Failure mode caught: successful server response mapped to `Err` variant.
    #[tokio::test]
    async fn invoke_returns_blob_id_on_success() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m1 = server
            .mock("POST", "/api/remember")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_id": "job-abc", "status": "pending"}).to_string())
            .create_async()
            .await;

        let _m2 = server
            .mock("GET", "/api/remember/job-abc")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"job_id": "job-abc", "status": "done", "blob_id": "blob-xyz"})
                    .to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(remember_input(&server, "hello world")).await;
        assert!(
            matches!(output, Output::Ok { blob_id } if blob_id == "blob-xyz"),
            "expected Ok{{blob_id: blob-xyz}}"
        );
    }

    /// `invoke` returns `Err` when the server signals job failure.
    /// Failure mode caught: failed job silently returns empty blob_id instead of error.
    #[tokio::test]
    async fn invoke_returns_err_on_job_failure() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m1 = server
            .mock("POST", "/api/remember")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_id": "job-fail", "status": "pending"}).to_string())
            .create_async()
            .await;

        let _m2 = server
            .mock("GET", "/api/remember/job-fail")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_id": "job-fail", "status": "failed"}).to_string())
            .create_async()
            .await;

        let output = tool.invoke(remember_input(&server, "will fail")).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "failed job must produce Err variant"
        );
    }

    /// `invoke` returns `Err` when the POST to `/api/remember` itself fails (4xx/5xx).
    /// Failure mode caught: HTTP error from the initial POST is swallowed.
    #[tokio::test]
    async fn invoke_returns_err_on_server_error() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m = server
            .mock("POST", "/api/remember")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;

        let output = tool
            .invoke(remember_input(&server, "text that will not be stored"))
            .await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 500 must produce Err variant"
        );
    }

    /// `invoke` sends the `text` field in the POST body.
    /// Failure mode caught: body serialisation drops the `text` field.
    #[tokio::test]
    async fn invoke_sends_correct_body() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m1 = server
            .mock("POST", "/api/remember")
            .match_body(Matcher::PartialJson(json!({"text": "exact text"})))
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_id": "j1", "status": "pending"}).to_string())
            .create_async()
            .await;

        let _m2 = server
            .mock("GET", "/api/remember/j1")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_id": "j1", "status": "done", "blob_id": "b1"}).to_string())
            .create_async()
            .await;

        let output = tool.invoke(remember_input(&server, "exact text")).await;
        assert!(matches!(output, Output::Ok { .. }));
    }
}
