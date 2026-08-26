//! # `xyz.taluslabs.social.twitter.get-user-by-id@1`
//!
//! Standard Nexus Tool that retrieves a user by their ID.

use {
    crate::{
        error::{parse_twitter_response, TwitterErrorKind, TwitterErrorResponse, TwitterResult},
        list::models::Includes,
        tweet::{
            models::{ExpansionField, TweetField, UserField},
            TWITTER_API_BASE,
        },
        user::models::{UserData, UserResponse},
    },
    reqwest::Client,
    ::{
        nexus_sdk::{fqn, ToolFqn},
        nexus_toolkit::*,
        schemars::JsonSchema,
        serde::{Deserialize, Serialize},
    },
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Bearer Token for user's Twitter account
    bearer_token: String,

    /// The ID of the User to lookup
    /// Example: "2244994945"
    user_id: String,

    /// A comma separated list of User fields to display
    #[serde(skip_serializing_if = "Option::is_none")]
    user_fields: Option<Vec<UserField>>,

    /// A comma separated list of fields to expand
    #[serde(skip_serializing_if = "Option::is_none")]
    expansions_fields: Option<Vec<ExpansionField>>,

    /// A comma separated list of fields to display
    #[serde(skip_serializing_if = "Option::is_none")]
    tweet_fields: Option<Vec<TweetField>>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        /// The retrieved user
        data: UserData,
        /// Additional entities related to the user
        #[serde(skip_serializing_if = "Option::is_none")]
        includes: Option<Includes>,
    },
    Err {
        /// Error message if the user lookup failed
        reason: String,
        /// Error kind
        kind: TwitterErrorKind,
        /// Status code
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct GetUserById {
    api_base: String,
}

impl NexusTool for GetUserById {
    type Input = Input;
    type Output = Output;

    fn description() -> &'static str {
        "Fetches one X user by identifier."
    }

    async fn new() -> Self {
        Self {
            api_base: TWITTER_API_BASE.to_string() + "/users",
        }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.social.twitter.get-user-by-id@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/get-user-by-id"
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, request: Self::Input) -> Self::Output {
        match self.fetch_user(&request).await {
            Ok(response) => {
                if let Some(user) = response.data {
                    Output::Ok {
                        data: user,
                        includes: response.includes,
                    }
                } else {
                    Output::Err {
                        reason: "No user data found in the response".to_string(),
                        kind: TwitterErrorKind::NotFound,
                        status_code: None,
                    }
                }
            }
            Err(e) => {
                let error_response: TwitterErrorResponse = e.to_error_response();

                Output::Err {
                    reason: error_response.reason,
                    kind: error_response.kind,
                    status_code: error_response.status_code,
                }
            }
        }
    }
}

impl GetUserById {
    /// Fetch user from Twitter API
    async fn fetch_user(&self, request: &Input) -> TwitterResult<UserResponse> {
        let client = Client::new();

        // Construct URL with user ID
        let url = format!("{}/{}", self.api_base, request.user_id);

        // Build request with query parameters
        let mut req_builder = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", request.bearer_token));

        // Add optional query parameters
        if let Some(user_fields) = &request.user_fields {
            let fields: Vec<String> = user_fields
                .iter()
                .map(|f| {
                    serde_json::to_string(f)
                        .unwrap()
                        .replace("\"", "")
                        .to_lowercase()
                })
                .collect();
            req_builder = req_builder.query(&[("user.fields", fields.join(","))]);
        }

        if let Some(expansions_fields) = &request.expansions_fields {
            let fields: Vec<String> = expansions_fields
                .iter()
                .map(|f| {
                    serde_json::to_string(f)
                        .unwrap()
                        .replace("\"", "")
                        .to_lowercase()
                })
                .collect();
            req_builder = req_builder.query(&[("expansions", fields.join(","))]);
        }

        if let Some(tweet_fields) = &request.tweet_fields {
            let fields: Vec<String> = tweet_fields
                .iter()
                .map(|f| {
                    serde_json::to_string(f)
                        .unwrap()
                        .replace("\"", "")
                        .to_lowercase()
                })
                .collect();
            req_builder = req_builder.query(&[("tweet.fields", fields.join(","))]);
        }

        // Send the request and parse the response
        let response = req_builder.send().await?;
        parse_twitter_response::<UserResponse>(response).await
    }
}

#[cfg(test)]
mod tests {
    use {super::*, ::mockito::Server, serde_json::json};

    impl GetUserById {
        fn with_api_base(api_base: &str) -> Self {
            Self {
                api_base: api_base.to_string() + "/users",
            }
        }
    }

    async fn create_server_and_tool() -> (mockito::ServerGuard, GetUserById) {
        let server = Server::new_async().await;
        let tool = GetUserById::with_api_base(&server.url());
        (server, tool)
    }

    fn create_test_input() -> Input {
        Input {
            bearer_token: "test_bearer_token".to_string(),
            user_id: "2244994945".to_string(),
            user_fields: None,
            expansions_fields: None,
            tweet_fields: None,
        }
    }

    #[tokio::test]
    async fn test_get_user_by_id_successful() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/users/2244994945")
            .match_header("Authorization", "Bearer test_bearer_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "data": {
                        "id": "2244994945",
                        "name": "X Dev",
                        "username": "TwitterDev",
                        "protected": false
                    },
                    "includes": {
                        "users": [{
                            "id": "12",
                            "name": "Expanded User",
                            "username": "expanded"
                        }]
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(create_test_input()).await;

        match output {
            Output::Ok { data, includes } => {
                assert_eq!(
                    data.id, "2244994945",
                    "Expected user ID to be '2244994945', got: {}",
                    data.id
                );
                assert_eq!(
                    data.name, "X Dev",
                    "Expected name to be 'X Dev', got: {}",
                    data.name
                );
                assert_eq!(
                    data.username, "TwitterDev",
                    "Expected username to be 'TwitterDev', got: {}",
                    data.username
                );
                assert_eq!(includes.unwrap().users.unwrap()[0].username, "expanded");
            }
            Output::Err {
                reason,
                kind,
                status_code,
            } => panic!(
                "Expected success, got error: {} (kind: {:?}, status_code: {:?})",
                reason, kind, status_code
            ),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_not_found() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/users/2244994945")
            .with_status(404)
            .with_body(
                json!({
                    "errors": [{
                        "message": "User not found",
                        "code": 50
                    }]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(create_test_input()).await;

        match output {
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                // Accept either NotFound or Api error kinds
                if kind == TwitterErrorKind::NotFound || kind == TwitterErrorKind::Api {
                    println!("  Error kind is acceptable: {:?}", kind);
                } else {
                    panic!("Expected error kind NotFound or Api, got: {:?}", kind);
                }

                // Check error message
                assert!(
                    reason.contains("User not found") || reason.contains("Not Found"),
                    "Expected error message to contain 'User not found' or 'Not Found', got: {}",
                    reason
                );

                // Check status code - it might be 404 or None depending on the response structure
                if status_code != Some(404) && status_code.is_some() {
                    panic!("Expected status code 404 or None, got: {:?}", status_code);
                }
            }
            Output::Ok { .. } => panic!("Expected error, got success"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_unauthorized() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/users/2244994945")
            .with_status(401)
            .with_body(
                json!({
                    "errors": [{
                        "message": "Unauthorized",
                        "code": 32
                    }]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(create_test_input()).await;

        match output {
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                // Accept either Auth or Api error kinds
                if kind == TwitterErrorKind::Auth || kind == TwitterErrorKind::Api {
                    println!("  Error kind is acceptable: {:?}", kind);
                } else {
                    panic!("Expected error kind Auth or Api, got: {:?}", kind);
                }

                // Check error message
                assert!(
                    reason.contains("Unauthorized") || reason.contains("Authentication"),
                    "Expected error message to contain 'Unauthorized' or 'Authentication', got: {}",
                    reason
                );

                // Check status code - it might be 401 or None depending on the response structure
                if status_code != Some(401) && status_code.is_some() {
                    panic!("Expected status code 401 or None, got: {:?}", status_code);
                }
            }
            Output::Ok { .. } => panic!("Expected error, got success"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_rate_limit() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/users/2244994945")
            .with_status(429)
            .with_body(
                json!({
                    "errors": [{
                        "message": "Rate limit exceeded",
                        "code": 88
                    }]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(create_test_input()).await;

        match output {
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                // Accept either RateLimit or Api error kinds
                if kind == TwitterErrorKind::RateLimit || kind == TwitterErrorKind::Api {
                    println!("  Error kind is acceptable: {:?}", kind);
                } else {
                    panic!("Expected error kind RateLimit or Api, got: {:?}", kind);
                }

                // Check error message
                assert!(
                    reason.contains("Rate limit") || reason.contains("rate limit"),
                    "Expected error message to contain 'Rate limit' or 'rate limit', got: {}",
                    reason
                );

                // Check status code - it might be 429 or None depending on the response structure
                if status_code != Some(429) && status_code.is_some() {
                    panic!("Expected status code 429 or None, got: {:?}", status_code);
                }
            }
            Output::Ok { .. } => panic!("Expected error, got success"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_partial_response_handling() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/users/2244994945")
            .with_status(200)
            .with_body(
                json!({
                    "data": {
                        "id": "2244994945",
                        "name": "X Dev",
                        "username": "TwitterDev"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let output = tool.invoke(create_test_input()).await;

        match output {
            Output::Ok { data, .. } => {
                assert_eq!(
                    data.id, "2244994945",
                    "Expected user ID to be '2244994945', got: {}",
                    data.id
                );
                assert_eq!(
                    data.name, "X Dev",
                    "Expected name to be 'X Dev', got: {}",
                    data.name
                );
                assert_eq!(
                    data.username, "TwitterDev",
                    "Expected username to be 'TwitterDev', got: {}",
                    data.username
                );
            }
            Output::Err {
                reason,
                kind,
                status_code,
            } => panic!(
                "Expected success, got error: {} (kind: {:?}, status_code: {:?})",
                reason, kind, status_code
            ),
        }

        mock.assert_async().await;
    }
}
