//! # `xyz.taluslabs.social.twitter.get-tweet@1`
//!
//! Standard Nexus Tool that retrieves a single tweet from the Twitter API.

use {
    crate::{
        error::{parse_twitter_response, TwitterErrorKind, TwitterErrorResponse, TwitterResult},
        tweet::{
            models::{GetTweetResponse, Includes, Meta, Tweet},
            TWITTER_API_BASE,
        },
    },
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    reqwest::Client,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Bearer Token for user's Twitter account
    bearer_token: String,
    /// Tweet ID to retrieve
    tweet_id: String,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        /// The retrieved tweet
        data: Tweet,
        /// Additional entities related to the tweet:
        /// - media: Images and videos
        /// - places: Geographic locations
        /// - polls: Twitter polls
        /// - topics: Related topics
        /// - tweets: Referenced tweets
        /// - users: Mentioned users
        #[serde(skip_serializing_if = "Option::is_none")]
        includes: Option<Includes>,
        /// Metadata about the tweet request:
        /// - newest_id: Newest tweet ID in a collection
        /// - next_token: Pagination token for next results
        /// - oldest_id: Oldest tweet ID in a collection
        /// - previous_token: Pagination token for previous results
        /// - result_count: Number of results returned
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Meta>,
    },
    Err {
        /// Detailed error message
        reason: String,
        /// Type of error (network, server, auth, etc.)
        kind: TwitterErrorKind,
        /// HTTP status code if available
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct GetTweet {
    api_base: String,
}

impl NexusTool for GetTweet {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self {
            api_base: TWITTER_API_BASE.to_string() + "/tweets",
        }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.social.twitter.get-tweet@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/get-tweet"
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, request: Self::Input) -> Self::Output {
        let client = Client::new();

        // Add authentication header
        let url = format!("{}/{}", self.api_base, request.tweet_id);

        // Make the request
        match self.fetch_tweet(&client, &url, &request.bearer_token).await {
            Ok(response) => {
                if let Some(tweet) = response.data {
                    Output::Ok {
                        data: tweet,
                        includes: response.includes,
                        meta: response.meta,
                    }
                } else {
                    // Return an error if there's no tweet data
                    let error_response = TwitterErrorResponse {
                        reason: "No tweet data found in the response".to_string(),
                        kind: TwitterErrorKind::NotFound,
                        status_code: None,
                    };

                    Output::Err {
                        reason: error_response.reason,
                        kind: error_response.kind,
                        status_code: error_response.status_code,
                    }
                }
            }
            Err(e) => {
                // Use the centralized error conversion
                let error_response = e.to_error_response();

                Output::Err {
                    reason: error_response.reason,
                    kind: error_response.kind,
                    status_code: error_response.status_code,
                }
            }
        }
    }
}

impl GetTweet {
    /// Fetch tweet from Twitter API
    async fn fetch_tweet(
        &self,
        client: &Client,
        url: &str,
        bearer_token: &str,
    ) -> TwitterResult<GetTweetResponse> {
        let response = client
            .get(url)
            .header("Authorization", format!("Bearer {}", bearer_token))
            .send()
            .await?;

        parse_twitter_response::<GetTweetResponse>(response).await
    }
}

#[cfg(test)]
mod tests {
    use {super::*, ::mockito::Server, serde_json::json};

    impl GetTweet {
        fn with_api_base(api_base: &str) -> Self {
            Self {
                api_base: api_base.to_string(),
            }
        }
    }

    async fn create_server_and_tool() -> (mockito::ServerGuard, GetTweet) {
        let server = Server::new_async().await;
        let tool = GetTweet::with_api_base(&(server.url() + "/tweets"));
        (server, tool)
    }

    fn create_test_input() -> Input {
        Input {
            bearer_token: "test_bearer_token".to_string(),
            tweet_id: "test_tweet_id".to_string(),
        }
    }

    #[tokio::test]
    async fn test_get_tweet_successful() {
        // Create server and tool
        let (mut server, tool) = create_server_and_tool().await;

        // Set up mock response
        let mock = server
            .mock("GET", "/tweets/test_tweet_id")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                   "data": {
                        "author_id": "2244994945",
                        "created_at": "Wed Jan 06 18:40:40 +0000 2021",
                        "id": "1346889436626259968",
                        "text": "Learn how to use the user Tweet timeline and user mention timeline endpoints in the X API v2 to explore Tweet\\u2026 https:\\/\\/t.co\\/56a0vZUx7i",
                        "username": "XDevelopers"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        // Test the tweet request
        let output = tool.invoke(create_test_input()).await;

        // Verify the response
        match output {
            Output::Ok { data, .. } => {
                assert_eq!(data.id, "1346889436626259968");
                assert_eq!(data.text, "Learn how to use the user Tweet timeline and user mention timeline endpoints in the X API v2 to explore Tweet\\u2026 https:\\/\\/t.co\\/56a0vZUx7i");
                assert_eq!(data.author_id, Some("2244994945".to_string()));
                assert!(data.created_at.is_some());
                assert_eq!(data.username, Some("XDevelopers".to_string()));
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

        // Verify that the mock was called
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_tweet_not_found() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/tweets/test_tweet_id")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "errors": [
                        {
                            "value": "test_tweet_id",
                            "detail": "Could not find tweet with id: [test_tweet_id].",
                            "title": "Not Found Error",
                            "type": "https://api.twitter.com/2/problems/resource-not-found"
                        }
                    ]
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
                // Check error type
                assert_eq!(
                    kind,
                    TwitterErrorKind::NotFound,
                    "Expected error kind NotFound, got: {:?}",
                    kind
                );

                // Check error message
                assert!(
                    reason.contains("Not Found Error"),
                    "Expected error message to contain 'Not Found Error', got: {}",
                    reason
                );
                assert!(
                    reason.contains("Could not find tweet with id"),
                    "Expected error message to contain tweet ID details, got: {}",
                    reason
                );

                // Check status code
                assert_eq!(
                    status_code,
                    Some(404),
                    "Expected status code 404, got: {:?}",
                    status_code
                );
            }
            Output::Ok { .. } => panic!("Expected error, got success"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_tweet_unauthorized() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/tweets/test_tweet_id")
            .with_status(401)
            .with_header("content-type", "application/json")
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
                // Check error type
                assert_eq!(
                    kind,
                    TwitterErrorKind::Auth,
                    "Expected error kind Auth, got: {:?}",
                    kind
                );

                // Check error message
                assert!(
                    reason.contains("Unauthorized"),
                    "Expected error message to contain 'Unauthorized', got: {}",
                    reason
                );

                // Check status code
                assert_eq!(
                    status_code,
                    Some(401),
                    "Expected status code 401, got: {:?}",
                    status_code
                );
            }
            Output::Ok { .. } => panic!("Expected error, got success"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_tweet_rate_limit() {
        let (mut server, tool) = create_server_and_tool().await;

        let mock = server
            .mock("GET", "/tweets/test_tweet_id")
            .match_header("Authorization", "Bearer test_bearer_token")
            .match_query(mockito::Matcher::Any)
            .with_status(429)
            .with_header("content-type", "application/json")
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
                println!("Kind: {:?}", kind);
                println!("reason: {:?}", reason);
                // Check error type
                assert_eq!(
                    kind,
                    TwitterErrorKind::RateLimit,
                    "Expected error kind RateLimit, got: {:?}",
                    kind
                );

                // Check error message
                assert!(
                    reason.contains("Rate limit exceeded"),
                    "Expected error message to contain 'Rate limit exceeded', got: {}",
                    reason
                );

                // Check status code
                assert_eq!(
                    status_code,
                    Some(429),
                    "Expected status code 429, got: {:?}",
                    status_code
                );
            }
            Output::Ok { .. } => panic!("Expected error, got success"),
        }

        mock.assert_async().await;
    }
}
