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

/// One-shot guard so the "missing delegate key" warning is emitted at most
/// once per process even though `from_env` runs once per registered tool.
///
/// `Once::call_once` is a blocking primitive — anything inside the closure
/// blocks every other caller until it returns. The closure must remain
/// trivially fast (a single `log::warn!` is fine); never add filesystem,
/// network, or any work that can spuriously stall.
static WARN_MISSING_KEY: Once = Once::new();

/// Load `<cwd>/.env` into the process environment if it exists.
///
/// Restricted to the current working directory (no parent walk) so a `.env`
/// planted at `/`, `/etc/`, or any ancestor of the binary's cwd cannot
/// influence the process. Existing exports always win — variables already
/// set in the environment are not overwritten.
///
/// Must be called from `main` **before** the tokio runtime is built so the
/// `set_var` calls happen single-threaded, even though the practical race
/// window in this binary is empty.
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

/// Classification of the delegate key value read from the environment.
///
/// "Valid" means: the hex decodes to exactly 32 bytes — the Ed25519 scalar
/// shape used by both MemWal's relayer auth and Sui's default account-key
/// flavour. Any 32 raw bytes form a valid Ed25519 secret (ed25519-dalek
/// SHA-512-hashes them to derive the scalar), so this is the strongest check
/// we can do client-side. The relayer additionally verifies that the
/// derived public key is registered on chain as a delegate for a MemWal
/// account — that authority check can only happen server-side.
/// Classification result with a hand-written `Debug` impl that redacts the
/// secret hex in the `Ok` variant. Auto-deriving `Debug` would let any
/// future `{:?}` print dump the delegate key into a log line or panic
/// message; this impl makes that impossible by construction.
enum KeyValidation {
    /// Key is set and decodes to 32 bytes. Carries the original hex string
    /// wrapped in `Zeroizing` so the heap buffer is wiped on drop.
    Ok(Zeroizing<String>),
    /// Env var is unset or empty. Boot continues; signed calls will fail.
    Missing,
    /// Env var is set but malformed. Carries an operator-facing reason.
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

/// Run the delegate-key validation eagerly at process startup.
///
/// `bootstrap!` constructs `NexusTool` instances lazily on first request, so
/// without this hook a malformed-key misconfiguration would only surface
/// when an `/invoke` actually arrived — long after `server-start` reported
/// "Ready". Call this from `main` before handing control to the toolkit;
/// `main` is the only site permitted to exit the process on error.
pub(crate) fn validate_credentials_at_startup() -> Result<(), String> {
    read_validated_private_key().map(|_| ())
}

/// Read and validate `MEMWAL_DELEGATE_PRIVATE_KEY`.
///
/// - **Set and valid** → `Ok(Some(hex))`.
/// - **Unset / empty** → `Ok(None)` after a one-shot warning. The binary
///   keeps booting so `/tools` listing and process liveness still work;
///   signed calls fail at signature time with `AuthError::MissingKey`.
/// - **Set but malformed** → `Err(reason)`. `main` translates this to a
///   process exit at startup; `from_env` (called per tool boot or per
///   invocation) downgrades it to an error log + empty key so a single
///   misconfigured rotate doesn't kill in-flight requests.
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
/// Reference constant — operators select this by setting `MEMWAL_SERVER_URL`
/// in the deployment environment rather than via a Rust code path.
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

/// First poll fires after this delay so a job that completes quickly is
/// observed without waiting a full back-off cycle.
const POLL_INITIAL: Duration = Duration::from_millis(100);

/// Upper bound on the inter-poll delay during exponential backoff. Caps how
/// much rate-limit budget a single slow job can spend.
const POLL_MAX: Duration = Duration::from_secs(4);

/// Wall-clock budget for a single `poll_job` / `poll_bulk_jobs` call before
/// it gives up with `MemWalError::Timeout`. Sized for Walrus tail-latency
/// — erasure-coded shard replication can take 30-60 s under load.
const POLL_BUDGET: Duration = Duration::from_secs(60);

/// Exponential backoff with capping. Each call doubles the delay until it
/// reaches POLL_MAX.
fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(POLL_MAX)
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

/// Server-side per-batch cap. Mirrors the relayer's `MAX_BULK_ITEMS = 20`
/// in `services/server/src/routes.rs` at the pinned tag — kept in sync so
/// the tool's error message matches the relayer's behavior. Validated at
/// the tool boundary so a 21-item batch surfaces as a clean tool-level
/// reason rather than an opaque HTTP 400.
pub(crate) const MAX_BULK_ITEMS: usize = 20;

/// Upper bound on each tool's primary text input. Sized to comfortably hold
/// a long document while keeping signature material and outbound bandwidth
/// reasonable. The relayer enforces its own size limits server-side; this
/// is a defensive cap at the tool boundary so oversized inputs fail
/// immediately instead of consuming a signed request slot and a rate-limit
/// point.
pub(crate) const MAX_TEXT_BYTES: usize = 1 << 20; // 1 MiB

/// Translate a non-2xx response into a `MemWalError`, distinguishing rate
/// limits (so callers can back off intelligently) from generic upstream
/// failures (where the full body is logged structurally but only a terse
/// status family is surfaced to the unauthenticated /invoke caller).
///
/// **Logging sensitivity:** the body snippet logged under
/// `target = "memwal::upstream"` can contain account hints, moderation
/// messages quoting stored memory text, or other relayer internals. The
/// snippet is capped at 256 chars but is NOT redacted. Operators who treat
/// their log files as a lower-trust channel than the binary itself should
/// filter via `RUST_LOG=memwal::upstream=off` (or `=error`) and rely on
/// the terse client-facing error returned below.
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

/// Lifecycle states emitted by the relayer's job-state machine.
///
/// `#[serde(other)]` Unknown captures any status string the relayer
/// introduces later: the polling loops surface Unknown as an error rather
/// than looping on it indefinitely, so a new pre-pinned-tag status cannot
/// stall the call.
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

/// Response from `GET /health`. The relayer reports `{"status": "ok",
/// "version": "0.1.0", "mode": "production"}` — additional fields are
/// allowed and ignored. We require `version` so missing/typo cases fail
/// loudly per the maintenance-pin contract on [`MEMWAL_API_VERSION`].
#[derive(Deserialize)]
struct HealthResponse {
    version: Option<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Process-wide HTTP client, shared across every `MemWalClient` so the
/// connection pool, TLS session cache, and HTTP/2 multiplexing survive
/// across Nexus `invoke` calls.
///
/// `reqwest::Client` wraps an `Arc<ClientRef>` internally, so cloning is a
/// cheap Arc bump. The builder options below are conservative defaults — a
/// hung relayer terminates the per-request future after 30 s instead of
/// parking the calling task indefinitely.
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

/// True when the operator has explicitly opted in to non-HTTPS relayer URLs
/// for local development. Production deploys must leave this unset.
fn allow_insecure() -> bool {
    std::env::var("MEMWAL_ALLOW_INSECURE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Parse a candidate relayer base URL against the relayer-URL policy.
///
/// - Requires `https://` unless `allow_insecure` is `true`, in which case
///   `http://` is also accepted. The flag is parameterised (rather than
///   read from env here) so the policy is pure-testable.
/// - Rejects paths, queries, and fragments in the base — `Url::join` would
///   otherwise concatenate them with the per-call path and silently rewrite
///   the signed message's path segment.
/// - Rejects userinfo (`https://user:secret@host`) — basic-auth has no
///   place on a relayer URL that authenticates via signed headers, and
///   leaving it in would tag every outbound request with the credentials.
fn parse_relayer_url(raw: &str, allow_insecure: bool) -> Result<Url, MemWalError> {
    // Don't echo the raw URL in the parse-failure message: an operator who
    // accidentally embedded credentials (`https://user:secret@host/...`)
    // and then mistyped the rest would otherwise leak them into stderr,
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
    /// `Some` once the delegate key parsed successfully. `None` when the
    /// env var was missing or malformed at construction; signed calls then
    /// return `AuthError::MissingKey` so the failure mode is a clean 4xx
    /// at the tool boundary rather than a process exit.
    signing: Option<Arc<SigningMaterial>>,
    /// MemWal account object ID. Empty string when not configured — the
    /// relayer then resolves the account from the public key via on-chain
    /// scan. Signed into the canonical message; sent as `x-account-id`
    /// header only when non-empty (mirrors the JS SDK).
    account_id: Arc<str>,
}

struct SigningMaterial {
    signing_key: SigningKey,
    public_key_hex: String,
}

impl MemWalClient {
    /// Build a `MemWalClient` from environment variables. This is the
    /// production constructor.
    ///
    /// `MEMWAL_SERVER_URL` overrides the relayer URL; missing or empty
    /// values fall back to `DEFAULT_SERVER_URL`. The URL is validated
    /// (https-only unless `MEMWAL_ALLOW_INSECURE=1`) and parse failure
    /// returns `MemWalError::Config` rather than panicking.
    ///
    /// Delegates key handling to [`read_validated_private_key`]: a missing
    /// key produces a one-shot warning and the client boots with no
    /// signing material; a malformed key logs an error and behaves the
    /// same way. Startup-time validation in `main` is the right place to
    /// fail-fast on persistent misconfiguration.
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

    /// `GET /api/remember/:job_id` — poll until the job reaches a terminal
    /// state. Returns the `blob_id` on success.
    ///
    /// Polling cadence is exponential: a 100 ms initial probe (so jobs that
    /// finish quickly resolve quickly), doubling up to a 4 s ceiling, until
    /// the 60 s wall-clock budget is exhausted. An unrecognized status from
    /// the server is treated as an error rather than looped on so a future
    /// status addition cannot stall the call indefinitely.
    ///
    /// **Cancel-safety:** the polling loop itself is cancel-safe — dropping
    /// the returned future releases the HTTP connection and the timer
    /// cleanly. **The server-side job is not cancellable**: once the initial
    /// POST returned 202, the relayer writes the Walrus blob regardless of
    /// whether the caller still listens. A dropped poll leaks one blob
    /// reference; operators sensitive to storage cost must not cancel.
    pub(crate) async fn poll_job(&self, job_id: &str) -> Result<String, MemWalError> {
        let path = format!("/api/remember/{job_id}");
        let deadline = Instant::now() + POLL_BUDGET;
        let mut delay = POLL_INITIAL;

        loop {
            sleep(delay).await;
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

    /// `POST /api/remember/bulk` — submit up to MAX_BULK_ITEMS texts in a single
    /// 202-Accepted call. Returns one job_id per item, in the order submitted.
    /// Each job still needs polling — use [`poll_bulk_jobs`] for the batched
    /// status endpoint instead of N separate [`poll_job`] calls.
    ///
    /// Callers should enforce the `1..=MAX_BULK_ITEMS` cap before calling so
    /// oversized batches fail at the tool boundary instead of round-tripping
    /// to the relayer for an opaque HTTP 400.
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

    /// `POST /api/remember/bulk/status` — poll batched jobs until every one
    /// reaches a terminal state or the wall-clock budget runs out. Returns
    /// blob_ids in the same order as `job_ids`. Any individual `failed`
    /// surfaces as `Err(JobFailed)` identifying the first failure.
    ///
    /// Two optimizations over the naïve poll-all-every-time approach:
    /// - A terminal-state `HashMap` caches jobs that already finished, so
    ///   each subsequent poll only queries the still-pending subset
    ///   (relayer pays less work; rate-limit budget shrinks with completion).
    /// - Lookups during result assembly are O(1) via the same map — no
    ///   per-poll O(N²) linear scan.
    ///
    /// **Cancel-safety:** same caveat as [`MemWalClient::poll_job`] — the
    /// local loop drops cleanly, but the server-side bulk writes complete
    /// regardless. Dropping mid-poll can leak up to `MAX_BULK_ITEMS` blobs.
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

            sleep(delay).await;
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

    /// `GET /health` — check whether the relayer is reachable and on the
    /// expected API version.
    ///
    /// The health endpoint is public (no auth required). The response must
    /// be JSON with a `"version"` string field that matches
    /// [`MEMWAL_API_VERSION`]; a missing field, wrong type, non-JSON body,
    /// or mismatched version is a failure. The maintenance-pin contract
    /// stated on [`MEMWAL_API_VERSION`] depends on this check actually
    /// being enforced — a "best-effort" version check is the same as no
    /// check at all.
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
