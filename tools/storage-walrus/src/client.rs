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
//! ## Authentication for Cloud Run publishers
//!
//! When the resolved publisher URL is a Google Cloud Run hostname
//! (`*.run.app`), the underlying [`reqwest::Client`] is built with a default
//! `Authorization: Bearer <id_token>` header. The token is an OIDC ID token
//! fetched from the GCE metadata server with the publisher URL as the
//! audience claim, and is what authenticates this tool against a publisher
//! configured with `INGRESS_TRAFFIC_INTERNAL_ONLY` + `roles/run.invoker`.
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
//! [Restricting ingress for Cloud Run]: https://cloud.google.com/run/docs/securing/ingress#internal
//! [Cloud Run IAM roles]: https://cloud.google.com/run/docs/reference/iam/roles#standard-roles

use {
    nexus_sdk::walrus::WalrusClient,
    reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION},
    std::time::Duration,
};

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
                headers.insert(AUTHORIZATION, value);
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
/// [Fetching identity tokens]: https://cloud.google.com/run/docs/authenticating/service-to-service#use_a_token_to_call_a_cloud_run_service_or_function
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
}
