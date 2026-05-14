//! MemWal HTTP client.
//!
//! ## URL resolution
//!
//! The server URL is resolved in this order:
//!   1. Explicit value passed via the tool's JSON input (`server_url` field).
//!   2. `MEMWAL_SERVER_URL` environment variable — set on the container so ops
//!      can point all tools at a staging or self-hosted relayer without
//!      changing every caller.
//!   3. The public production relayer (`https://relayer.memwal.ai`).
//!
//! ## Authentication
//!
//! The delegate private key is loaded from `MEMWAL_DELEGATE_PRIVATE_KEY` (hex-
//! encoded Ed25519 scalar). It is never accepted as a tool input because tool
//! inputs flow through the Nexus DAG as on-chain data and may be visible to
//! Leader nodes and on-chain auditors.  Credentials that identify the tool's
//! own service identity belong in the deployment environment, not the data flow.
//!
//! ## Async job polling
//!
//! Several MemWal endpoints return a `job_id` and a 202 Accepted status.
//! [`MemWalClient::poll_job`] retries `GET /api/remember/:job_id` until the
//! job reaches `completed` or `failed`, or a timeout is exceeded.

use {
    crate::{
        auth::sign_request,
        error::{AuthError, MemWalError},
    },
    serde::{Deserialize, Serialize},
    std::time::Duration,
    tokio::time::sleep,
};

/// MemWal relayer version this crate was written against.
///
/// Source: `GET /health` → `{"status":"ok","version":"0.1.0"}`.
/// The Rust server (`services/server/`) has not been formally tagged in the
/// MystenLabs/MemWal repository; the API was derived from
/// `services/server/src/types.rs` at commit
/// `a3469abd7f35895d7156b38dc9058d9c458acd47` on `main`.
/// There is no published OpenAPI spec; the docs at <https://docs.memwal.ai>
/// describe the API in prose only.
///
/// Update this string — and the matching constant in `memwal-test-server` —
/// whenever the relayer API contract changes (new endpoints, changed request /
/// response shapes, auth header format, job-poll semantics). The
/// `api_version_constants_match` integration test asserts both are equal so a
/// mismatch surfaces as a test failure rather than a silent divergence.
pub(crate) const MEMWAL_API_VERSION: &str = "0.1.0";

/// Default MemWal production relayer URL.
pub(crate) const DEFAULT_SERVER_URL: &str = "https://relayer.memwal.ai";

/// Env var for overriding the relayer URL at the operator level.
pub(crate) const ENV_SERVER_URL: &str = "MEMWAL_SERVER_URL";

/// Env var carrying the hex-encoded Ed25519 delegate private key.
pub(crate) const ENV_PRIVATE_KEY: &str = "MEMWAL_DELEGATE_PRIVATE_KEY";

/// Delay between job-status polls.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum number of poll attempts before giving up (~15 s total).
const MAX_POLLS: u32 = 30;

// ---------------------------------------------------------------------------
// API response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct RememberResponse {
    pub(crate) job_id: String,
}

#[derive(Deserialize)]
pub(crate) struct JobStatusResponse {
    /// "pending" | "running" | "uploaded" | "done" | "failed"
    pub(crate) status: String,
    pub(crate) blob_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct AnalyzeResponse {
    pub(crate) job_ids: Vec<String>,
}

/// One result item from `POST /api/recall` (`RecallResult` in server types).
/// The server does not include `namespace` in per-result items.
#[derive(Deserialize, Clone)]
pub(crate) struct MemoryResult {
    pub(crate) text: String,
    pub(crate) blob_id: String,
    pub(crate) distance: f64,
}

#[derive(Deserialize)]
pub(crate) struct RecallResponse {
    pub(crate) results: Vec<MemoryResult>,
}

/// One source memory from `POST /api/ask` (`RecallResult` in server types).
#[derive(Deserialize, Clone)]
pub(crate) struct AskSource {
    pub(crate) blob_id: String,
    pub(crate) text: String,
    pub(crate) distance: f64,
}

/// Response from `POST /api/ask`. The server field is `memories`, not `sources`.
#[derive(Deserialize)]
pub(crate) struct AskResponse {
    pub(crate) answer: String,
    pub(crate) memories: Vec<AskSource>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub(crate) struct MemWalClient {
    pub(crate) api_base: String,
    pub(crate) private_key_hex: String,
    http: reqwest::Client,
}

impl MemWalClient {
    pub(crate) fn new(api_base: String, private_key_hex: String) -> Self {
        Self {
            api_base,
            private_key_hex,
            http: reqwest::Client::new(),
        }
    }

    /// Resolve server URL and private key from an optional caller override and
    /// environment variables, falling back to the production default.
    pub(crate) fn from_env(server_url_override: Option<String>) -> Self {
        let api_base = server_url_override
            .or_else(|| std::env::var(ENV_SERVER_URL).ok())
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
        let private_key_hex = std::env::var(ENV_PRIVATE_KEY).unwrap_or_default();
        Self::new(api_base, private_key_hex)
    }

    // -----------------------------------------------------------------------
    // Low-level request helpers
    // -----------------------------------------------------------------------

    async fn post<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<reqwest::Response, MemWalError> {
        let body_bytes = serde_json::to_vec(body).expect("serializable body");
        let headers = sign_request(&self.private_key_hex, "POST", path, &body_bytes)?;
        let url = format!("{}{path}", self.api_base);

        let resp = self
            .http
            .post(&url)
            .header("x-public-key", &headers.public_key)
            .header("x-signature", &headers.signature)
            .header("x-timestamp", &headers.timestamp)
            .header("content-type", "application/json")
            .body(body_bytes)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MemWalError::Server(format!("HTTP {status}: {body}")));
        }

        Ok(resp)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response, MemWalError> {
        let headers = sign_request(&self.private_key_hex, "GET", path, b"")?;
        let url = format!("{}{path}", self.api_base);

        let resp = self
            .http
            .get(&url)
            .header("x-public-key", &headers.public_key)
            .header("x-signature", &headers.signature)
            .header("x-timestamp", &headers.timestamp)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MemWalError::Server(format!("HTTP {status}: {body}")));
        }

        Ok(resp)
    }

    // -----------------------------------------------------------------------
    // Domain-level API calls
    // -----------------------------------------------------------------------

    /// `POST /api/remember` — enqueue a single memory.
    pub(crate) async fn remember(
        &self,
        text: &str,
        namespace: Option<&str>,
    ) -> Result<String, MemWalError> {
        #[derive(Serialize)]
        struct Req<'a> {
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
        }
        let resp: RememberResponse = self
            .post("/api/remember", &Req { text, namespace })
            .await?
            .json()
            .await?;
        Ok(resp.job_id)
    }

    /// `GET /api/remember/:job_id` — poll until the job reaches a terminal state.
    /// Returns the `blob_id` on success.
    ///
    /// Terminal statuses from the server: `"done"` (success) and `"failed"`.
    /// Intermediate statuses `"pending"`, `"running"`, `"uploaded"` are treated
    /// as still in progress.
    pub(crate) async fn poll_job(&self, job_id: &str) -> Result<String, MemWalError> {
        let path = format!("/api/remember/{job_id}");
        for _ in 0..MAX_POLLS {
            sleep(POLL_INTERVAL).await;
            let status: JobStatusResponse = self.get(&path).await?.json().await?;
            match status.status.as_str() {
                "done" => {
                    return Ok(status.blob_id.unwrap_or_default());
                }
                "failed" => return Err(MemWalError::JobFailed(job_id.to_string())),
                // "pending" | "running" | "uploaded" → still in progress
                _ => {}
            }
        }
        Err(MemWalError::Timeout(job_id.to_string()))
    }

    /// `POST /api/recall` — semantic search over stored memories.
    pub(crate) async fn recall(
        &self,
        query: &str,
        limit: Option<u32>,
        namespace: Option<&str>,
    ) -> Result<Vec<MemoryResult>, MemWalError> {
        #[derive(Serialize)]
        struct Req<'a> {
            query: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            limit: Option<u32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
        }
        let resp: RecallResponse = self
            .post(
                "/api/recall",
                &Req {
                    query,
                    limit,
                    namespace,
                },
            )
            .await?
            .json()
            .await?;
        Ok(resp.results)
    }

    /// `POST /api/ask` — memory-augmented Q&A.
    pub(crate) async fn ask(
        &self,
        question: &str,
        namespace: Option<&str>,
        limit: Option<u32>,
    ) -> Result<AskResponse, MemWalError> {
        #[derive(Serialize)]
        struct Req<'a> {
            question: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            limit: Option<u32>,
        }
        // The server field is `memories`, not `sources` — AskResponse reflects this.
        let resp: AskResponse = self
            .post(
                "/api/ask",
                &Req {
                    question,
                    namespace,
                    limit,
                },
            )
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    /// `POST /api/analyze` — extract facts from text and enqueue each as a memory.
    /// Returns the number of jobs submitted.
    pub(crate) async fn analyze(
        &self,
        text: &str,
        namespace: Option<&str>,
    ) -> Result<usize, MemWalError> {
        #[derive(Serialize)]
        struct Req<'a> {
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
        }
        let resp: AnalyzeResponse = self
            .post("/api/analyze", &Req { text, namespace })
            .await?
            .json()
            .await?;
        Ok(resp.job_ids.len())
    }

    /// `GET /health` — check whether the relayer is reachable and on the
    /// expected API version.
    ///
    /// The health endpoint is public (no auth required). When the response
    /// body contains a `"version"` field, it is compared against
    /// [`MEMWAL_API_VERSION`]. A mismatch means the deployed relayer has been
    /// upgraded to an incompatible version and the tools need updating.
    pub(crate) async fn health_check(&self) -> Result<(), MemWalError> {
        let url = format!("{}/health", self.api_base);
        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(MemWalError::Server(format!(
                "relayer health returned HTTP {}",
                resp.status().as_u16()
            )));
        }

        // Version check: if the relayer reports a version, it must match ours.
        if let Ok(body) = resp.json::<serde_json::Value>().await {
            if let Some(server_ver) = body.get("version").and_then(|v| v.as_str()) {
                if server_ver != MEMWAL_API_VERSION {
                    return Err(MemWalError::Server(format!(
                        "relayer version mismatch: tools expect {MEMWAL_API_VERSION}, \
                         server reports {server_ver} — update the tools or pin the relayer"
                    )));
                }
            }
        }

        Ok(())
    }

    /// Returns `Err` if the private key env var is missing or unparsable.
    pub(crate) fn validate_key(&self) -> Result<(), AuthError> {
        if self.private_key_hex.is_empty() {
            return Err(AuthError::MissingKey);
        }
        let raw = hex::decode(&self.private_key_hex)?;
        let _: [u8; 32] = raw
            .try_into()
            .map_err(|v: Vec<u8>| AuthError::InvalidKeyLength(v.len()))?;
        Ok(())
    }
}
