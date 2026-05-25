//! Stateless Stripe HTTP client.
//!
//! Credential model (audit checks C7, C9, C10):
//! - This client holds NO Stripe credentials between requests.
//! - The `api_key` is attached per-call via `.with_auth(&input.api_key)`.
//! - The struct's only persistent field is an `Arc<reqwest::Client>` —
//!   a stateless connection pool. `new()` is cheap and idempotent.
//! - No `std::env::var` reads; no on-disk reads.

use {
    crate::{
        error::{try_parse_api_error, StripeErrorKind, StripeErrorResponse},
        tools::STRIPE_API_BASE,
    },
    reqwest::{Client, RequestBuilder},
    serde::{de::DeserializeOwned, Serialize},
    std::sync::Arc,
};

#[derive(Clone)]
pub struct StripeClient {
    client: Arc<Client>,
    base_url: String,
    bearer: Option<String>,
    idempotency_key: Option<String>,
}

impl StripeClient {
    pub fn new(base_url: Option<&str>) -> Self {
        let base_url = base_url.unwrap_or(STRIPE_API_BASE).to_string();
        let client = Client::builder()
            .user_agent("nexus-sdk-payments-stripe/1.0")
            .build()
            .expect("Failed to create HTTP client");
        Self {
            client: Arc::new(client),
            base_url,
            bearer: None,
            idempotency_key: None,
        }
    }

    /// Attach the per-request Stripe secret key. Returns a new builder
    /// so the caller's base client never sees the credential.
    #[must_use]
    pub fn with_auth(mut self, bearer: &str) -> Self {
        self.bearer = Some(bearer.to_string());
        self
    }

    /// Attach an `Idempotency-Key` header.
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
            req = req.bearer_auth(bearer);
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
