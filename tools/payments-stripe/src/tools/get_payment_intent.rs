//! # `xyz.taluslabs.payments.stripe.get-payment-intent@1`

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
    pub api_key: String,
    pub payment_intent_id: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        id: String,
        status: String,
        amount: i64,
        currency: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
    },
    Err {
        reason: String,
        kind: StripeErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct GetPaymentIntent {
    client: StripeClient,
}

#[derive(Deserialize)]
struct StripePaymentIntent {
    id: String,
    status: String,
    amount: i64,
    currency: String,
    client_secret: Option<String>,
}

impl NexusTool for GetPaymentIntent {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self {
            client: StripeClient::new(None),
        }
    }

    fn fqn() -> ToolFqn {
        fqn!("xyz.taluslabs.payments.stripe.get-payment-intent@1")
    }

    fn path() -> &'static str {
        "/get-payment-intent"
    }

    fn description() -> &'static str {
        "Retrieves a Stripe PaymentIntent by id."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        if input.payment_intent_id.trim().is_empty() {
            return Output::Err {
                reason: "payment_intent_id must not be empty".to_string(),
                kind: StripeErrorKind::InvalidRequest,
                status_code: None,
            };
        }

        let endpoint = format!("v1/payment_intents/{}", input.payment_intent_id);
        let client = self.client.clone().with_auth(&input.api_key);

        match client.get::<StripePaymentIntent>(&endpoint).await {
            Ok(pi) => Output::Ok {
                id: pi.id,
                status: pi.status,
                amount: pi.amount,
                currency: pi.currency,
                client_secret: pi.client_secret,
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

    async fn create_server_and_tool() -> (mockito::ServerGuard, GetPaymentIntent) {
        let server = Server::new_async().await;
        let client = StripeClient::new(Some(&server.url()));
        (server, GetPaymentIntent { client })
    }

    #[tokio::test]
    async fn test_get_payment_intent_success() {
        let (mut server, tool) = create_server_and_tool().await;
        let mock = server
            .mock("GET", "/v1/payment_intents/pi_test_123")
            .match_header("authorization", "Bearer sk_test_FAKE")
            .with_status(200)
            .with_body(
                json!({
                    "id": "pi_test_123",
                    "status": "succeeded",
                    "amount": 1500,
                    "currency": "usd",
                    "client_secret": null
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool
            .invoke(Input {
                api_key: "sk_test_FAKE".to_string(),
                payment_intent_id: "pi_test_123".to_string(),
            })
            .await;
        match result {
            Output::Ok { id, status, .. } => {
                assert_eq!(id, "pi_test_123");
                assert_eq!(status, "succeeded");
            }
            Output::Err { reason, .. } => panic!("expected Ok, got Err: {reason}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_payment_intent_not_found() {
        let (mut server, tool) = create_server_and_tool().await;
        let _mock = server
            .mock("GET", "/v1/payment_intents/pi_missing")
            .with_status(404)
            .with_body(
                json!({
                    "error": {
                        "type": "invalid_request_error",
                        "message": "No such payment_intent: pi_missing"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool
            .invoke(Input {
                api_key: "sk_test_FAKE".to_string(),
                payment_intent_id: "pi_missing".to_string(),
            })
            .await;
        assert!(matches!(
            result,
            Output::Err {
                kind: StripeErrorKind::InvalidRequest,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_get_payment_intent_empty_id() {
        let (_, tool) = create_server_and_tool().await;
        let result = tool
            .invoke(Input {
                api_key: "sk_test_FAKE".to_string(),
                payment_intent_id: "".to_string(),
            })
            .await;
        assert!(matches!(result, Output::Err { .. }));
    }
}
