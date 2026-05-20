//! # `xyz.taluslabs.memory.memwal.remember_bulk@1`
//!
//! Nexus Tool that stores up to 20 memories in a single batched call. Returns
//! a confirmed Walrus `blob_id` per input item, in the same order. Suitable
//! for DAG vertices that produce many memories at once (e.g. an extraction
//! pipeline emitting multiple structured facts).
//!
//! The relayer rate-limits `/api/remember` at weight 5 per call, but
//! `/api/remember/bulk` costs weight 10 for up to 20 items. That's a 10×
//! efficiency gain in rate-limit budget when batching is feasible.

use {
    crate::client::{MemWalClient, MAX_BULK_ITEMS},
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
    namespace: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// 1–20 items to store. The relayer rejects empty arrays and any batch
    /// over MAX_BULK_ITEMS = 20.
    items: Vec<BulkItem>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Every item was durably stored. `blob_ids` aligns positionally with the
    /// input `items` array (i.e. `blob_ids[i]` is the blob for `items[i]`).
    Ok { blob_ids: Vec<String> },
    Err { reason: String },
}

pub(crate) struct RememberBulkMemories {
    client: MemWalClient,
}

impl NexusTool for RememberBulkMemories {
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
        fqn!("xyz.taluslabs.memory.memwal.remember_bulk@1")
    }

    fn path() -> &'static str {
        "/memory/remember_bulk"
    }

    fn description() -> &'static str {
        "Store up to 20 memories in one batched call; returns one blob_id per item."
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
        // Validate batch size before signing anything — empty arrays waste a
        // signed request slot; oversized batches surface as opaque HTTP 400
        // from the relayer without this guard.
        if input.items.is_empty() {
            return Output::Err {
                reason: "bulk batch must contain at least 1 item".into(),
            };
        }
        if input.items.len() > MAX_BULK_ITEMS {
            return Output::Err {
                reason: format!(
                    "bulk batch capped at {MAX_BULK_ITEMS} items, got {}",
                    input.items.len()
                ),
            };
        }

        // Map our owned `Vec<BulkItem>` into the borrowed `(&str, Option<&str>)`
        // tuple shape the client expects. This avoids `Item<'_>` lifetime
        // gymnastics by keeping the inputs alive in `items_refs`.
        let items_refs: Vec<(&str, Option<&str>)> = input
            .items
            .iter()
            .map(|i| (i.text.as_str(), i.namespace.as_deref()))
            .collect();

        let job_ids = match self.client.remember_bulk(&items_refs).await {
            Ok(ids) => ids,
            Err(e) => {
                return Output::Err {
                    reason: e.to_string(),
                };
            }
        };

        match self.client.poll_bulk_jobs(&job_ids).await {
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
            client: MemWalClient::with_test_config(server_url, &hex::encode([0x42u8; 32]), ""),
        }
    }

    fn bulk_input(texts: &[&str]) -> Input {
        Input {
            items: texts
                .iter()
                .map(|t| BulkItem {
                    text: t.to_string(),
                    namespace: None,
                })
                .collect(),
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

        let output = tool.invoke(bulk_input(&["one", "two", "three"])).await;
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

        let output = tool.invoke(bulk_input(&["a", "b"])).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "any failed job must produce Err"
        );
    }

    /// `invoke` returns `Err` for an empty `items` array without hitting the network.
    /// Failure mode caught: an empty batch silently round-trips, burning a
    /// signed request slot and a rate-limit point for an opaque 400.
    #[tokio::test]
    async fn invoke_returns_err_on_empty_items() {
        let server = Server::new_async().await;
        let tool = make_tool(&server.url());
        let output = tool.invoke(bulk_input(&[])).await;
        match output {
            Output::Err { reason } => {
                assert!(
                    reason.contains("1 item"),
                    "reason must mention the lower bound; got: {reason}"
                );
            }
            Output::Ok { .. } => panic!("empty batch must produce Err"),
        }
    }

    /// `invoke` returns `Err` for a batch of MAX_BULK_ITEMS+1 items without
    /// contacting the relayer.
    /// Failure mode caught: oversized batch reaches the relayer and gets an
    /// opaque HTTP 400 instead of a tool-side cap explanation.
    #[tokio::test]
    async fn invoke_returns_err_on_oversized_batch() {
        let server = Server::new_async().await;
        let tool = make_tool(&server.url());
        let texts: Vec<String> = (0..MAX_BULK_ITEMS + 1).map(|i| format!("item-{i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let output = tool.invoke(bulk_input(&refs)).await;
        match output {
            Output::Err { reason } => {
                assert!(
                    reason.contains(&MAX_BULK_ITEMS.to_string()),
                    "reason must mention the cap; got: {reason}"
                );
            }
            Output::Ok { .. } => panic!("oversized batch must produce Err"),
        }
    }

    /// `invoke` returns `Err` when the initial bulk POST fails (4xx/5xx).
    /// Failure mode caught: the initial-submit error is swallowed.
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

        let output = tool.invoke(bulk_input(&["x"])).await;
        assert!(
            matches!(output, Output::Err { .. }),
            "submit-time 400 must produce Err"
        );
    }
}
