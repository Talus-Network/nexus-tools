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
    std::{sync::Once, time::Duration},
    tokio::time::sleep,
};

/// One-shot guard so the "missing delegate key" warning is emitted at most
/// once per process even though `from_env` runs once per registered tool.
static WARN_MISSING_KEY: Once = Once::new();

/// Load a `.env` file into the process environment if one is present.
///
/// Uses `dotenvy::dotenv()`, which searches starting at the process's current
/// working directory and walks up the parent chain until it finds a `.env`.
/// **Existing exports always win** — variables already set in the environment
/// are not overwritten. This matches the just `server-start` wrapper's
/// snapshot/restore behavior, so the two paths agree on precedence.
///
/// Failure modes:
/// - No `.env` anywhere on the walk → silent, normal in container deploys
///   where env vars come from the orchestrator.
/// - `.env` found but unreadable / malformed → one-line WARN to stderr; boot
///   continues so missing/invalid `MEMWAL_DELEGATE_PRIVATE_KEY` is still
///   surfaced by [`validate_credentials_at_startup`].
pub(crate) fn load_dotenv_if_present() {
    match dotenvy::dotenv() {
        Ok(path) => eprintln!("INFO: loaded env vars from {}", path.display()),
        Err(e) if e.not_found() => {}
        Err(e) => eprintln!("WARN: failed to load .env: {e}"),
    }
}

/// Classification of the delegate key value read from the environment.
///
/// "Valid" means: the hex decodes to exactly 32 bytes — the Ed25519 scalar
/// shape used by both MemWal's relayer auth and Sui's default account-key
/// flavour. Any 32 raw bytes form a valid Ed25519 secret (ed25519-dalek
/// SHA-512-hashes them to derive the scalar), so this is the strongest check
/// we can do client-side. The relayer additionally verifies that the
/// derived public key is registered on chain as a delegate for a MemWal
/// account — that authority check can only happen server-side.
#[derive(Debug, PartialEq, Eq)]
enum KeyValidation {
    /// Key is set and decodes to 32 bytes. Carries the original hex string.
    Ok(String),
    /// Env var is unset or empty. Boot continues; signed calls will fail.
    Missing,
    /// Env var is set but malformed. Carries an operator-facing reason.
    Invalid(String),
}

/// Pure classifier — no I/O, no exits, deterministic. Side effects live in
/// [`read_validated_private_key`].
fn classify_key(value: Option<&str>) -> KeyValidation {
    let raw = match value {
        Some(s) if !s.is_empty() => s,
        _ => return KeyValidation::Missing,
    };
    let bytes = match hex::decode(raw) {
        Ok(b) => b,
        Err(e) => {
            return KeyValidation::Invalid(format!(
                "is set but is not valid hex ({e}). Expected 64 hex chars \
                 encoding a 32-byte Ed25519 scalar."
            ));
        }
    };
    if bytes.len() != 32 {
        return KeyValidation::Invalid(format!(
            "decoded to {} bytes, expected 32 (Ed25519 scalar).",
            bytes.len()
        ));
    }
    KeyValidation::Ok(raw.to_string())
}

/// Run the delegate-key validation eagerly at process startup.
///
/// `bootstrap!` constructs `NexusTool` instances lazily on first request, so
/// without this hook the FATAL-on-malformed-key path would only fire when an
/// `/invoke` actually arrived — long after `server-start` reported "Ready".
/// Call this from `main` before handing control to the toolkit.
pub(crate) fn validate_credentials_at_startup() {
    let _ = read_validated_private_key();
}

/// Read and validate `MEMWAL_DELEGATE_PRIVATE_KEY` at startup.
///
/// - **Set and valid** → returns the hex string.
/// - **Unset / empty** → returns `None` after a one-shot stderr warning.
///   The binary continues so `/tools` listing and process liveness still
///   work; signed calls will fail with `MissingKey`.
/// - **Set but malformed** → writes a fatal error to stderr and aborts the
///   process. An explicitly-set-but-broken key is always a misconfiguration;
///   continuing would mask it behind health checks that look "fine" on the
///   listing endpoint.
fn read_validated_private_key() -> Option<String> {
    match classify_key(std::env::var(ENV_PRIVATE_KEY).ok().as_deref()) {
        KeyValidation::Ok(k) => Some(k),
        KeyValidation::Missing => {
            WARN_MISSING_KEY.call_once(|| {
                eprintln!(
                    "WARN: {ENV_PRIVATE_KEY} is not set — the memory-memwal \
                     tools booted, but every signed call and per-tool health \
                     check will fail with MissingKey until this env var is \
                     either exported in the process environment or placed in \
                     a `.env` file in or above the current working directory."
                );
            });
            None
        }
        KeyValidation::Invalid(reason) => {
            eprintln!("FATAL: {ENV_PRIVATE_KEY} {reason}");
            std::process::exit(1);
        }
    }
}

/// MemWal relayer version this crate was written against.
///
/// **Pinned reference:** tag [`@mysten-incubation/memwal@0.0.4`][tag] at
/// commit `0cd0862ade` on `https://github.com/MystenLabs/MemWal`. Every
/// wire-format invariant in this crate (canonical signed message, header
/// names, endpoint paths, request/response shapes) was derived from
/// `services/server/src/{auth,types,routes,rate_limit}.rs` at that tag.
///
/// The string itself is the `version` field of the relayer's
/// `services/server/Cargo.toml` at that tag (`0.1.0`). It is also what the
/// live relayer reports at `GET /health` (`{"status":"ok","version":"0.1.0"}`).
/// `health_check()` compares the two and fails fast on mismatch.
///
/// There is no published OpenAPI spec; the prose docs at
/// <https://docs.memwal.ai> describe the API at a high level only — the
/// source is the authoritative wire-format reference.
///
/// **Maintenance:** when the relayer publishes a new tag with a Cargo
/// package-version bump or any change to `auth.rs`/`types.rs`/`routes.rs`,
/// re-audit those files at the new tag and update this constant + the doc
/// comment together. Until then, this crate is intentionally pinned to a
/// known-good release rather than tracking `main`.
///
/// [tag]: https://github.com/MystenLabs/MemWal/tree/%40mysten-incubation%2Fmemwal%400.0.4/services/server
pub(crate) const MEMWAL_API_VERSION: &str = "0.1.0";

/// Public MemWal relayer pointing at Sui **mainnet**.
///
/// Verified via `GET /config` → `{"network": "mainnet", ...}`.
///
/// Reference constant — operators select this via the `MEMWAL_SERVER_URL`
/// env var or the `server_url` tool input rather than via a Rust code path.
#[allow(dead_code)]
pub(crate) const RELAYER_URL_MAINNET: &str = "https://relayer.memwal.ai";

/// Public MemWal relayer pointing at Sui **testnet**.
///
/// The hostname carries `staging` rather than `testnet` because Walrus
/// Foundation runs this instance as their pre-production environment, which
/// is the relayer wired to Sui testnet. Documented in
/// `docs/relayer/public-relayer.md` in the MystenLabs/MemWal repository.
pub(crate) const RELAYER_URL_TESTNET: &str = "https://relayer.staging.memwal.ai";

/// Default URL used when neither the tool input nor `MEMWAL_SERVER_URL` is
/// set. Points at testnet so the MemWal beta API can be exercised without
/// spending real SUI; mainnet deployments must opt in via env var or
/// per-call override.
pub(crate) const DEFAULT_SERVER_URL: &str = RELAYER_URL_TESTNET;

/// Env var for overriding the relayer URL at the operator level.
pub(crate) const ENV_SERVER_URL: &str = "MEMWAL_SERVER_URL";

/// Env var carrying the hex-encoded Ed25519 delegate private key.
pub(crate) const ENV_PRIVATE_KEY: &str = "MEMWAL_DELEGATE_PRIVATE_KEY";

/// Env var carrying the MemWal account object ID (the on-chain Sui Move object
/// that owns the delegate keys). When set, it is sent as the `x-account-id`
/// header AND embedded in the signed canonical message — matching the JS
/// SDK 1:1. When unset, the relayer falls back to an on-chain registry scan
/// keyed by public key, which is slower and more fragile.
pub(crate) const ENV_ACCOUNT_ID: &str = "MEMWAL_ACCOUNT_ID";

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

/// Response from `POST /api/forget` — count of memories removed.
#[derive(Deserialize)]
pub(crate) struct ForgetResponse {
    pub(crate) deleted: u64,
    /// Server echoes the namespace it interpreted (`"default"` when absent
    /// from the request). Currently informational only.
    #[allow(dead_code)]
    pub(crate) namespace: String,
}

/// Response from `POST /api/stats` — per-namespace usage summary.
#[derive(Deserialize)]
pub(crate) struct StatsResponse {
    pub(crate) memory_count: i64,
    pub(crate) storage_bytes: i64,
    pub(crate) namespace: String,
}

/// Response from `POST /api/remember/bulk` — 202 Accepted with one job_id per item.
#[derive(Deserialize)]
pub(crate) struct RememberBulkResponse {
    pub(crate) job_ids: Vec<String>,
}

/// One result entry from `POST /api/remember/bulk/status`.
#[derive(Deserialize, Clone)]
pub(crate) struct BulkStatusItem {
    pub(crate) job_id: String,
    /// "pending" | "running" | "uploaded" | "done" | "failed"
    pub(crate) status: String,
    pub(crate) blob_id: Option<String>,
    /// Populated when `status == "failed"`. Exposed so partial-success
    /// callers (via [`MemWalClient::poll_bulk_status_once`]) can inspect
    /// the reason — `poll_bulk_jobs` itself only reports the failed job_id.
    #[allow(dead_code)]
    pub(crate) error: Option<String>,
}

/// Response from `POST /api/remember/bulk/status` — one entry per requested job_id.
#[derive(Deserialize)]
pub(crate) struct BulkStatusResponse {
    pub(crate) results: Vec<BulkStatusItem>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub(crate) struct MemWalClient {
    pub(crate) api_base: String,
    pub(crate) private_key_hex: String,
    /// MemWal account object ID. Empty string when not configured — the
    /// relayer then resolves the account from the public key via on-chain
    /// scan. Signed into the canonical message; sent as `x-account-id`
    /// header only when non-empty (mirrors the JS SDK).
    pub(crate) account_id: String,
    http: reqwest::Client,
}

impl MemWalClient {
    pub(crate) fn new(api_base: String, private_key_hex: String, account_id: String) -> Self {
        Self {
            api_base,
            private_key_hex,
            account_id,
            http: reqwest::Client::new(),
        }
    }

    /// Resolve server URL, private key, and account ID from an optional
    /// caller override and environment variables, falling back to the
    /// production default URL and an empty account id.
    ///
    /// Delegates key handling to [`read_validated_private_key`]: a missing
    /// key produces a one-shot warning and the binary keeps booting; a key
    /// that is set but malformed terminates the process.
    pub(crate) fn from_env(server_url_override: Option<String>) -> Self {
        let api_base = server_url_override
            .or_else(|| std::env::var(ENV_SERVER_URL).ok())
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
        let private_key_hex = read_validated_private_key().unwrap_or_default();
        let account_id = std::env::var(ENV_ACCOUNT_ID).unwrap_or_default();
        Self::new(api_base, private_key_hex, account_id)
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
        let headers = sign_request(
            &self.private_key_hex,
            "POST",
            path,
            &body_bytes,
            &self.account_id,
        )?;
        let url = format!("{}{path}", self.api_base);

        let mut req = self
            .http
            .post(&url)
            .header("x-public-key", &headers.public_key)
            .header("x-signature", &headers.signature)
            .header("x-timestamp", &headers.timestamp)
            .header("x-nonce", &headers.nonce)
            .header("content-type", "application/json");
        if !self.account_id.is_empty() {
            req = req.header("x-account-id", &self.account_id);
        }
        let resp = req.body(body_bytes).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MemWalError::Server(format!("HTTP {status}: {body}")));
        }

        Ok(resp)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response, MemWalError> {
        let headers = sign_request(
            &self.private_key_hex,
            "GET",
            path,
            b"",
            &self.account_id,
        )?;
        let url = format!("{}{path}", self.api_base);

        let mut req = self
            .http
            .get(&url)
            .header("x-public-key", &headers.public_key)
            .header("x-signature", &headers.signature)
            .header("x-timestamp", &headers.timestamp)
            .header("x-nonce", &headers.nonce);
        if !self.account_id.is_empty() {
            req = req.header("x-account-id", &self.account_id);
        }
        let resp = req.send().await?;

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

    /// `POST /api/forget` — delete every memory in the given namespace.
    /// Owner-scoped: only memories whose owner matches the authenticated
    /// account can be deleted, regardless of the namespace string sent.
    pub(crate) async fn forget(&self, namespace: Option<&str>) -> Result<u64, MemWalError> {
        #[derive(Serialize)]
        struct Req<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
        }
        let resp: ForgetResponse = self
            .post("/api/forget", &Req { namespace })
            .await?
            .json()
            .await?;
        Ok(resp.deleted)
    }

    /// `POST /api/stats` — memory count + stored byte total for a namespace.
    pub(crate) async fn stats(&self, namespace: Option<&str>) -> Result<StatsResponse, MemWalError> {
        #[derive(Serialize)]
        struct Req<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
        }
        self.post("/api/stats", &Req { namespace })
            .await?
            .json()
            .await
            .map_err(MemWalError::from)
    }

    /// `POST /api/remember/bulk` — submit up to MAX_BULK_ITEMS texts in a single
    /// 202-Accepted call. Returns one job_id per item, in the order submitted.
    /// Each job still needs polling — use [`poll_bulk_jobs`] for the batched
    /// status endpoint instead of N separate [`poll_job`] calls.
    pub(crate) async fn remember_bulk(
        &self,
        items: &[(&str, Option<&str>)],
    ) -> Result<Vec<String>, MemWalError> {
        #[derive(Serialize)]
        struct Item<'a> {
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
        }
        #[derive(Serialize)]
        struct Req<'a> {
            items: Vec<Item<'a>>,
        }
        let req = Req {
            items: items
                .iter()
                .map(|(text, ns)| Item { text, namespace: *ns })
                .collect(),
        };
        let resp: RememberBulkResponse =
            self.post("/api/remember/bulk", &req).await?.json().await?;
        Ok(resp.job_ids)
    }

    /// `POST /api/remember/bulk/status` — poll every job once. Iterates the
    /// batched endpoint until all jobs reach a terminal state (`done` or
    /// `failed`) or the global timeout fires.
    ///
    /// Returns blob_ids in the same order as `job_ids` on success. If any
    /// individual job ends `failed`, returns an `Err(JobFailed(job_id))`
    /// identifying the first failure — callers that want partial-success
    /// semantics should call [`MemWalClient::poll_bulk_status_once`] instead.
    pub(crate) async fn poll_bulk_jobs(
        &self,
        job_ids: &[String],
    ) -> Result<Vec<String>, MemWalError> {
        if job_ids.is_empty() {
            return Ok(Vec::new());
        }
        for _ in 0..MAX_POLLS {
            sleep(POLL_INTERVAL).await;
            let statuses = self.poll_bulk_status_once(job_ids).await?;
            // Index-correlate input order; the relayer returns results in
            // input order, but we still verify by job_id to be defensive.
            let mut all_done = true;
            for want_id in job_ids {
                let entry = statuses
                    .iter()
                    .find(|s| s.job_id == *want_id)
                    .ok_or_else(|| {
                        MemWalError::Server(format!("bulk status response missing job {want_id}"))
                    })?;
                match entry.status.as_str() {
                    "done" => {}
                    "failed" => return Err(MemWalError::JobFailed(want_id.clone())),
                    // pending | running | uploaded
                    _ => {
                        all_done = false;
                        break;
                    }
                }
            }
            if all_done {
                return job_ids
                    .iter()
                    .map(|want_id| {
                        statuses
                            .iter()
                            .find(|s| s.job_id == *want_id)
                            .and_then(|s| s.blob_id.clone())
                            .ok_or_else(|| {
                                MemWalError::Server(format!(
                                    "job {want_id} reached done but blob_id missing"
                                ))
                            })
                    })
                    .collect();
            }
        }
        Err(MemWalError::Timeout(format!(
            "{} bulk jobs did not finish in time",
            job_ids.len()
        )))
    }

    /// Single shot of `POST /api/remember/bulk/status`. Returns whatever the
    /// server says without polling. Exposed for callers that want to inspect
    /// per-job state directly (e.g. tolerate partial failures).
    pub(crate) async fn poll_bulk_status_once(
        &self,
        job_ids: &[String],
    ) -> Result<Vec<BulkStatusItem>, MemWalError> {
        #[derive(Serialize)]
        struct Req<'a> {
            job_ids: &'a [String],
        }
        let resp: BulkStatusResponse = self
            .post("/api/remember/bulk/status", &Req { job_ids })
            .await?
            .json()
            .await?;
        Ok(resp.results)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `classify_key(None)` reports `Missing`.
    /// Failure mode caught: an unset env var is silently treated as a valid
    /// empty key, masking the misconfiguration behind apparent boot success.
    #[test]
    fn classify_key_missing_when_unset() {
        assert_eq!(classify_key(None), KeyValidation::Missing);
    }

    /// `classify_key(Some(""))` reports `Missing`, matching shells that export
    /// the variable with an empty value.
    /// Failure mode caught: an empty-string env var is treated as a valid key
    /// and produces empty hex headers on every signed request.
    #[test]
    fn classify_key_missing_when_empty() {
        assert_eq!(classify_key(Some("")), KeyValidation::Missing);
    }

    /// A canonical 32-byte hex key is classified as `Ok` and the original
    /// hex string is preserved verbatim (the signer uses the hex form, not
    /// re-encoded bytes).
    /// Failure mode caught: a valid key is mis-classified as Invalid, which
    /// would abort the binary on the happy path.
    #[test]
    fn classify_key_ok_on_valid_32_byte_hex() {
        let key = hex::encode([0x42u8; 32]);
        match classify_key(Some(&key)) {
            KeyValidation::Ok(returned) => assert_eq!(returned, key),
            other => panic!("expected Ok(\"{key}\"), got {other:?}"),
        }
    }

    /// Non-hex input is classified as `Invalid` with a reason mentioning hex.
    /// Failure mode caught: malformed input passes validation, exits would be
    /// missing, and the binary boots with garbage credentials that produce
    /// confusing relayer errors at request time.
    #[test]
    fn classify_key_invalid_on_non_hex() {
        match classify_key(Some("not-hex!!")) {
            KeyValidation::Invalid(reason) => assert!(
                reason.contains("hex"),
                "reason must mention hex format; got: {reason}"
            ),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// Hex that decodes to fewer than 32 bytes is classified as `Invalid`
    /// with a byte-count reason.
    /// Failure mode caught: a truncated key is accepted, leading to silent
    /// signature verification failures the operator cannot diagnose.
    #[test]
    fn classify_key_invalid_on_too_few_bytes() {
        let short = hex::encode([0u8; 16]); // 16 bytes
        match classify_key(Some(&short)) {
            KeyValidation::Invalid(reason) => {
                assert!(
                    reason.contains("16") && reason.contains("32"),
                    "reason must mention actual (16) and expected (32) byte counts; got: {reason}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// Hex that decodes to more than 32 bytes is also classified as `Invalid`.
    /// Failure mode caught: an over-long key (e.g. a Sui-formatted key
    /// pasted with extra flag bytes) is silently truncated or accepted.
    #[test]
    fn classify_key_invalid_on_too_many_bytes() {
        let long = hex::encode([0u8; 64]); // 64 bytes
        match classify_key(Some(&long)) {
            KeyValidation::Invalid(reason) => {
                assert!(
                    reason.contains("64") && reason.contains("32"),
                    "reason must mention actual (64) and expected (32) byte counts; got: {reason}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// All-`0xff` 32-byte input is `Ok` — Ed25519 has no scalar-range
    /// rejection in dalek's `from_bytes`, which SHA-512-derives the scalar.
    /// Failure mode caught: an over-strict client-side filter rejects a key
    /// the relayer would actually accept, blocking legitimate deployments.
    #[test]
    fn classify_key_ok_on_all_ones_32_bytes() {
        let key = hex::encode([0xffu8; 32]);
        assert_eq!(classify_key(Some(&key)), KeyValidation::Ok(key));
    }
}
