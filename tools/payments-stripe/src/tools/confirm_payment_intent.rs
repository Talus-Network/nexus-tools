//! # `xyz.taluslabs.payments.stripe.confirm-payment-intent@1`

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
    pub idempotency_key: Option<String>,
    pub payment_intent_id: String,
    pub payment_method: Option<String>,
    pub return_url: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_action_type: Option<String>,
    },
    Err {
        reason: String,
        kind: StripeErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct ConfirmPaymentIntent {
    client: StripeClient,
}

#[derive(Deserialize)]
struct ConfirmResponse {
    id: String,
    status: String,
    next_action: Option<NextAction>,
}

#[derive(Deserialize)]
struct NextAction {
    #[serde(rename = "type")]
    action_type: String,
}

impl NexusTool for ConfirmPaymentIntent {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self {
            client: StripeClient::new(None),
        }
    }

    fn fqn() -> ToolFqn {
        fqn!("xyz.taluslabs.payments.stripe.confirm-payment-intent@1")
    }

    fn path() -> &'static str {
        "/confirm-payment-intent"
    }

    fn description() -> &'static str {
        "Confirms a Stripe PaymentIntent."
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

        let mut form: Vec<(&str, String)> = Vec::new();
        if let Some(pm) = &input.payment_method {
            form.push(("payment_method", pm.clone()));
        }
        if let Some(ru) = &input.return_url {
            form.push(("return_url", ru.clone()));
        }

        let endpoint = format!("v1/payment_intents/{}/confirm", input.payment_intent_id);
        let mut client = self.client.clone().with_auth(&input.api_key);
        if let Some(k) = &input.idempotency_key {
            client = client.with_idempotency(k);
        }

        match client
            .post_form::<ConfirmResponse, _>(&endpoint, &form)
            .await
        {
            Ok(r) => Output::Ok {
                id: r.id,
                status: r.status,
                next_action_type: r.next_action.map(|na| na.action_type),
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

    async fn create_server_and_tool() -> (mockito::ServerGuard, ConfirmPaymentIntent) {
        let server = Server::new_async().await;
        let client = StripeClient::new(Some(&server.url()));
        (server, ConfirmPaymentIntent { client })
    }

    fn test_input() -> Input {
        Input {
            api_key: "sk_test_FAKE".to_string(),
            idempotency_key: None,
            payment_intent_id: "pi_test_123".to_string(),
            payment_method: Some("pm_card_visa".to_string()),
            return_url: None,
        }
    }

    #[tokio::test]
    async fn test_confirm_success_no_next_action() {
        let (mut server, tool) = create_server_and_tool().await;
        let _mock = server
            .mock("POST", "/v1/payment_intents/pi_test_123/confirm")
            .with_status(200)
            .with_body(
                json!({
                    "id": "pi_test_123",
                    "status": "succeeded",
                    "next_action": null
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool.invoke(test_input()).await;
        match result {
            Output::Ok {
                status,
                next_action_type,
                ..
            } => {
                assert_eq!(status, "succeeded");
                assert_eq!(next_action_type, None);
            }
            Output::Err { reason, .. } => panic!("expected Ok, got Err: {reason}"),
        }
    }

    #[tokio::test]
    async fn test_confirm_requires_action() {
        let (mut server, tool) = create_server_and_tool().await;
        let _mock = server
            .mock("POST", "/v1/payment_intents/pi_test_123/confirm")
            .with_status(200)
            .with_body(
                json!({
                    "id": "pi_test_123",
                    "status": "requires_action",
                    "next_action": { "type": "redirect_to_url" }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool.invoke(test_input()).await;
        match result {
            Output::Ok {
                next_action_type, ..
            } => {
                assert_eq!(next_action_type.as_deref(), Some("redirect_to_url"));
            }
            Output::Err { reason, .. } => panic!("expected Ok, got Err: {reason}"),
        }
    }

    #[tokio::test]
    async fn test_confirm_empty_id() {
        let (_, tool) = create_server_and_tool().await;
        let mut input = test_input();
        input.payment_intent_id = "".to_string();
        let result = tool.invoke(input).await;
        assert!(matches!(result, Output::Err { .. }));
    }
}
