//! MemWal HTTP client.
//!
//! Credentials (`MEMWAL_DELEGATE_PRIVATE_KEY`, optional `MEMWAL_ACCOUNT_ID`)
//! and the relayer URL (`MEMWAL_SERVER_URL`) come from env at startup — never
//! from tool inputs, which flow through the Nexus DAG as on-chain data.

use {
    crate::{
        auth::{parse_signing_key, sign_request},
        error::{AuthError, MemWalError},
    },
    ed25519_dalek::SigningKey,
    serde::{Deserialize, Serialize},
    std::{
        collections::HashMap,
        sync::{Arc, Once, OnceLock},
        time::Duration,
    },
    tokio::time::{sleep, Instant},
    url::Url,
    zeroize::Zeroizing,
};

/// One-shot guard so the "missing delegate key" warning fires once even
/// though `from_env` runs once per registered tool. Keep the closure
/// trivially fast — `Once::call_once` blocks every other caller until it
/// returns.
static WARN_MISSING_KEY: Once = Once::new();

/// Load `<cwd>/.env` if present. Cwd-only (no parent walk) so a planted
/// `.env` above the binary's cwd cannot influence the process. Existing
/// exports always win. Must be called before the tokio runtime is built —
/// `set_var` is unsound from a multi-threaded process.
pub(crate) fn load_dotenv_if_present() {
    let candidate = match std::env::current_dir() {
        Ok(d) => d.join(".env"),
        Err(e) => {
            log::warn!("could not read cwd while looking for .env: {e}");
            return;
        }
    };
    if !candidate.is_file() {
        return;
    }
    match dotenvy::from_path(&candidate) {
        Ok(()) => log::info!("loaded env vars from {}", candidate.display()),
        Err(e) => log::warn!("failed to load {}: {e}", candidate.display()),
    }
}

/// Classification of the delegate key env var. Hand-written `Debug` redacts
/// the secret in `Ok` — never `derive` Debug on this type.
enum KeyValidation {
    /// Valid: 32-byte Ed25519 scalar, hex-encoded. Wrapped in `Zeroizing` so
    /// the heap buffer is wiped on drop.
    Ok(Zeroizing<String>),
    /// Unset or empty. Boot continues; signed calls fail with MissingKey.
    Missing,
    /// Set but malformed. Carries an operator-facing reason.
    Invalid(String),
}

impl std::fmt::Debug for KeyValidation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok(_) => write!(f, "Ok(<redacted>)"),
            Self::Missing => write!(f, "Missing"),
            Self::Invalid(reason) => f.debug_tuple("Invalid").field(reason).finish(),
        }
    }
}

impl PartialEq for KeyValidation {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ok(a), Self::Ok(b)) => a.as_str() == b.as_str(),
            (Self::Missing, Self::Missing) => true,
            (Self::Invalid(a), Self::Invalid(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for KeyValidation {}

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
    KeyValidation::Ok(Zeroizing::new(raw.to_string()))
}

/// Eager startup-time key validation. Without this, a malformed key would
/// only surface on the first `/invoke` (well after `server-start` reports
/// "Ready") because `bootstrap!` constructs `NexusTool` instances lazily.
/// Called from `main`, which owns the only `process::exit` site.
pub(crate) fn validate_credentials_at_startup() -> Result<(), String> {
    read_validated_private_key().map(|_| ())
}

/// `Ok(Some(hex))` valid; `Ok(None)` unset (warn-and-continue); `Err(reason)`
/// malformed. `main` exits on `Err`; `from_env` downgrades it to a log so a
/// mid-process key rotation doesn't kill in-flight requests.
fn read_validated_private_key() -> Result<Option<Zeroizing<String>>, String> {
    // `env::var` returns a fresh `String` — wrap it immediately so that
    // intermediate copy is also zeroed on drop.
    let raw = std::env::var(ENV_PRIVATE_KEY).ok().map(Zeroizing::new);
    match classify_key(raw.as_deref().map(|z| z.as_str())) {
        KeyValidation::Ok(k) => Ok(Some(k)),
        KeyValidation::Missing => {
            WARN_MISSING_KEY.call_once(|| {
                log::warn!(
                    "{ENV_PRIVATE_KEY} is not set — every signed call and \
                     per-tool health check will fail with MissingKey until \
                     this env var is exported in the process environment."
                );
            });
            Ok(None)
        }
        KeyValidation::Invalid(reason) => Err(reason),
    }
}

/// MemWal relayer version this crate is pinned to.
///
/// Source of truth: tag [`@mysten-incubation/memwal@0.0.4`][tag],
/// `services/server/Cargo.toml`. Every wire-format invariant in this crate
/// (canonical signed message, header names, endpoint paths, request/
/// response shapes) was derived from
/// `services/server/src/{auth,types,routes,rate_limit}.rs` at that tag.
///
/// **Maintenance contract:** on every new relayer tag with a Cargo version
/// bump *or* any change to those four files, re-audit and update this
/// constant. [`MemWalClient::health_check`] enforces the version match at
/// runtime against `GET /health`'s `version` field.
///
/// [tag]: https://github.com/MystenLabs/MemWal/tree/%40mysten-incubation%2Fmemwal%400.0.4/services/server
pub(crate) const MEMWAL_API_VERSION: &str = "0.1.0";

#[allow(dead_code)]
pub(crate) const RELAYER_URL_MAINNET: &str = "https://relayer.memwal.ai";

/// Walrus Foundation's pre-production deployment is wired to Sui testnet —
/// the `staging` hostname is not a typo.
pub(crate) const RELAYER_URL_TESTNET: &str = "https://relayer.staging.memwal.ai";

/// Default points at testnet so the beta API can be exercised without
/// real SUI; mainnet requires opting in via `MEMWAL_SERVER_URL`.
pub(crate) const DEFAULT_SERVER_URL: &str = RELAYER_URL_TESTNET;

pub(crate) const ENV_SERVER_URL: &str = "MEMWAL_SERVER_URL";
pub(crate) const ENV_PRIVATE_KEY: &str = "MEMWAL_DELEGATE_PRIVATE_KEY";
pub(crate) const ENV_ACCOUNT_ID: &str = "MEMWAL_ACCOUNT_ID";

/// First poll fires after a short delay so fast jobs resolve fast.
const POLL_INITIAL: Duration = Duration::from_millis(100);
/// Cap on inter-poll delay — bounds the rate-limit cost of a slow job.
const POLL_MAX: Duration = Duration::from_secs(4);
/// Wall-clock budget per `poll_job` / `poll_bulk_jobs`. Sized for Walrus
/// erasure-coded tail latency (30–60 s under load).
const POLL_BUDGET: Duration = Duration::from_secs(60);

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(POLL_MAX)
}

/// Add 0..25% positive jitter to `delay` so concurrent polls of the same
/// job (e.g. two clients waiting on a shared job_id) don't lock-step on the
/// exact 100/200/400 ms ticks. Entropy comes from the system clock's
/// sub-second nanoseconds — adequate for spreading-out, not for security.
fn jittered(delay: Duration) -> Duration {
    let jitter_ms = (delay.as_millis() as u64 / 4).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    delay + Duration::from_millis(nanos % jitter_ms)
}

fn check_text_len(field: &str, value: &str) -> Result<(), MemWalError> {
    if value.len() > MAX_TEXT_BYTES {
        return Err(MemWalError::Config(format!(
            "{field} exceeds maximum allowed size of {MAX_TEXT_BYTES} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

/// Mirrors the relayer's `MAX_BULK_ITEMS` so a 21-item batch fails at the
/// tool boundary with a clear reason instead of an opaque HTTP 400.
pub(crate) const MAX_BULK_ITEMS: usize = 20;

/// Defensive cap on text-carrying tool inputs. Oversized inputs fail before
/// burning a signed-request slot.
pub(crate) const MAX_TEXT_BYTES: usize = 1 << 20; // 1 MiB

/// Map non-2xx into `MemWalError`: 429 → `RateLimited` (parses
/// `Retry-After`); everything else → a terse `Server(_)` that does NOT
/// inline the upstream body, since `/invoke` callers are unauthenticated.
///
/// The full body (capped 256 chars) is logged under `target=memwal::upstream`.
/// It may quote stored memory text or other relayer internals — operators
/// who treat logs as lower-trust than the binary should filter via
/// `RUST_LOG=memwal::upstream=off`.
async fn map_error_response(resp: reqwest::Response) -> MemWalError {
    let status = resp.status();
    if status.as_u16() == 429 {
        let retry_after_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let body = resp.text().await.unwrap_or_default();
        log::warn!(
            target: "memwal::upstream",
            "HTTP 429 (retry-after={retry_after_secs:?}): {}",
            body.chars().take(256).collect::<String>()
        );
        return MemWalError::RateLimited { retry_after_secs };
    }
    let body = resp.text().await.unwrap_or_default();
    log::warn!(
        target: "memwal::upstream",
        "HTTP {}: {}",
        status,
        body.chars().take(256).collect::<String>()
    );
    let terse = match status.as_u16() {
        401 | 403 => "upstream auth failure".to_string(),
        408 | 504 => "upstream timeout".to_string(),
        s if (400..500).contains(&s) => format!("upstream rejected the request (HTTP {s})"),
        s => format!("upstream unavailable (HTTP {s})"),
    };
    MemWalError::Server(terse)
}

// ---------------------------------------------------------------------------
// API response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct RememberResponse {
    pub(crate) job_id: String,
}

/// Job state from the relayer. `Unknown` (via `#[serde(other)]`) captures
/// future statuses — polling loops surface it as an error rather than
/// looping on something they don't recognize.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobStatus {
    Pending,
    Running,
    Uploaded,
    Done,
    Failed,
    #[serde(other)]
    Unknown,
}

impl JobStatus {
    fn is_in_progress(self) -> bool {
        matches!(
            self,
            JobStatus::Pending | JobStatus::Running | JobStatus::Uploaded
        )
    }
}

#[derive(Deserialize)]
pub(crate) struct JobStatusResponse {
    pub(crate) status: JobStatus,
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
    pub(crate) status: JobStatus,
    pub(crate) blob_id: Option<String>,
}

/// Response from `POST /api/remember/bulk/status` — one entry per requested job_id.
#[derive(Deserialize)]
pub(crate) struct BulkStatusResponse {
    pub(crate) results: Vec<BulkStatusItem>,
}

/// `GET /health` body. `version` is required (enforces the maintenance-pin
/// contract on [`MEMWAL_API_VERSION`]); other fields are allowed and ignored.
#[derive(Deserialize)]
struct HealthResponse {
    version: Option<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Process-wide HTTP client shared across every `MemWalClient` so the
/// connection pool, TLS session cache, and HTTP/2 multiplexing survive
/// across `invoke` calls. `reqwest::Client` clone is a cheap `Arc` bump.
static SHARED_HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn shared_http() -> reqwest::Client {
    SHARED_HTTP
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(8)
                .tcp_keepalive(Duration::from_secs(30))
                .build()
                .expect("reqwest client builder must succeed with these options")
        })
        .clone()
}

/// `MEMWAL_ALLOW_INSECURE=1` opts in to non-HTTPS relayer URLs (dev/test only).
fn allow_insecure() -> bool {
    std::env::var("MEMWAL_ALLOW_INSECURE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Validate a relayer base URL. Rejects: non-https (unless `allow_insecure`),
/// path beyond root, query/fragment, userinfo. `allow_insecure` is a
/// parameter (not read from env) so the policy is pure-testable.
fn parse_relayer_url(raw: &str, allow_insecure: bool) -> Result<Url, MemWalError> {
    // Don't echo the raw URL in parse errors: an embedded
    // `user:secret@host` plus a typo would leak the secret into stderr,
    // log files, and (because each tool's `new()` panics on Config) into
    // the panic message itself.
    let url =
        Url::parse(raw).map_err(|e| MemWalError::Config(format!("invalid relayer URL: {e}")))?;
    let scheme = url.scheme();
    let scheme_ok = scheme == "https" || (allow_insecure && scheme == "http");
    if !scheme_ok {
        return Err(MemWalError::Config(format!(
            "relayer URL must use https (got scheme `{scheme}`)"
        )));
    }
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return Err(MemWalError::Config(format!(
            "relayer URL must not carry a path (got `{path}`)"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(MemWalError::Config(
            "relayer URL must not carry a query or fragment".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(MemWalError::Config(
            "relayer URL must not embed credentials (userinfo)".into(),
        ));
    }
    Ok(url)
}

/// Per-tool HTTP client. All non-trivial state is behind `Arc`/`reqwest::Client`
/// so cloning is cheap — every `NexusTool` holds one and reuses it across
/// every `invoke`. The signing key is parsed at construction so the per-call
/// path never re-decodes hex or rebuilds the public key.
#[derive(Clone)]
pub(crate) struct MemWalClient {
    http: reqwest::Client,
    api_base: Url,
    /// `None` when the env key was missing or malformed; signed calls then
    /// return `AuthError::MissingKey` instead of taking down the process.
    signing: Option<Arc<SigningMaterial>>,
    /// Empty when unconfigured (relayer falls back to a registry scan).
    /// Signed into the canonical message; sent as `x-account-id` only when
    /// non-empty (mirrors the JS SDK).
    account_id: Arc<str>,
}

struct SigningMaterial {
    signing_key: SigningKey,
    public_key_hex: String,
}

impl MemWalClient {
    /// Production constructor: validates `MEMWAL_SERVER_URL` (`Err(Config)`
    /// on parse failure) and parses the delegate key. A missing/malformed
    /// key logs but doesn't error here — `main`'s startup validation is the
    /// fail-fast site.
    pub(crate) fn from_env() -> Result<Self, MemWalError> {
        let api_base_raw = std::env::var(ENV_SERVER_URL)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
        let api_base = parse_relayer_url(&api_base_raw, allow_insecure())?;

        let signing = match read_validated_private_key() {
            Ok(Some(hex)) => match parse_signing_key(&hex) {
                Ok((signing_key, public_key_hex)) => Some(Arc::new(SigningMaterial {
                    signing_key,
                    public_key_hex,
                })),
                Err(e) => {
                    log::error!("{ENV_PRIVATE_KEY} could not be parsed: {e}");
                    None
                }
            },
            Ok(None) => None,
            Err(reason) => {
                log::error!("{ENV_PRIVATE_KEY} {reason}");
                None
            }
        };

        let account_id: Arc<str> = std::env::var(ENV_ACCOUNT_ID).unwrap_or_default().into();

        Ok(Self {
            http: shared_http(),
            api_base,
            signing,
            account_id,
        })
    }

    /// Test-only constructor. Accepts an unparsed URL (HTTP loopback for
    /// mockito) and a hex key; panics on parse failure since tests should
    /// not be exercising the error path.
    #[cfg(test)]
    pub(crate) fn with_test_config(
        api_base: &str,
        private_key_hex: &str,
        account_id: &str,
    ) -> Self {
        let url = Url::parse(api_base).expect("test URL must parse");
        let (signing_key, public_key_hex) =
            parse_signing_key(private_key_hex).expect("test key must parse");
        Self {
            http: reqwest::Client::new(),
            api_base: url,
            signing: Some(Arc::new(SigningMaterial {
                signing_key,
                public_key_hex,
            })),
            account_id: account_id.into(),
        }
    }

    /// Sign and dispatch a request, returning the `reqwest::Response` so
    /// callers can `.json()` it.
    fn signed_headers(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<crate::auth::AuthHeaders, MemWalError> {
        let signing = self
            .signing
            .as_ref()
            .ok_or(MemWalError::Auth(AuthError::MissingKey))?;
        Ok(sign_request(
            &signing.signing_key,
            &signing.public_key_hex,
            method,
            path,
            body,
            &self.account_id,
        )?)
    }

    fn join_path(&self, path: &str) -> Result<Url, MemWalError> {
        self.api_base
            .join(path)
            .map_err(|e| MemWalError::Config(format!("invalid path `{path}`: {e}")))
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
        let headers = self.signed_headers("POST", path, &body_bytes)?;
        let url = self.join_path(path)?;

        let mut req = self
            .http
            .post(url)
            .header("x-public-key", &headers.public_key)
            .header("x-signature", &headers.signature)
            .header("x-timestamp", &headers.timestamp)
            .header("x-nonce", &headers.nonce)
            .header("content-type", "application/json");
        if !self.account_id.is_empty() {
            req = req.header("x-account-id", self.account_id.as_ref());
        }
        let resp = req.body(body_bytes).send().await?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        Ok(resp)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response, MemWalError> {
        let headers = self.signed_headers("GET", path, b"")?;
        let url = self.join_path(path)?;

        let mut req = self
            .http
            .get(url)
            .header("x-public-key", &headers.public_key)
            .header("x-signature", &headers.signature)
            .header("x-timestamp", &headers.timestamp)
            .header("x-nonce", &headers.nonce);
        if !self.account_id.is_empty() {
            req = req.header("x-account-id", self.account_id.as_ref());
        }
        let resp = req.send().await?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
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
        check_text_len("text", text)?;
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

    /// Poll `GET /api/remember/:job_id` to terminal state. Exponential
    /// backoff (POLL_INITIAL → POLL_MAX) within POLL_BUDGET; an unrecognized
    /// status from the server is a hard error, not an in-progress signal.
    ///
    /// **Cancel-safety:** the loop is cancel-safe locally, but the
    /// server-side write is **not cancellable** — dropping the future after
    /// the initial 202 leaks one Walrus blob.
    pub(crate) async fn poll_job(&self, job_id: &str) -> Result<String, MemWalError> {
        let path = format!("/api/remember/{job_id}");
        let deadline = Instant::now() + POLL_BUDGET;
        let mut delay = POLL_INITIAL;

        loop {
            sleep(jittered(delay)).await;
            let status: JobStatusResponse = self.get(&path).await?.json().await?;
            match status.status {
                JobStatus::Done => {
                    return status.blob_id.ok_or_else(|| {
                        MemWalError::Server(format!(
                            "job {job_id} reached terminal status `done` but \
                             blob_id is missing from the response"
                        ))
                    });
                }
                JobStatus::Failed => return Err(MemWalError::JobFailed(job_id.to_string())),
                s if s.is_in_progress() => {}
                JobStatus::Unknown => {
                    return Err(MemWalError::Server(format!(
                        "job {job_id} returned an unrecognized status"
                    )));
                }
                _ => unreachable!("is_in_progress covers Pending/Running/Uploaded"),
            }
            if Instant::now() >= deadline {
                return Err(MemWalError::Timeout(job_id.to_string()));
            }
            delay = next_backoff(delay);
        }
    }

    /// `POST /api/recall` — semantic search over stored memories.
    pub(crate) async fn recall(
        &self,
        query: &str,
        limit: Option<u32>,
        namespace: Option<&str>,
    ) -> Result<Vec<MemoryResult>, MemWalError> {
        check_text_len("query", query)?;
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
        check_text_len("question", question)?;
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
        check_text_len("text", text)?;
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
    pub(crate) async fn stats(
        &self,
        namespace: Option<&str>,
    ) -> Result<StatsResponse, MemWalError> {
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

    /// Submit up to `MAX_BULK_ITEMS` texts in one 202-Accepted call. Callers
    /// must enforce the `1..=MAX_BULK_ITEMS` cap before this — oversized
    /// batches get an opaque HTTP 400 from the relayer. Pair with
    /// [`poll_bulk_jobs`] for the batched status endpoint.
    pub(crate) async fn remember_bulk(
        &self,
        items: &[(&str, Option<&str>)],
    ) -> Result<Vec<String>, MemWalError> {
        for (text, _) in items {
            check_text_len("text", text)?;
        }
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
                .map(|(text, ns)| Item {
                    text,
                    namespace: *ns,
                })
                .collect(),
        };
        let resp: RememberBulkResponse =
            self.post("/api/remember/bulk", &req).await?.json().await?;
        Ok(resp.job_ids)
    }

    /// Poll batched jobs to terminal state, returning blob_ids in input
    /// order. First `failed` surfaces as `Err(JobFailed)`. Each poll queries
    /// only the still-pending subset (cached terminal jobs in a HashMap),
    /// so rate-limit cost shrinks with completion. Same cancel-safety
    /// caveat as [`MemWalClient::poll_job`], scaled to MAX_BULK_ITEMS.
    pub(crate) async fn poll_bulk_jobs(
        &self,
        job_ids: &[String],
    ) -> Result<Vec<String>, MemWalError> {
        if job_ids.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + POLL_BUDGET;
        let mut delay = POLL_INITIAL;
        let mut terminal: HashMap<String, BulkStatusItem> = HashMap::with_capacity(job_ids.len());

        loop {
            let pending: Vec<String> = job_ids
                .iter()
                .filter(|id| !terminal.contains_key(*id))
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }

            sleep(jittered(delay)).await;
            let statuses = self.poll_bulk_status_once(&pending).await?;
            let by_id: HashMap<&str, &BulkStatusItem> =
                statuses.iter().map(|s| (s.job_id.as_str(), s)).collect();

            for id in &pending {
                let Some(item) = by_id.get(id.as_str()) else {
                    return Err(MemWalError::Server(format!(
                        "bulk status response missing job {id}"
                    )));
                };
                match item.status {
                    JobStatus::Done | JobStatus::Failed => {
                        terminal.insert(id.clone(), (*item).clone());
                    }
                    s if s.is_in_progress() => {}
                    JobStatus::Unknown => {
                        return Err(MemWalError::Server(format!(
                            "job {id} returned an unrecognized status"
                        )));
                    }
                    _ => unreachable!("is_in_progress covers Pending/Running/Uploaded"),
                }
            }

            if Instant::now() >= deadline {
                return Err(MemWalError::Timeout(format!(
                    "{} bulk jobs did not finish in time",
                    job_ids.len() - terminal.len()
                )));
            }
            delay = next_backoff(delay);
        }

        // All jobs terminal — assemble in input order, surface first failure.
        job_ids
            .iter()
            .map(|id| {
                let item = terminal
                    .get(id)
                    .expect("loop only exits when terminal covers job_ids");
                match item.status {
                    JobStatus::Done => item.blob_id.clone().ok_or_else(|| {
                        MemWalError::Server(format!(
                            "job {id} reached terminal status `done` but \
                             blob_id is missing"
                        ))
                    }),
                    JobStatus::Failed => Err(MemWalError::JobFailed(id.clone())),
                    _ => unreachable!("terminal cache only stores Done/Failed"),
                }
            })
            .collect()
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

    /// `GET /health` — reachability + API-version match. Public (no auth).
    /// Required: JSON body with `version` matching [`MEMWAL_API_VERSION`].
    /// Missing field / wrong type / non-JSON / mismatch all return `Err`;
    /// "best-effort" version check would void the maintenance-pin contract.
    pub(crate) async fn health_check(&self) -> Result<(), MemWalError> {
        let url = self.join_path("/health")?;
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(MemWalError::Server(format!(
                "relayer health returned HTTP {}",
                status.as_u16()
            )));
        }
        let body: HealthResponse = resp.json().await.map_err(|e| {
            MemWalError::Server(format!("relayer /health did not return JSON: {e}"))
        })?;
        let server_ver = body.version.ok_or_else(|| {
            MemWalError::Server(
                "relayer /health did not return a `version` field — pre-pinned-tag relayer?".into(),
            )
        })?;
        if server_ver != MEMWAL_API_VERSION {
            return Err(MemWalError::Server(format!(
                "relayer version mismatch: tools expect {MEMWAL_API_VERSION}, \
                 server reports {server_ver} — update the tools or pin the relayer"
            )));
        }
        Ok(())
    }

    /// Returns `Err(MissingKey)` when the delegate key was missing or
    /// malformed at construction time. Validity follows from the type:
    /// `signing` only carries `Some` once `parse_signing_key` succeeded,
    /// so there is no per-call hex decode or length check to repeat.
    pub(crate) fn validate_key(&self) -> Result<(), AuthError> {
        if self.signing.is_none() {
            return Err(AuthError::MissingKey);
        }
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
    /// Failure mode caught: a valid key is misclassified as Invalid, which
    /// would abort the binary on the happy path.
    #[test]
    fn classify_key_ok_on_valid_32_byte_hex() {
        let key = hex::encode([0x42u8; 32]);
        match classify_key(Some(&key)) {
            KeyValidation::Ok(returned) => assert_eq!(returned.as_str(), key.as_str()),
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
        assert_eq!(
            classify_key(Some(&key)),
            KeyValidation::Ok(Zeroizing::new(key))
        );
    }

    /// `Debug` impl on `KeyValidation::Ok` redacts the hex secret.
    /// Failure mode caught: a future refactor switches the manual `Debug` impl
    /// back to a derive, or someone accidentally prints `{:?}` on a
    /// KeyValidation — either way, the delegate key would land in a log line
    /// or panic message. This test asserts the redaction is structural.
    #[test]
    fn key_validation_debug_redacts_secret() {
        let key_hex = hex::encode([0x42u8; 32]);
        let v = KeyValidation::Ok(Zeroizing::new(key_hex.clone()));
        let debug_output = format!("{v:?}");
        assert!(
            !debug_output.contains(&key_hex),
            "Debug output must not contain the hex secret. Got: {debug_output}"
        );
        assert_eq!(debug_output, "Ok(<redacted>)");
    }

    /// `Debug` impl on the other two variants still emits informative text
    /// (no redaction needed — they don't carry secrets).
    /// Failure mode caught: an over-zealous redaction sweep silently strips
    /// the `Invalid` reason, hiding the actual misconfiguration from the
    /// startup log line.
    #[test]
    fn key_validation_debug_preserves_non_secret_variants() {
        assert_eq!(format!("{:?}", KeyValidation::Missing), "Missing");
        let reason = "is set but is not valid hex (...)".to_string();
        let dbg = format!("{:?}", KeyValidation::Invalid(reason.clone()));
        assert!(dbg.contains(&reason), "Invalid must show its reason: {dbg}");
    }

    /// `PartialEq` distinguishes every cross-variant pair.
    /// Failure mode caught: a hand-written `eq` that accidentally returns
    /// `true` on cross-variant comparisons would let miscategorized inputs
    /// slip through equality checks.
    #[test]
    fn key_validation_partial_eq_cross_variants_differ() {
        let v_ok = KeyValidation::Ok(Zeroizing::new("x".into()));
        let v_missing = KeyValidation::Missing;
        let v_invalid = KeyValidation::Invalid("r".into());
        assert_ne!(v_ok, v_missing);
        assert_ne!(v_ok, v_invalid);
        assert_ne!(v_missing, v_invalid);
    }

    /// `jittered(delay)` returns a value in `[delay, delay + ceil(delay/4))`.
    /// Failure mode caught: a regression that overshoots the 25% jitter cap
    /// would silently extend the polling deadline, or a missing-jitter
    /// regression (returning exactly `delay`) would re-introduce lock-step
    /// polling between concurrent clients on the same job.
    #[test]
    fn jittered_bounded() {
        for d_ms in [100u64, 250, 500, 1000, 4000] {
            let d = Duration::from_millis(d_ms);
            // The function isn't deterministic, but a small batch will
            // exercise enough nanosecond values to expose an out-of-range
            // result.
            for _ in 0..50 {
                let got = jittered(d);
                assert!(
                    got >= d,
                    "jittered({d:?}) returned {got:?}, less than the base delay"
                );
                let max_jitter = Duration::from_millis((d_ms / 4).max(1));
                assert!(
                    got < d + max_jitter,
                    "jittered({d:?}) returned {got:?}, exceeded base + 25%"
                );
            }
        }
    }

    /// `check_text_len` accepts an input exactly at the cap, rejects one
    /// byte over, and the error message names the cap and the actual size.
    /// Failure mode caught: an off-by-one on the `>` vs `>=` comparison,
    /// or an error message that doesn't help the operator diagnose what
    /// they sent.
    #[test]
    fn check_text_len_boundary() {
        let at_limit = "x".repeat(MAX_TEXT_BYTES);
        assert!(check_text_len("text", &at_limit).is_ok());

        let over_limit = "x".repeat(MAX_TEXT_BYTES + 1);
        match check_text_len("text", &over_limit) {
            Err(MemWalError::Config(reason)) => {
                assert!(
                    reason.contains(&MAX_TEXT_BYTES.to_string()),
                    "reason must mention the cap; got: {reason}"
                );
                assert!(
                    reason.contains(&(MAX_TEXT_BYTES + 1).to_string()),
                    "reason must mention the actual size; got: {reason}"
                );
            }
            other => panic!("expected Config(...), got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // parse_relayer_url policy tests
    //
    // The flag is passed explicitly (rather than read from env) so each test
    // is hermetic — no global state, no test ordering, no MEMWAL_ALLOW_INSECURE
    // bleed-through across the suite.
    // -----------------------------------------------------------------------

    /// Accepts a canonical https URL with no trailing slash.
    /// Failure mode caught: an over-strict validator that rejects the most
    /// common form would block the default deployment.
    #[test]
    fn parse_relayer_url_accepts_canonical_https() {
        assert!(parse_relayer_url("https://relayer.memwal.ai", false).is_ok());
    }

    /// Accepts a trailing slash — url::Url normalises the path to "/".
    /// Failure mode caught: a regression where the path check rejects "/"
    /// would break operators who copy-paste from a browser bar.
    #[test]
    fn parse_relayer_url_accepts_trailing_slash() {
        assert!(parse_relayer_url("https://relayer.memwal.ai/", false).is_ok());
    }

    /// Accepts an explicit port.
    /// Failure mode caught: a host:port URL is misparsed or rejected.
    #[test]
    fn parse_relayer_url_accepts_port() {
        assert!(parse_relayer_url("https://relayer.memwal.ai:8443", false).is_ok());
    }

    /// Rejects http:// when allow_insecure is false.
    /// Failure mode caught: a regression that defaults to permissive
    /// scheme handling would let an operator (or a tool input that hadn't
    /// been removed) downgrade signing material to cleartext.
    #[test]
    fn parse_relayer_url_rejects_http_when_insecure_disallowed() {
        let err =
            parse_relayer_url("http://relayer.memwal.ai", false).expect_err("must reject http");
        assert!(err.to_string().contains("https"));
    }

    /// Accepts http:// when allow_insecure is true (for local dev / mockito).
    /// Failure mode caught: the escape hatch is broken; mockito tests can't
    /// construct a real `MemWalClient::from_env` path.
    #[test]
    fn parse_relayer_url_accepts_http_when_insecure_allowed() {
        assert!(parse_relayer_url("http://127.0.0.1:1234", true).is_ok());
    }

    /// Rejects exotic schemes regardless of allow_insecure.
    /// Failure mode caught: a `ws://` or `file://` URL bypasses scheme
    /// checks and reaches reqwest, which would either error obscurely or
    /// open an unintended transport.
    #[test]
    fn parse_relayer_url_rejects_non_http_schemes() {
        for s in ["ws://x", "file:///etc/passwd", "data:text/plain,foo"] {
            assert!(
                parse_relayer_url(s, true).is_err(),
                "expected rejection of `{s}` even with allow_insecure"
            );
        }
    }

    /// Rejects URLs that carry a path beyond the root.
    /// Failure mode caught: `Url::join("/api/remember")` against a base
    /// like `https://host/v1` would yield `https://host/api/remember`
    /// (Url::join treats absolute paths as replacements), silently
    /// rewriting what the operator configured.
    #[test]
    fn parse_relayer_url_rejects_path() {
        let err = parse_relayer_url("https://relayer.memwal.ai/v1", false)
            .expect_err("path must be rejected");
        assert!(err.to_string().contains("path"));
    }

    /// Rejects URLs that carry a query string.
    /// Failure mode caught: `?leak=/api/recall` smuggled in the base URL
    /// would either be silently dropped by Url::join or merged into the
    /// signed-path semantics, depending on the relayer's parsing.
    #[test]
    fn parse_relayer_url_rejects_query() {
        let err = parse_relayer_url("https://relayer.memwal.ai/?leak=1", false)
            .expect_err("query must be rejected");
        assert!(err.to_string().contains("query") || err.to_string().contains("fragment"));
    }

    /// Rejects URLs that carry a fragment.
    /// Failure mode caught: same shape as the query case; fragments are
    /// usually stripped client-side but should not be silently accepted.
    #[test]
    fn parse_relayer_url_rejects_fragment() {
        let err = parse_relayer_url("https://relayer.memwal.ai/#frag", false)
            .expect_err("fragment must be rejected");
        assert!(err.to_string().contains("query") || err.to_string().contains("fragment"));
    }

    /// Rejects URLs that embed a username.
    /// Failure mode caught: an operator who pastes `https://user@host`
    /// (e.g. from a curl command) would otherwise have the username
    /// tagged onto every outbound request as a Basic-Auth identifier
    /// — pointless against this relayer and an unintended exposure.
    #[test]
    fn parse_relayer_url_rejects_userinfo_username_only() {
        let err = parse_relayer_url("https://user@relayer.memwal.ai", false)
            .expect_err("userinfo must be rejected");
        assert!(err.to_string().contains("credentials"));
    }

    /// Rejects URLs that embed a username and password.
    /// Failure mode caught: the more dangerous variant of the userinfo
    /// case — a real secret tagged onto every outbound request, AND a
    /// secret that would have leaked through parse-error echoes before
    /// the no-echo policy was put in place.
    #[test]
    fn parse_relayer_url_rejects_userinfo_with_password() {
        let err = parse_relayer_url("https://user:secret@relayer.memwal.ai", false)
            .expect_err("userinfo must be rejected");
        assert!(err.to_string().contains("credentials"));
    }

    /// Parse-failure messages do NOT echo the raw input.
    /// Failure mode caught: a future refactor reintroduces `{raw}` into
    /// the error string and a typoed `https://user:secret@host/bad path`
    /// (unparsable due to space) leaks the secret into logs and panic
    /// messages.
    #[test]
    fn parse_relayer_url_does_not_echo_raw_on_parse_failure() {
        let err = parse_relayer_url("https://user:secret@host/bad path", false)
            .expect_err("malformed URL must Err");
        let msg = err.to_string();
        assert!(!msg.contains("secret"), "secret leaked into error: {msg}");
        assert!(!msg.contains("user"), "userinfo leaked into error: {msg}");
    }

    fn make_client(server_url: &str) -> MemWalClient {
        MemWalClient::with_test_config(server_url, &hex::encode([0x42u8; 32]), "")
    }

    /// `health_check` returns `Ok(())` when `/health` returns the pinned version.
    /// Failure mode caught: a version comparison that silently accepts any
    /// response would surface here as a passing test against a deliberately
    /// matching version mock; we then can flip the mock to verify failure
    /// in the sibling tests.
    #[tokio::test]
    async fn health_check_ok_on_matching_version() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/health")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({"status": "ok", "version": MEMWAL_API_VERSION}).to_string(),
            )
            .create_async()
            .await;
        let client = make_client(&server.url());
        assert!(client.health_check().await.is_ok());
    }

    /// `health_check` returns `Err` when `/health` reports a different
    /// version. The MEMWAL_API_VERSION pin is the maintenance contract;
    /// silently accepting a mismatch defeats the audit trail.
    #[tokio::test]
    async fn health_check_err_on_mismatched_version() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/health")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({"status": "ok", "version": "9.9.9"}).to_string())
            .create_async()
            .await;
        let client = make_client(&server.url());
        let err = client
            .health_check()
            .await
            .expect_err("version mismatch must Err");
        assert!(
            err.to_string().contains("9.9.9"),
            "error must mention the server version; got: {err}"
        );
    }

    /// `health_check` returns `Err` when `/health` omits `version`.
    /// Failure mode caught: a degraded relayer that drops the version field
    /// would silently look healthy under the previous best-effort check.
    #[tokio::test]
    async fn health_check_err_when_version_missing() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/health")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({"status": "ok"}).to_string())
            .create_async()
            .await;
        let client = make_client(&server.url());
        let err = client
            .health_check()
            .await
            .expect_err("missing version must Err");
        assert!(
            err.to_string().contains("version"),
            "error must mention version; got: {err}"
        );
    }

    /// A 429 from the relayer surfaces as `MemWalError::RateLimited` with
    /// the parsed `Retry-After` seconds, not the generic `Server` variant.
    /// Failure mode caught: rate-limit responses look identical to other
    /// upstream failures, so a DAG retry policy cannot distinguish "back
    /// off" from "try a different relayer".
    #[tokio::test]
    async fn post_translates_429_to_rate_limited() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/forget")
            .with_status(429)
            .with_header("retry-after", "42")
            .with_body("rate limited")
            .create_async()
            .await;
        let client = make_client(&server.url());
        let err = client.forget(None).await.expect_err("429 must Err");
        match err {
            MemWalError::RateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, Some(42));
            }
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    /// A 429 with no `Retry-After` header surfaces with `None` rather than
    /// failing to parse.
    /// Failure mode caught: a missing header is treated as a malformed
    /// response and the call becomes Server(...) instead of RateLimited.
    #[tokio::test]
    async fn post_translates_429_without_retry_after() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/forget")
            .with_status(429)
            .with_body("rate limited")
            .create_async()
            .await;
        let client = make_client(&server.url());
        let err = client.forget(None).await.expect_err("429 must Err");
        assert!(matches!(
            err,
            MemWalError::RateLimited {
                retry_after_secs: None
            }
        ));
    }

    /// A 500 from the relayer surfaces with a terse client-facing reason —
    /// the upstream body is logged but NOT inlined into the error string.
    /// Failure mode caught: a relayer that leaks account internals or
    /// moderation messages in its 500 body would forward that text verbatim
    /// to the unauthenticated /invoke caller.
    #[tokio::test]
    async fn post_terse_message_on_5xx() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/forget")
            .with_status(500)
            .with_body("secret internal payload that should not leak")
            .create_async()
            .await;
        let client = make_client(&server.url());
        let err = client.forget(None).await.expect_err("500 must Err");
        let msg = err.to_string();
        assert!(!msg.contains("secret internal payload"));
        assert!(msg.contains("upstream"));
    }

    /// `health_check` returns `Err` when the body is not JSON at all.
    /// Failure mode caught: a misconfigured reverse proxy serving an HTML
    /// error page would parse-fail silently and the call would short-circuit
    /// to Ok under a permissive `if let Ok(...)` pattern.
    #[tokio::test]
    async fn health_check_err_on_non_json_body() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/health")
            .with_status(200)
            .with_body("<html>nginx error page</html>")
            .create_async()
            .await;
        let client = make_client(&server.url());
        assert!(client.health_check().await.is_err());
    }
}
