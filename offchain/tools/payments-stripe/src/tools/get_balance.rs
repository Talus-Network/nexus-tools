//! # `xyz.taluslabs.payments.stripe.get-balance@1`

use {
    crate::{error::StripeErrorKind, stripe_client::StripeClient, tools::models::BalanceAmount},
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub api_key: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        available: Vec<BalanceAmount>,
        pending: Vec<BalanceAmount>,
    },
    Err {
        reason: String,
        kind: StripeErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct GetBalance {
    client: StripeClient,
}

#[derive(Deserialize)]
struct BalanceResponse {
    available: Vec<BalanceAmount>,
    pending: Vec<BalanceAmount>,
}

impl NexusTool for GetBalance {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self {
            client: StripeClient::new(None),
        }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.payments.stripe.get-balance@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/get-balance"
    }

    fn description() -> &'static str {
        "Retrieves the Stripe platform balance."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        let client = self.client.clone().with_auth(&input.api_key);
        match client.get::<BalanceResponse>("v1/balance").await {
            Ok(b) => Output::Ok {
                available: b.available,
                pending: b.pending,
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

    async fn create_server_and_tool() -> (mockito::ServerGuard, GetBalance) {
        let server = Server::new_async().await;
        let client = StripeClient::new(Some(&server.url()));
        (server, GetBalance { client })
    }

    #[tokio::test]
    async fn test_get_balance_success() {
        let (mut server, tool) = create_server_and_tool().await;
        let _mock = server
            .mock("GET", "/v1/balance")
            .with_status(200)
            .with_body(
                json!({
                    "available": [{ "amount": 1000, "currency": "usd" }],
                    "pending":   [{ "amount":  500, "currency": "usd" }]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool
            .invoke(Input {
                api_key: "sk_test_FAKE".to_string(),
            })
            .await;
        match result {
            Output::Ok { available, pending } => {
                assert_eq!(available.len(), 1);
                assert_eq!(available[0].amount, 1000);
                assert_eq!(pending[0].amount, 500);
            }
            Output::Err { reason, .. } => panic!("expected Ok, got Err: {reason}"),
        }
    }
}
