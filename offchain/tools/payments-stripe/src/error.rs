//! Stripe-specific error envelope + kind mapping.
//!
//! Stripe returns errors as `{"error":{"type","code","message","param"}}`.
//! We map `type` to a typed kind enum so DAG authors can branch on the
//! failure class without parsing `reason`.

use {
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    thiserror::Error,
};

/// Machine-readable error kinds for Stripe operations.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StripeErrorKind {
    /// Stripe `invalid_request_error` — malformed request.
    InvalidRequest,
    /// Stripe `card_error` — card was declined.
    CardError,
    /// Stripe `validation_error` — request parameters invalid.
    ValidationError,
    /// Stripe `idempotency_error` — idempotency key reused with different params.
    IdempotencyError,
    /// Stripe `authentication_error` — missing or wrong API key.
    Unauthorized,
    /// HTTP 402 payment required.
    PaymentRequired,
    /// HTTP 403.
    Forbidden,
    /// Resource not found.
    NotFound,
    /// HTTP 408 / network timeout.
    TimedOut,
    /// HTTP 409 — conflict.
    Conflict,
    /// Stripe `rate_limit_error` / HTTP 429.
    RateLimitExceeded,
    /// Stripe `api_error` / HTTP 5xx.
    InternalServerError,
    /// HTTP 502 bad gateway.
    BadGateway,
    /// HTTP 503 service unavailable.
    ServiceUnavailable,
    /// Connection-level failure.
    NetworkConnectionFailed,
    /// Failed to parse the upstream response.
    Parse,
    /// Anything we don't have a typed mapping for.
    Unknown,
}

/// Public error surface returned to the DAG via `Output::Err`.
#[derive(Debug, Serialize, Deserialize)]
pub struct StripeErrorResponse {
    pub reason: String,
    pub kind: StripeErrorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum StripeError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Response parsing error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("Stripe API error: {0}")]
    ApiError(String),
}

impl StripeErrorKind {
    pub fn from_status_code(status_code: u16) -> Self {
        match status_code {
            400 => Self::InvalidRequest,
            401 => Self::Unauthorized,
            402 => Self::PaymentRequired,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            408 => Self::TimedOut,
            409 => Self::Conflict,
            429 => Self::RateLimitExceeded,
            500 => Self::InternalServerError,
            502 => Self::BadGateway,
            503 => Self::ServiceUnavailable,
            504 => Self::TimedOut,
            _ => Self::Unknown,
        }
    }

    pub fn from_network_error(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::TimedOut
        } else {
            // is_connect / is_request / anything else all map here.
            Self::NetworkConnectionFailed
        }
    }

    /// Map Stripe's `error.type` to our kind. Cover every documented
    /// Stripe error class (audit check H10).
    pub fn from_api_error_type(error_type: &str) -> Self {
        match error_type {
            "invalid_request_error" => Self::InvalidRequest,
            "card_error" => Self::CardError,
            "validation_error" => Self::ValidationError,
            "idempotency_error" => Self::IdempotencyError,
            "rate_limit_error" => Self::RateLimitExceeded,
            "authentication_error" => Self::Unauthorized,
            "api_error" | "api_connection_error" => Self::InternalServerError,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Deserialize)]
struct StripeEnvelope {
    error: StripeErrorBody,
}

#[derive(Debug, Deserialize)]
struct StripeErrorBody {
    #[serde(rename = "type")]
    error_type: Option<String>,
    code: Option<String>,
    message: Option<String>,
    param: Option<String>,
}

/// Best-effort parse of a non-2xx response body into a typed error.
/// Returns `None` if the body doesn't match Stripe's envelope; the
/// caller falls back to status-code-only mapping.
pub fn try_parse_api_error(body: &str, status_code: u16) -> Option<StripeErrorResponse> {
    let env: StripeEnvelope = serde_json::from_str(body).ok()?;
    let kind = env
        .error
        .error_type
        .as_deref()
        .map(StripeErrorKind::from_api_error_type)
        .unwrap_or_else(|| StripeErrorKind::from_status_code(status_code));

    let reason = match (env.error.message, env.error.code, env.error.param) {
        (Some(m), _, Some(p)) => format!("{} (param: {})", m, p),
        (Some(m), _, None) => m,
        (None, Some(c), _) => format!("Stripe error code: {}", c),
        _ => format!("Stripe API error ({})", status_code),
    };

    Some(StripeErrorResponse {
        reason,
        kind,
        status_code: Some(status_code),
    })
}
