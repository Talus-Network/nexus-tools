//! # `xyz.taluslabs.payments.stripe.create-payment-intent@1`
//!
//! Creates a Stripe PaymentIntent.
//!
//! Stateless: holds only a stateless connection pool. Credential
//! (`api_key`) is supplied per request via `Input`.

use {
    crate::{error::StripeErrorKind, stripe_client::StripeClient},
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Stripe secret key. NEVER put this in DAG `default_values` — the
    /// Leader supplies it at execution time.
    pub api_key: String,
    /// Optional Idempotency-Key for safe retry.
    pub idempotency_key: Option<String>,
    /// Amount in the smallest currency unit (cents for USD).
    pub amount: i64,
    /// ISO-4217 lowercase currency code.
    pub currency: String,
    /// Existing Stripe customer id (`cus_…`).
    pub customer: Option<String>,
    /// Free-form description shown on the dashboard.
    pub description: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        id: String,
        client_secret: String,
        status: String,
        amount: i64,
        currency: String,
    },
    Err {
        reason: String,
        kind: StripeErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct CreatePaymentIntent {
    client: StripeClient,
}

#[derive(Deserialize)]
struct StripePaymentIntent {
    id: String,
    client_secret: String,
    status: String,
    amount: i64,
    currency: String,
}

impl NexusTool for CreatePaymentIntent {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self {
            client: StripeClient::new(None),
        }
    }

    fn fqn() -> ToolFqn {
        fqn!("xyz.taluslabs.payments.stripe.create-payment-intent@1")
    }

    fn path() -> &'static str {
        "/create-payment-intent"
    }

    fn description() -> &'static str {
        "Creates a Stripe PaymentIntent."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        if input.amount <= 0 {
            return Output::Err {
                reason: "amount must be positive (and in smallest currency unit, e.g. cents)"
                    .to_string(),
                kind: StripeErrorKind::InvalidRequest,
                status_code: None,
            };
        }
        if input.currency.trim().is_empty() {
            return Output::Err {
                reason: "currency must be a non-empty ISO-4217 code".to_string(),
                kind: StripeErrorKind::InvalidRequest,
                status_code: None,
            };
        }

        let mut form: Vec<(&str, String)> = vec![
            ("amount", input.amount.to_string()),
            ("currency", input.currency.to_lowercase()),
        ];
        if let Some(c) = &input.customer {
            form.push(("customer", c.clone()));
        }
        if let Some(d) = &input.description {
            form.push(("description", d.clone()));
        }

        let mut client = self.client.clone().with_auth(&input.api_key);
        if let Some(k) = &input.idempotency_key {
            client = client.with_idempotency(k);
        }

        match client
            .post_form::<StripePaymentIntent, _>("v1/payment_intents", &form)
            .await
        {
            Ok(pi) => Output::Ok {
                id: pi.id,
                client_secret: pi.client_secret,
                status: pi.status,
                amount: pi.amount,
                currency: pi.currency,
            },
            Err(e) => Output::Err {
                reason: e.reason,
                kind: e.kind,
                status_code: e.status_code,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        ::{mockito::Server, serde_json::json},
    };

    async fn create_server_and_tool() -> (mockito::ServerGuard, CreatePaymentIntent) {
        let server = Server::new_async().await;
        let client = StripeClient::new(Some(&server.url()));
        (server, CreatePaymentIntent { client })
    }

    fn test_input() -> Input {
        Input {
            api_key: "sk_test_FAKE_FOR_TESTS_ONLY".to_string(),
            idempotency_key: Some("test-idempotency-key-001".to_string()),
            amount: 2000,
            currency: "usd".to_string(),
            customer: None,
            description: Some("Test payment".to_string()),
        }
    }

    #[tokio::test]
    async fn test_create_payment_intent_success() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("POST", "/v1/payment_intents")
            .match_header("authorization", "Bearer sk_test_FAKE_FOR_TESTS_ONLY")
            .match_header("idempotency-key", "test-idempotency-key-001")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "id": "pi_test_123",
                    "client_secret": "pi_test_123_secret_abc",
                    "status": "requires_payment_method",
                    "amount": 2000,
                    "currency": "usd"
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool.invoke(test_input()).await;
        match result {
            Output::Ok {
                id,
                client_secret,
                status,
                amount,
                currency,
            } => {
                assert_eq!(id, "pi_test_123");
                assert_eq!(client_secret, "pi_test_123_secret_abc");
                assert_eq!(status, "requires_payment_method");
                assert_eq!(amount, 2000);
                assert_eq!(currency, "usd");
            }
            Output::Err { reason, .. } => panic!("expected Ok, got Err: {reason}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_create_payment_intent_card_error() {
        let (mut server, tool) = create_server_and_tool().await;

        let _mock = server
            .mock("POST", "/v1/payment_intents")
            .with_status(402)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "error": {
                        "type": "card_error",
                        "code": "card_declined",
                        "message": "Your card was declined.",
                        "param": "card"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool.invoke(test_input()).await;
        match result {
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                assert_eq!(kind, StripeErrorKind::CardError);
                assert!(reason.contains("Your card was declined"));
                assert_eq!(status_code, Some(402));
            }
            Output::Ok { .. } => panic!("expected Err, got Ok"),
        }
    }

    #[tokio::test]
    async fn test_create_payment_intent_idempotency_error() {
        let (mut server, tool) = create_server_and_tool().await;

        let _mock = server
            .mock("POST", "/v1/payment_intents")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "error": {
                        "type": "idempotency_error",
                        "message": "Keys for idempotent requests can only be used with the same parameters they were first used with."
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool.invoke(test_input()).await;
        assert!(matches!(
            result,
            Output::Err {
                kind: StripeErrorKind::IdempotencyError,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_create_payment_intent_validation_zero_amount() {
        let (_, tool) = create_server_and_tool().await;
        let mut input = test_input();
        input.amount = 0;
        let result = tool.invoke(input).await;
        assert!(matches!(
            result,
            Output::Err {
                kind: StripeErrorKind::InvalidRequest,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_create_payment_intent_validation_empty_currency() {
        let (_, tool) = create_server_and_tool().await;
        let mut input = test_input();
        input.currency = "".to_string();
        let result = tool.invoke(input).await;
        assert!(matches!(
            result,
            Output::Err {
                kind: StripeErrorKind::InvalidRequest,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_create_payment_intent_rate_limit() {
        let (mut server, tool) = create_server_and_tool().await;

        let _mock = server
            .mock("POST", "/v1/payment_intents")
            .with_status(429)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "error": {
                        "type": "rate_limit_error",
                        "message": "Too many requests"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool.invoke(test_input()).await;
        assert!(matches!(
            result,
            Output::Err {
                kind: StripeErrorKind::RateLimitExceeded,
                ..
            }
        ));
    }
}
