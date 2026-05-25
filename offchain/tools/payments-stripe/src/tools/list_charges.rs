//! # `xyz.taluslabs.payments.stripe.list-charges@1`

use {
    crate::{error::StripeErrorKind, stripe_client::StripeClient, tools::models::ChargeSummary},
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub api_key: String,
    pub limit: Option<i64>,
    pub customer: Option<String>,
    pub starting_after: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        charges: Vec<ChargeSummary>,
        has_more: bool,
    },
    Err {
        reason: String,
        kind: StripeErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct ListCharges {
    client: StripeClient,
}

#[derive(Deserialize)]
struct ListResponse {
    data: Vec<ChargeSummary>,
    has_more: bool,
}

impl NexusTool for ListCharges {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self {
            client: StripeClient::new(None),
        }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.payments.stripe.list-charges@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/list-charges"
    }

    fn description() -> &'static str {
        "Lists Stripe charges with optional filtering and pagination."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        if let Some(l) = input.limit {
            if !(1..=100).contains(&l) {
                return Output::Err {
                    reason: "limit must be in 1..=100".to_string(),
                    kind: StripeErrorKind::InvalidRequest,
                    status_code: None,
                };
            }
        }

        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(l) = input.limit {
            query.push(("limit", l.to_string()));
        }
        if let Some(c) = &input.customer {
            query.push(("customer", c.clone()));
        }
        if let Some(s) = &input.starting_after {
            query.push(("starting_after", s.clone()));
        }

        let qs = serde_urlencoded_minimal(&query);
        let endpoint = if qs.is_empty() {
            "v1/charges".to_string()
        } else {
            format!("v1/charges?{qs}")
        };

        let client = self.client.clone().with_auth(&input.api_key);
        match client.get::<ListResponse>(&endpoint).await {
            Ok(r) => Output::Ok {
                charges: r.data,
                has_more: r.has_more,
            },
            Err(e) => Output::Err {
                reason: e.reason,
                kind: e.kind,
                status_code: e.status_code,
            },
        }
    }
}

fn serde_urlencoded_minimal(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v),))
        .collect::<Vec<_>>()
        .join("&")
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        ::{mockito::Server, serde_json::json},
    };

    async fn create_server_and_tool() -> (mockito::ServerGuard, ListCharges) {
        let server = Server::new_async().await;
        let client = StripeClient::new(Some(&server.url()));
        (server, ListCharges { client })
    }

    #[tokio::test]
    async fn test_list_charges_success() {
        let (mut server, tool) = create_server_and_tool().await;
        let _mock = server
            .mock("GET", "/v1/charges?limit=2")
            .with_status(200)
            .with_body(
                json!({
                    "data": [
                        { "id": "ch_1", "amount": 1000, "currency": "usd", "status": "succeeded", "customer": "cus_a" },
                        { "id": "ch_2", "amount": 2500, "currency": "usd", "status": "succeeded" }
                    ],
                    "has_more": true
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool
            .invoke(Input {
                api_key: "sk_test_FAKE".to_string(),
                limit: Some(2),
                customer: None,
                starting_after: None,
            })
            .await;
        match result {
            Output::Ok { charges, has_more } => {
                assert_eq!(charges.len(), 2);
                assert!(has_more);
                assert_eq!(charges[0].id, "ch_1");
                assert_eq!(charges[1].customer, None);
            }
            Output::Err { reason, .. } => panic!("expected Ok, got Err: {reason}"),
        }
    }

    #[tokio::test]
    async fn test_list_charges_limit_out_of_range() {
        let (_, tool) = create_server_and_tool().await;
        let result = tool
            .invoke(Input {
                api_key: "sk_test_FAKE".to_string(),
                limit: Some(150),
                customer: None,
                starting_after: None,
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
}
