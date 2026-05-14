//! Walrus client configuration and HTTP transport setup.
//!
//! ## URL resolution
//!
//! When [`WalrusConfig::build`] runs, the publisher and aggregator URLs are
//! resolved independently in this order:
//!   1. Explicit value passed via [`WalrusConfig::with_publisher_url`] /
//!      [`WalrusConfig::with_aggregator_url`] (per-request, from the tool's
//!      JSON input).
//!   2. Env var ([`ENV_PUBLISHER_URL`] / [`ENV_AGGREGATOR_URL`]) — set on the
//!      container so ops can point the tool at a non-default publisher
//!      without changing every caller.
//!   3. SDK defaults (the public Walrus endpoints baked into `nexus_sdk`).
//!
//! ## Retry on publisher consistency 500
//!
//! When the publisher's Sui RPC is a load-balanced endpoint, the post-write
//! read of the freshly-minted `Blob` NFT may land on a different fullnode
//! than the write and return `NotExists`. The publisher surfaces this as a
//! `500` with a "response does not contain object data" message body.
//! [`with_publisher_retry`] detects this specific signature and retries
//! once after a configurable delay (default 2s, override via
//! [`ENV_PUBLISHER_RETRY_DELAY_SECS`]). All other errors are propagated.
//!
//! ## Authentication for Cloud Run publishers
//!
//! When the resolved publisher URL is a Google Cloud Run hostname
//! (`*.run.app`), the underlying [`reqwest::Client`] is built with a default
//! `X-Serverless-Authorization: Bearer <id_token>` header. The token is an
//! OIDC ID token fetched from the GCE metadata server with the publisher
//! URL as the audience claim, and is what authenticates this tool against
//! a publisher running on Cloud Run with `roles/run.invoker` IAM enforced.
//!
//! `X-Serverless-Authorization` is used (rather than `Authorization`)
//! because Cloud Run validates and then **strips** that header before
//! forwarding the request to the container. This is necessary because the
//! Walrus publisher binary parses any `Authorization: Bearer …` header it
//! receives as its own Walrus-JWT for replay protection, and rejects
//! Google's OIDC tokens since they lack the `jti` (JWT ID) claim.
//!
//! References:
//! - [Service-to-service authentication on Cloud Run]
//! - [Fetch an ID token from the metadata server]
//! - [Restricting ingress for Cloud Run]
//! - [Cloud Run IAM roles] — `roles/run.invoker`
//!
//! Failures of the metadata fetch (DNS, timeout, non-2xx, header build error)
//! degrade gracefully to a plain client. The request then surfaces a 401/403
//! from Cloud Run, which is the same outcome a misconfigured deployment
//! produces today and avoids hanging the publisher build forever — the
//! metadata fetch is bounded by [`METADATA_FETCH_TIMEOUT`].
//!
//! [Service-to-service authentication on Cloud Run]: https://cloud.google.com/run/docs/authenticating/service-to-service
//! [Fetch an ID token from the metadata server]: https://cloud.google.com/docs/authentication/get-id-token#metadata-server
//! [Restricting ingress for Cloud Run]: https://cloud.google.com/run/docs/securing/ingress#internal-services
//! [Cloud Run IAM roles]: https://cloud.google.com/run/docs/reference/iam/roles#standard-roles

use {
    nexus_sdk::walrus::{WalrusClient, WalrusError},
    reqwest::header::{HeaderMap, HeaderName, HeaderValue},
    std::{future::Future, time::Duration},
};

/// Header name Cloud Run uses to receive an OIDC ID token *without* forwarding
/// it to the container. We use this (rather than `Authorization`) because the
/// Walrus publisher parses any `Authorization: Bearer …` header as its own
/// Walrus-JWT for replay protection and rejects Google's OIDC tokens
/// (which lack the `jti` claim).
///
/// See <https://cloud.google.com/run/docs/authenticating/service-to-service#x_serverless_authorization>.
static X_SERVERLESS_AUTHORIZATION: HeaderName =
    HeaderName::from_static("x-serverless-authorization");

/// Env var providing a default Walrus publisher URL when input doesn't specify one.
const ENV_PUBLISHER_URL: &str = "WALRUS_PUBLISHER_URL";

/// Env var providing a default Walrus aggregator URL when input doesn't specify one.
const ENV_AGGREGATOR_URL: &str = "WALRUS_AGGREGATOR_URL";

/// GCE metadata server endpoint for fetching OIDC ID tokens for the
/// container's runtime service account. Reachable from any GCP compute
/// surface (Cloud Run, GKE, GCE) at the link-local address
/// `metadata.google.internal`. Requires the `Metadata-Flavor: Google`
/// header to defeat SSRF-style cross-origin requests.
///
/// See <https://cloud.google.com/docs/authentication/get-id-token#metadata-server>.
const METADATA_IDENTITY_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity";

/// Bound the metadata-server fetch so a misconfigured deployment fails fast
/// instead of hanging the publisher build forever.
const METADATA_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Configuration for Walrus client
#[derive(Default)]
pub struct WalrusConfig {
    /// The walrus publisher URL
    pub publisher_url: Option<String>,
    /// The URL of the aggregator
    pub aggregator_url: Option<String>,
}

impl WalrusConfig {
    /// Create a new WalrusConfig with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the publisher URL
    pub fn with_publisher_url(mut self, url: Option<String>) -> Self {
        self.publisher_url = url;
        self
    }

    /// Set the aggregator URL
    pub fn with_aggregator_url(mut self, url: Option<String>) -> Self {
        self.aggregator_url = url;
        self
    }

    /// Build a WalrusClient with the configured settings.
    ///
    /// URL resolution order (per side, publisher and aggregator):
    ///   1. Explicit value passed via `with_*_url`
    ///   2. Env var (`WALRUS_PUBLISHER_URL` / `WALRUS_AGGREGATOR_URL`)
    ///   3. SDK defaults (public Walrus endpoints)
    ///
    /// When the resolved publisher URL points at a private Google Cloud Run host
    /// (`*.run.app`), an OIDC ID token is fetched from the GCE metadata server
    /// and attached as a default `Authorization: Bearer` header on every request.
    /// This authenticates requests against Cloud Run services with
    /// `INGRESS_TRAFFIC_INTERNAL_ONLY` and `roles/run.invoker` enforcement.
    pub async fn build(self) -> WalrusClient {
        let publisher_url = self
            .publisher_url
            .or_else(|| std::env::var(ENV_PUBLISHER_URL).ok());
        let aggregator_url = self
            .aggregator_url
            .or_else(|| std::env::var(ENV_AGGREGATOR_URL).ok());

        let http_client = build_http_client(publisher_url.as_deref()).await;

        let mut client_builder = WalrusClient::builder().with_client(http_client);
        if let Some(ref url) = publisher_url {
            client_builder = client_builder.with_publisher_url(url);
        }
        if let Some(ref url) = aggregator_url {
            client_builder = client_builder.with_aggregator_url(url);
        }
        client_builder.build()
    }
}

/// Build a reqwest::Client that, when targeting a Cloud Run host, carries an OIDC
/// ID token as a default Authorization header. Falls back to a plain client on
/// any failure (the request will then fail at the publisher with a 401/403 if
/// auth is actually required, which is the same outcome as today's behaviour).
async fn build_http_client(publisher_url: Option<&str>) -> reqwest::Client {
    let Some(audience) = publisher_url.filter(|u| is_cloud_run_url(u)) else {
        return reqwest::Client::new();
    };

    match fetch_id_token(audience).await {
        Ok(token) => match HeaderValue::from_str(&format!("Bearer {token}")) {
            Ok(value) => {
                let mut headers = HeaderMap::new();
                // X-Serverless-Authorization (not Authorization) so Cloud Run
                // consumes the token for IAM and strips it before passing
                // the request on to the publisher container.
                headers.insert(X_SERVERLESS_AUTHORIZATION.clone(), value);
                reqwest::Client::builder()
                    .default_headers(headers)
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new())
            }
            Err(_) => reqwest::Client::new(),
        },
        Err(_) => reqwest::Client::new(),
    }
}

/// True if the URL looks like a Google Cloud Run service URL (e.g. *.run.app).
fn is_cloud_run_url(url: &str) -> bool {
    url.contains(".run.app")
}

/// Fetch an OIDC ID token for the given audience from the GCE metadata
/// server. The audience must match the verifier's expectation — for Cloud
/// Run that's the receiving service's URL.
///
/// The token returned in the response body is a JWT signed by Google; we
/// don't parse or validate it client-side, the receiving Cloud Run service
/// does that. See [Fetching identity tokens] in the Cloud Run docs.
///
/// [Fetching identity tokens]: https://cloud.google.com/run/docs/authenticating/service-to-service#acquire-token
async fn fetch_id_token(audience: &str) -> Result<String, reqwest::Error> {
    let url = format!("{METADATA_IDENTITY_URL}?audience={audience}&format=full");
    let response = reqwest::Client::builder()
        .timeout(METADATA_FETCH_TIMEOUT)
        .build()?
        .get(url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await?
        .error_for_status()?;
    response.text().await
}

/// Default backoff before retrying a publisher operation that hit the
/// read-after-write consistency race described on
/// [`is_transient_publisher_500`]. Two seconds is comfortably larger than a
/// Sui checkpoint (~250 ms) but small enough that the user-facing latency
/// stays acceptable.
const DEFAULT_PUBLISHER_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Env var used to override [`DEFAULT_PUBLISHER_RETRY_DELAY`]. Value is parsed
/// as an integer number of seconds (`u64::from_str`); empty / unset / invalid
/// values fall back to the default.
const ENV_PUBLISHER_RETRY_DELAY_SECS: &str = "WALRUS_PUBLISHER_RETRY_DELAY_SECS";

/// Resolve the retry delay from the env var, falling back to the default.
fn publisher_retry_delay() -> Duration {
    std::env::var(ENV_PUBLISHER_RETRY_DELAY_SECS)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PUBLISHER_RETRY_DELAY)
}

/// True if `err` is the publisher's `500 Internal` response caused by a
/// post-write Sui read landing on a different fullnode (behind the public RPC
/// load balancer) than the one that executed the storage transaction.
///
/// The publisher correctly stored the blob and minted the on-chain `Blob`
/// NFT, but its follow-up `sui_getObject` for that NFT returned `NotExists`
/// because the read hit a fullnode that hadn't observed the write yet. The
/// publisher surfaces this as:
///
/// ```text
/// 500 INTERNAL: "client internal error: response does not contain object data
///                [err=Some(NotExists { object_id: 0x… })]"
/// ```
///
/// Such a request is safe to retry: Walrus blob IDs are deterministic in the
/// content + encoding parameters, so the second PUT lands on the same blob
/// and the publisher returns `AlreadyCertified` once the chain state has
/// propagated across the load balancer's fullnodes.
fn is_transient_publisher_500(err: &WalrusError) -> bool {
    matches!(
        err,
        WalrusError::ApiError {
            status_code: 500,
            message,
        } if message.contains("response does not contain object data")
            && message.contains("NotExists")
    )
}

/// Run `attempt` once, and if it fails with [`is_transient_publisher_500`],
/// sleep [`PUBLISHER_RETRY_DELAY`] and run it once more. Any other failure
/// (including the retry's failure) is returned to the caller unchanged.
///
/// The single-retry policy is intentional: this isn't an unreliable-network
/// scenario worth exponential backoff. It's a brief consistency lag between
/// fullnodes behind the public Sui RPC load balancer, which clears in well
/// under [`PUBLISHER_RETRY_DELAY`] in practice.
pub async fn with_publisher_retry<F, Fut, T>(attempt: F) -> Result<T, WalrusError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, WalrusError>>,
{
    retry_with_delay(publisher_retry_delay(), attempt).await
}

/// Inner helper that exposes the delay as a parameter so tests can pass
/// [`Duration::ZERO`] and run without sleeping.
async fn retry_with_delay<F, Fut, T>(delay: Duration, mut attempt: F) -> Result<T, WalrusError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, WalrusError>>,
{
    match attempt().await {
        Ok(value) => Ok(value),
        Err(err) if is_transient_publisher_500(&err) => {
            eprintln!(
                "publisher returned transient 500 (Sui RPC read-after-write); \
                 retrying once after {}ms: {err}",
                delay.as_millis()
            );
            tokio::time::sleep(delay).await;
            attempt().await
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use {super::*, tokio::sync::Mutex};

    /// Serializes tests that mutate process-global env vars.
    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    #[test]
    fn cloud_run_url_detection() {
        assert!(is_cloud_run_url(
            "https://walrus-publisher-testnet-abc-uc.a.run.app"
        ));
        assert!(is_cloud_run_url("https://foo.run.app"));
        assert!(!is_cloud_run_url(
            "https://publisher.walrus-testnet.walrus.space"
        ));
        assert!(!is_cloud_run_url("http://localhost:8080"));
    }

    #[tokio::test]
    async fn build_http_client_returns_plain_for_no_url() {
        // None URL must skip the metadata-server path entirely.
        let _ = build_http_client(None).await;
    }

    #[tokio::test]
    async fn build_http_client_returns_plain_for_non_cloud_run_url() {
        // Non-Cloud-Run URL must skip the metadata-server path entirely.
        let _ = build_http_client(Some("https://publisher.walrus-testnet.walrus.space")).await;
    }

    #[tokio::test]
    async fn build_http_client_falls_back_when_metadata_unreachable() {
        // Cloud Run URL → fetch_id_token is invoked, which fails because
        // metadata.google.internal is unreachable from the test environment.
        // The fallback path should still return a usable client.
        let _ = build_http_client(Some("https://test-service-abc-uc.a.run.app")).await;
    }

    #[tokio::test]
    async fn build_with_no_input_no_env() {
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var(ENV_PUBLISHER_URL);
        std::env::remove_var(ENV_AGGREGATOR_URL);

        // No publisher_url, no aggregator_url, no env vars → SDK defaults are used.
        let _client = WalrusConfig::new().build().await;
    }

    #[tokio::test]
    async fn build_uses_env_var_when_input_missing() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(ENV_PUBLISHER_URL, "https://env-publisher.example.com");
        std::env::set_var(ENV_AGGREGATOR_URL, "https://env-aggregator.example.com");

        // Both env-var fallback branches are exercised.
        let _client = WalrusConfig::new().build().await;

        std::env::remove_var(ENV_PUBLISHER_URL);
        std::env::remove_var(ENV_AGGREGATOR_URL);
    }

    #[tokio::test]
    async fn build_input_overrides_env_var() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(ENV_PUBLISHER_URL, "https://env-publisher.example.com");

        // Explicit input should win over env var.
        let _client = WalrusConfig::new()
            .with_publisher_url(Some("https://input-publisher.example.com".to_string()))
            .with_aggregator_url(Some("https://input-aggregator.example.com".to_string()))
            .build()
            .await;

        std::env::remove_var(ENV_PUBLISHER_URL);
    }

    // --- Publisher retry classifier & helper ---

    /// The exact body the publisher returned in the trace that motivated this
    /// retry (mainnet match 0x4ed94a23…, execution 0x8dded729…).
    const TRANSIENT_500_BODY: &str = r#"{"error":{"status":"INTERNAL","code":500,"message":"client internal error: response does not contain object data [err=Some(NotExists { object_id: 0x376fc555774bfccbe3bf8967bc85d0bf1daf749fa57709b258b18a36633b594c })]"}}"#;

    fn transient_500() -> WalrusError {
        WalrusError::ApiError {
            status_code: 500,
            message: TRANSIENT_500_BODY.to_string(),
        }
    }

    #[test]
    fn transient_500_is_classified() {
        assert!(is_transient_publisher_500(&transient_500()));
    }

    #[test]
    fn other_500_is_not_classified_as_transient() {
        assert!(!is_transient_publisher_500(&WalrusError::ApiError {
            status_code: 500,
            message: "Internal Server Error: unrelated failure".to_string(),
        }));
    }

    #[test]
    fn non_500_status_is_not_classified_as_transient() {
        assert!(!is_transient_publisher_500(&WalrusError::ApiError {
            status_code: 404,
            message: "response does not contain object data NotExists".to_string(),
        }));
    }

    #[tokio::test]
    async fn retry_helper_retries_once_on_transient_500() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = AtomicUsize::new(0);

        let result: Result<u8, WalrusError> = retry_with_delay(Duration::ZERO, || {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n == 1 {
                    Err(transient_500())
                } else {
                    Ok(42u8)
                }
            }
        })
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn retry_helper_does_not_retry_on_other_errors() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = AtomicUsize::new(0);

        let result: Result<(), WalrusError> = retry_with_delay(Duration::ZERO, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(WalrusError::ApiError {
                    status_code: 503,
                    message: "Service Unavailable".to_string(),
                })
            }
        })
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            result,
            Err(WalrusError::ApiError {
                status_code: 503,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn retry_helper_does_not_retry_on_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = AtomicUsize::new(0);

        let result: Result<u8, WalrusError> = retry_with_delay(Duration::ZERO, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Ok(7u8) }
        })
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(result.unwrap(), 7);
    }

    #[tokio::test]
    async fn retry_delay_defaults_to_two_seconds() {
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var(ENV_PUBLISHER_RETRY_DELAY_SECS);
        assert_eq!(publisher_retry_delay(), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn retry_delay_is_overridable_via_env() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(ENV_PUBLISHER_RETRY_DELAY_SECS, "5");
        assert_eq!(publisher_retry_delay(), Duration::from_secs(5));
        std::env::remove_var(ENV_PUBLISHER_RETRY_DELAY_SECS);
    }

    #[tokio::test]
    async fn retry_delay_falls_back_to_default_on_invalid_env() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(ENV_PUBLISHER_RETRY_DELAY_SECS, "not-a-number");
        assert_eq!(publisher_retry_delay(), Duration::from_secs(2));
        std::env::remove_var(ENV_PUBLISHER_RETRY_DELAY_SECS);
    }

    #[tokio::test]
    async fn retry_helper_returns_second_error_when_retry_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = AtomicUsize::new(0);

        let result: Result<(), WalrusError> = retry_with_delay(Duration::ZERO, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Err(transient_500()) }
        })
        .await;

        // Both attempts ran; final error is propagated.
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(is_transient_publisher_500(&result.unwrap_err()));
    }
}
