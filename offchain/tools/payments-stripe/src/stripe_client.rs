//! Stateless Stripe HTTP client.
//!
//! Credential model (audit checks C1, C7, C8, C9, C10):
//!
//! - `STRIPE_API_KEY` is read once at startup via [`from_env`]. The
//!   bearer is wrapped in [`zeroize::Zeroizing<String>`] so the heap
//!   buffer is wiped on drop. The struct hand-implements `Debug` to
//!   print `<redacted>` — never `#[derive(Debug)]`.
//! - The credential never appears on the `Input` struct of any tool —
//!   tool inputs flow through the Nexus DAG on Sui as plaintext.
//! - The struct's only mutable per-request field is the optional
//!   `Idempotency-Key` (Stripe-style writes); cheap to clone, no
//!   shared state.
//! - In Cloud Run the env var is mounted by Secret Manager via a
//!   `secretKeyRef` binding configured by the operator (out-of-band).
//!   The deploy pipeline does NOT provision the upstream API key.
//!
//! Canonical reference: `offchain/tools/memory-memwal/src/client.rs`.

use {
    crate::{
        error::{try_parse_api_error, StripeErrorKind, StripeErrorResponse},
        tools::STRIPE_API_BASE,
    },
    reqwest::{Client, RequestBuilder},
    serde::{de::DeserializeOwned, Serialize},
    std::sync::{Arc, Once, OnceLock},
    zeroize::Zeroizing,
};

/// Env var holding the Stripe secret key (`sk_test_…` or `sk_live_…`).
/// Mounted in Cloud Run via `secretKeyRef`; configured by the operator
/// out-of-band, not by the deploy pipeline.
pub(crate) const ENV_API_KEY: &str = "STRIPE_API_KEY";

/// One-shot guard so the "missing key" warning fires once even though
/// `from_env` runs once per registered tool (six tools = six `new()` calls).
static WARN_MISSING_KEY: Once = Once::new();

/// Process-wide HTTP client shared across every `StripeClient` so the
/// connection pool, TLS session cache, and HTTP/2 multiplexing survive
/// across `invoke` calls. `reqwest::Client` clone is a cheap Arc bump.
static SHARED_HTTP: OnceLock<Client> = OnceLock::new();

fn shared_http() -> Client {
    SHARED_HTTP
        .get_or_init(|| {
            Client::builder()
                .user_agent("nexus-sdk-payments-stripe/1.0")
                .build()
                .expect("Failed to create HTTP client")
        })
        .clone()
}

/// Load `<cwd>/.env` if present. Cwd-only (no parent walk) so a planted
/// `.env` above the binary's cwd cannot influence the process. Existing
/// exports always win. MUST be called before the tokio runtime is built —
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

/// Classification of the Stripe key env var. Hand-written `Debug` redacts
/// the secret — never `derive` Debug on this type.
enum KeyValidation {
    Ok(Zeroizing<String>),
    Missing,
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

/// Pure classifier — no I/O, no exits, deterministic.
fn classify_key(value: Option<&str>) -> KeyValidation {
    let raw = match value {
        Some(s) if !s.is_empty() => s,
        _ => return KeyValidation::Missing,
    };
    // Stripe secret keys are prefixed `sk_test_` (test mode) or
    // `sk_live_` (production). Restricted keys use `rk_test_` / `rk_live_`.
    // Reject anything else as a typo / wrong env var.
    let valid_prefix = ["sk_test_", "sk_live_", "rk_test_", "rk_live_"]
        .iter()
        .any(|p| raw.starts_with(p));
    if !valid_prefix {
        return KeyValidation::Invalid(
            "is set but does not start with a recognized Stripe prefix \
             (sk_test_, sk_live_, rk_test_, rk_live_)."
                .to_string(),
        );
    }
    if raw.len() < 32 {
        return KeyValidation::Invalid(format!(
            "is set but is shorter than expected ({} chars). Real Stripe \
             keys are ≥32 chars after the prefix.",
            raw.len()
        ));
    }
    KeyValidation::Ok(Zeroizing::new(raw.to_string()))
}

/// Eager startup-time key validation. Without this, a malformed key would
/// only surface on the first `/invoke` (well after `server-start` reports
/// "Ready"). `main` is the only `process::exit` site.
pub(crate) fn validate_credentials_at_startup() -> Result<(), String> {
    read_validated_api_key().map(|_| ())
}

/// `Ok(Some(key))` valid; `Ok(None)` unset (warn-and-continue); `Err(reason)`
/// malformed. `main` exits on `Err`; `from_env` downgrades it to a log so a
/// mid-process key rotation doesn't kill in-flight requests.
fn read_validated_api_key() -> Result<Option<Zeroizing<String>>, String> {
    // `env::var` returns a fresh `String` — wrap it immediately so the
    // intermediate copy is also zeroed on drop.
    let raw = std::env::var(ENV_API_KEY).ok().map(Zeroizing::new);
    match classify_key(raw.as_deref().map(|z| z.as_str())) {
        KeyValidation::Ok(k) => Ok(Some(k)),
        KeyValidation::Missing => {
            WARN_MISSING_KEY.call_once(|| {
                log::warn!(
                    "{ENV_API_KEY} is not set — every invoke will fail \
                     upstream with 401 until this env var is exported in \
                     the process environment."
                );
            });
            Ok(None)
        }
        KeyValidation::Invalid(reason) => Err(reason),
    }
}

#[derive(Clone)]
pub struct StripeClient {
    client: Arc<Client>,
    base_url: String,
    /// Stripe secret key read once at startup from `ENV_API_KEY`. Wrapped
    /// in `Zeroizing` so heap buffers are wiped on drop. `None` if the
    /// env var was missing or malformed — `invoke()` calls will fail
    /// upstream with 401, which is the right signal to the operator.
    bearer: Option<Arc<Zeroizing<String>>>,
    /// Per-request idempotency key (Stripe-style writes). Set via
    /// `.with_idempotency(...)` on the cheap-cloned client.
    idempotency_key: Option<String>,
}

// Hand-written Debug — NEVER derive on a type holding a credential.
impl std::fmt::Debug for StripeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeClient")
            .field("base_url", &self.base_url)
            .field(
                "bearer",
                &if self.bearer.is_some() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .field("idempotency_key", &self.idempotency_key)
            .finish()
    }
}

impl StripeClient {
    /// Production constructor: reads `STRIPE_API_KEY` from the process
    /// env and wraps it in `Zeroizing`. A missing/malformed key logs but
    /// does not error here — `main`'s startup validation is the
    /// fail-fast site.
    pub fn from_env() -> Result<Self, String> {
        let bearer = match read_validated_api_key() {
            Ok(Some(k)) => Some(Arc::new(k)),
            Ok(None) => None,
            Err(reason) => {
                log::error!("{ENV_API_KEY} {reason}");
                None
            }
        };
        Ok(Self {
            client: Arc::new(shared_http()),
            base_url: STRIPE_API_BASE.to_string(),
            bearer,
            idempotency_key: None,
        })
    }

    /// Test-only constructor. Accepts a base URL (HTTP loopback for
    /// mockito) and a bearer string directly, bypassing env validation.
    #[cfg(test)]
    pub(crate) fn for_testing(base_url: &str, bearer: &str) -> Self {
        Self {
            client: Arc::new(
                Client::builder()
                    .user_agent("nexus-sdk-payments-stripe/1.0")
                    .build()
                    .expect("failed to build test HTTP client"),
            ),
            base_url: base_url.to_string(),
            bearer: Some(Arc::new(Zeroizing::new(bearer.to_string()))),
            idempotency_key: None,
        }
    }

    /// Attach an `Idempotency-Key` header for the next request. Required
    /// by Stripe writes; harmless on reads.
    #[must_use]
    pub fn with_idempotency(mut self, key: &str) -> Self {
        self.idempotency_key = Some(key.to_string());
        self
    }

    pub async fn get<T>(&self, endpoint: &str) -> Result<T, StripeErrorResponse>
    where
        T: DeserializeOwned,
    {
        let req = self.apply_headers(self.client.get(self.url(endpoint)));
        self.send(req).await
    }

    /// Stripe uses `application/x-www-form-urlencoded` for write bodies,
    /// not JSON. Pass a `serde_urlencoded`-compatible value.
    pub async fn post_form<T, B>(&self, endpoint: &str, body: &B) -> Result<T, StripeErrorResponse>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let req = self.apply_headers(self.client.post(self.url(endpoint)).form(body));
        self.send(req).await
    }

    fn url(&self, endpoint: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            endpoint.trim_start_matches('/')
        )
    }

    fn apply_headers(&self, mut req: RequestBuilder) -> RequestBuilder {
        if let Some(ref bearer) = self.bearer {
            req = req.bearer_auth(bearer.as_str());
        }
        if let Some(ref key) = self.idempotency_key {
            req = req.header("Idempotency-Key", key);
        }
        req
    }

    async fn send<T>(&self, req: RequestBuilder) -> Result<T, StripeErrorResponse>
    where
        T: DeserializeOwned,
    {
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(StripeErrorResponse {
                    reason: format!("Network error: {}", e),
                    kind: StripeErrorKind::from_network_error(&e),
                    status_code: Some(0),
                });
            }
        };

        let status = response.status();
        let text = match response.text().await {
            Ok(t) => t,
            Err(e) => {
                return Err(StripeErrorResponse {
                    reason: format!("Failed to read response: {}", e),
                    kind: StripeErrorKind::Parse,
                    status_code: None,
                });
            }
        };

        if !status.is_success() {
            if let Some(parsed) = try_parse_api_error(&text, status.as_u16()) {
                return Err(parsed);
            }
            return Err(StripeErrorResponse {
                reason: format!("Stripe API error ({}): {}", status, truncate(&text, 512)),
                kind: StripeErrorKind::from_status_code(status.as_u16()),
                status_code: Some(status.as_u16()),
            });
        }

        serde_json::from_str::<T>(&text).map_err(|e| StripeErrorResponse {
            reason: format!("Failed to parse JSON: {}", e),
            kind: StripeErrorKind::Parse,
            status_code: None,
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}
