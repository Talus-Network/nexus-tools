//! # `xyz.taluslabs.memory.memwal.remember_bulk@1`
//!
//! Nexus Tool that stores up to 20 memories in a single batched call. Returns
//! a confirmed Walrus `blob_id` per input item, in the same order. Suitable
//! for DAG vertices that produce many memories at once (e.g. an extraction
//! pipeline emitting multiple structured facts).
//!
//! ## Why bulk
//!
//! The relayer rate-limits `/api/remember` at weight 5 per call, but
//! `/api/remember/bulk` costs weight 10 for up to 20 items. That's a 10×
//! efficiency gain in rate-limit budget when batching is feasible. The
//! relayer's MAX_BULK_ITEMS = 20 cap is enforced server-side; this tool
//! does not split larger inputs — callers must chunk if they need more.
//!
//! ## Async behaviour
//!
//! The call blocks until every item is durably written to Walrus. Internally
//! the bulk endpoint returns one `job_id` per item; the tool then polls
//! `POST /api/remember/bulk/status` until every job reaches a terminal
//! state. A single failed job fails the whole batch (returns `Err`). For
//! partial-success semantics, call the items individually instead.

use {
    crate::client::MemWalClient,
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

/// One item in the bulk request. `namespace` is per-item: a single bulk call
/// can write to multiple namespaces in one go.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BulkItem {
    /// The text to store.
    text: String,
    /// Namespace for this specific item. Defaults to `"default"` on the
    /// server when omitted.
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// 1–20 items to store. The relayer rejects empty arrays and any batch
    /// over MAX_BULK_ITEMS = 20.
    items: Vec<BulkItem>,
    /// Override the relayer URL for this invocation.
    #[serde(default)]
    server_url: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Every item was durably stored. `blob_ids` aligns positionally with the
    /// input `items` array (i.e. `blob_ids[i]` is the blob for `items[i]`).
    Ok {
        blob_ids: Vec<String>,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct RememberBulkMemories {
    default_api_base: String,
    account_id: String,
    private_key_hex: String,
}

impl NexusTool for RememberBulkMemories {
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
        fqn!("xyz.taluslabs.memory.memwal.remember_bulk@1")
    }

    fn path() -> &'static str {
        "/memory/remember_bulk"
    }

    fn description() -> &'static str {
        "Store up to 20 memories in one batched call; returns one blob_id per item."
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

        // Map our owned `Vec<BulkItem>` into the borrowed `(&str, Option<&str>)`
        // tuple shape the client expects. This avoids `Item<'_>` lifetime gymnastics
        // by keeping the inputs alive in `items_refs`.
        let items_refs: Vec<(&str, Option<&str>)> = input
            .items
            .iter()
            .map(|i| (i.text.as_str(), i.namespace.as_deref()))
            .collect();

        let job_ids = match client.remember_bulk(&items_refs).await {
            Ok(ids) => ids,
            Err(e) => {
                return Output::Err {
                    reason: e.to_string(),
                };
            }
        };

        match client.poll_bulk_jobs(&job_ids).await {
            Ok(blob_ids) => Output::Ok { blob_ids },
            Err(e) => Output::Err {
                reason: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, mockito::Server, serde_json::json};

    fn make_tool(server_url: &str) -> RememberBulkMemories {
        RememberBulkMemories {
            default_api_base: server_url.to_string(),
            private_key_hex: hex::encode([0x42u8; 32]),
            account_id: String::new(),
        }
    }

    fn bulk_input(server: &mockito::ServerGuard, texts: &[&str]) -> Input {
        Input {
            items: texts
                .iter()
                .map(|t| BulkItem {
                    text: t.to_string(),
                    namespace: None,
                })
                .collect(),
            server_url: Some(server.url()),
        }
    }

    /// `invoke` returns `Ok { blob_ids }` aligned with the input order when
    /// every job reaches "done".
    /// Failure mode caught: blob_ids returned in random order, breaking
    /// callers that index-correlate items with results.
    #[tokio::test]
    async fn invoke_returns_blob_ids_in_input_order() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _bulk = server
            .mock("POST", "/api/remember/bulk")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "job_ids": ["j1", "j2", "j3"],
                    "total": 3,
                    "status": "running"
                })
                .to_string(),
            )
            .create_async()
            .await;

        // Server returns results in reversed order intentionally — the tool
        // must reorder by job_id to align with the input order.
        let _status = server
            .mock("POST", "/api/remember/bulk/status")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "results": [
                        {"job_id": "j3", "status": "done", "blob_id": "blob3"},
                        {"job_id": "j2", "status": "done", "blob_id": "blob2"},
                        {"job_id": "j1", "status": "done", "blob_id": "blob1"},
                    ]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool
            .invoke(bulk_input(&server, &["one", "two", "three"]))
            .await;
        match output {
            Output::Ok { blob_ids } => {
                assert_eq!(blob_ids, vec!["blob1", "blob2", "blob3"]);
            }
            Output::Err { reason } => panic!("unexpected Err: {reason}"),
        }
    }

    /// `invoke` returns `Err` when any individual job ends `failed`.
    /// Failure mode caught: a partial failure is silently presented as
    /// success with truncated blob_ids, hiding data loss.
    #[tokio::test]
    async fn invoke_returns_err_on_any_job_failure() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _bulk = server
            .mock("POST", "/api/remember/bulk")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"job_ids": ["jA", "jB"], "total": 2, "status": "running"}).to_string(),
            )
            .create_async()
            .await;

        let _status = server
            .mock("POST", "/api/remember/bulk/status")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "results": [
                        {"job_id": "jA", "status": "done", "blob_id": "blobA"},
                        {"job_id": "jB", "status": "failed", "error": "encrypt timeout"},
                    ]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(bulk_input(&server, &["a", "b"])).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "any failed job must produce Err"
        );
    }

    /// `invoke` returns `Err` when the initial bulk POST fails (4xx/5xx).
    /// Failure mode caught: the initial-submit error is swallowed, e.g. an
    /// over-cap batch (>20) returning 400 mapped to silent success.
    #[tokio::test]
    async fn invoke_returns_err_on_submit_failure() {
        let mut server = Server::new_async().await;
        let tool = make_tool(&server.url());

        let _bulk = server
            .mock("POST", "/api/remember/bulk")
            .with_status(400)
            .with_body("too many items")
            .create_async()
            .await;

        let output = tool.invoke(bulk_input(&server, &["x"])).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "submit-time 400 must produce Err"
        );
    }
}
