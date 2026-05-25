//! # `xyz.taluslabs.payments.stripe.create-customer@1`

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
    pub email: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        created: i64,
    },
    Err {
        reason: String,
        kind: StripeErrorKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct CreateCustomer {
    client: StripeClient,
}

#[derive(Deserialize)]
struct CustomerResponse {
    id: String,
    email: Option<String>,
    created: i64,
}

impl NexusTool for CreateCustomer {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self {
            client: StripeClient::new(None),
        }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.payments.stripe.create-customer@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/create-customer"
    }

    fn description() -> &'static str {
        "Creates a Stripe Customer."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        let mut form: Vec<(&str, String)> = Vec::new();
        if let Some(e) = &input.email {
            form.push(("email", e.clone()));
        }
        if let Some(n) = &input.name {
            form.push(("name", n.clone()));
        }
        if let Some(d) = &input.description {
            form.push(("description", d.clone()));
        }

        let mut client = self.client.clone().with_auth(&input.api_key);
        if let Some(k) = &input.idempotency_key {
            client = client.with_idempotency(k);
        }

        match client
            .post_form::<CustomerResponse, _>("v1/customers", &form)
            .await
        {
            Ok(c) => Output::Ok {
                id: c.id,
                email: c.email,
                created: c.created,
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

    async fn create_server_and_tool() -> (mockito::ServerGuard, CreateCustomer) {
        let server = Server::new_async().await;
        let client = StripeClient::new(Some(&server.url()));
        (server, CreateCustomer { client })
    }

    #[tokio::test]
    async fn test_create_customer_success() {
        let (mut server, tool) = create_server_and_tool().await;
        let _mock = server
            .mock("POST", "/v1/customers")
            .with_status(200)
            .with_body(
                json!({
                    "id": "cus_test_abc",
                    "email": "test@example.com",
                    "created": 1700000000
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool
            .invoke(Input {
                api_key: "sk_test_FAKE".to_string(),
                idempotency_key: None,
                email: Some("test@example.com".to_string()),
                name: Some("Test User".to_string()),
                description: None,
            })
            .await;
        match result {
            Output::Ok { id, email, created } => {
                assert_eq!(id, "cus_test_abc");
                assert_eq!(email.as_deref(), Some("test@example.com"));
                assert_eq!(created, 1700000000);
            }
            Output::Err { reason, .. } => panic!("expected Ok, got Err: {reason}"),
        }
    }

    #[tokio::test]
    async fn test_create_customer_auth_error() {
        let (mut server, tool) = create_server_and_tool().await;
        let _mock = server
            .mock("POST", "/v1/customers")
            .with_status(401)
            .with_body(
                json!({
                    "error": {
                        "type": "authentication_error",
                        "message": "Invalid API Key provided"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result = tool
            .invoke(Input {
                api_key: "sk_test_BAD".to_string(),
                idempotency_key: None,
                email: Some("test@example.com".to_string()),
                name: None,
                description: None,
            })
            .await;
        assert!(matches!(
            result,
            Output::Err {
                kind: StripeErrorKind::Unauthorized,
                ..
            }
        ));
    }
}
