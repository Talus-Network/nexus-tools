use {
    nexus_sdk::walrus::WalrusClient,
    reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION},
};

/// Env var providing a default Walrus publisher URL when input doesn't specify one.
const ENV_PUBLISHER_URL: &str = "WALRUS_PUBLISHER_URL";

/// Env var providing a default Walrus aggregator URL when input doesn't specify one.
const ENV_AGGREGATOR_URL: &str = "WALRUS_AGGREGATOR_URL";

/// GCP metadata server endpoint for fetching ID tokens.
const METADATA_IDENTITY_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity";

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

/// Fetch an OIDC ID token for the given audience from the GCE metadata server.
async fn fetch_id_token(audience: &str) -> Result<String, reqwest::Error> {
    let url = format!("{METADATA_IDENTITY_URL}?audience={audience}&format=full");
    let response = reqwest::Client::new()
        .get(url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await?
        .error_for_status()?;
    response.text().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_run_url_detection() {
        assert!(is_cloud_run_url(
            "https://walrus-publisher-testnet-abc-uc.a.run.app"
        ));
        assert!(is_cloud_run_url("https://foo.run.app"));
        assert!(!is_cloud_run_url("https://publisher.walrus-testnet.walrus.space"));
        assert!(!is_cloud_run_url("http://localhost:8080"));
    }
}
