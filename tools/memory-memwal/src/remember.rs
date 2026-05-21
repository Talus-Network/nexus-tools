//! # `xyz.taluslabs.memory.memwal.remember@1`
//!
//! Nexus Tool that stores a single piece of text as a persistent memory in
//! MemWal. The call blocks until the memory is durably stored on Walrus and
//! returns the resulting blob ID, making it safe to chain in a Nexus DAG —
//! the next vertex will not activate until the write is confirmed.

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
    namespace: Option<String>,
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
    client: MemWalClient,
}

impl NexusTool for RememberMemory {
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
        fqn!("xyz.taluslabs.memory.memwal.remember@1")
    }

    fn path() -> &'static str {
        "/memory/remember"
    }

    fn description() -> &'static str {
        "Store a text memory in MemWal and return its blob ID once durably written."
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
        let job_id = match self
            .client
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

        match self.client.poll_job(&job_id).await {
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
            client: MemWalClient::with_test_config(server_url, &hex::encode([0x42u8; 32]), ""),
        }
    }

    /// `health()` returns `Ok` when the relayer reports the matching API version.
    /// Failure mode caught: the tool's `NexusTool::health` wrapper drops the
    /// validate_key + health_check composition (e.g. a refactor that forgets
    /// to await the inner future) — only an end-to-end test through the trait
    /// catches that.
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
                json!({"job_id": "job-abc", "status": "done", "blob_id": "blob-xyz"}).to_string(),
            )
            .create_async()
            .await;

        let output = tool
            .invoke(Input {
                text: "hello world".into(),
                namespace: None,
            })
            .await;
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

        let output = tool
            .invoke(Input {
                text: "will fail".into(),
                namespace: None,
            })
            .await;
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
            .invoke(Input {
                text: "text that will not be stored".into(),
                namespace: None,
            })
            .await;
        assert!(
            matches!(output, Output::Err { .. }),
            "server 500 must produce Err variant"
        );
    }

    /// `invoke` returns `Err` when the server replies `"done"` without a
    /// `blob_id` field — the missing identifier must NOT be silently coerced
    /// to an empty string.
    /// Failure mode caught: a regression of `unwrap_or_default()` in
    /// `poll_job` would propagate `blob_id: ""` to downstream DAG vertices
    /// as if the write had succeeded.
    #[tokio::test]
    async fn invoke_returns_err_when_done_missing_blob_id() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _m1 = server
            .mock("POST", "/api/remember")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_id": "job-noid", "status": "pending"}).to_string())
            .create_async()
            .await;

        // status=done but no blob_id field.
        let _m2 = server
            .mock("GET", "/api/remember/job-noid")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({"job_id": "job-noid", "status": "done"}).to_string())
            .create_async()
            .await;

        let output = tool
            .invoke(Input {
                text: "text".into(),
                namespace: None,
            })
            .await;
        match output {
            Output::Err { reason } => {
                assert!(
                    reason.contains("blob_id"),
                    "error must mention missing blob_id; got: {reason}"
                );
            }
            Output::Ok { blob_id } => {
                panic!("expected Err, got Ok(blob_id={blob_id:?}); silent empty-blob bug")
            }
        }
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

        let output = tool
            .invoke(Input {
                text: "exact text".into(),
                namespace: None,
            })
            .await;
        assert!(matches!(output, Output::Ok { .. }));
    }
}
