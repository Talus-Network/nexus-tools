//! # `xyz.taluslabs.storage.walrus.read-json@1`
//!
//! Standard Nexus Tool that reads a JSON file from Walrus and returns the JSON data.

use {
    crate::{
        client::{TargetPolicy, WalrusConfig},
        utils::validation::EndpointError,
    },
    nexus_sdk::{fqn, walrus::WalrusError, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    serde_json::Value,
    std::time::Duration,
    thiserror::Error,
};

/// Errors that can occur during JSON upload
#[derive(Error, Debug)]
pub enum ReadJsonError {
    #[error("Failed to read JSON: {0}")]
    ReadError(#[from] WalrusError),
    #[error("Refused endpoint: {0}")]
    Endpoint(#[from] EndpointError),
    #[error("Invalid JSON data: {0}")]
    InvalidJson(String),
    #[error("JSON validation error: {0}")]
    ValidationError(String),
}

/// Types of errors that can occur during JSON read
#[derive(Serialize, JsonSchema, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ReadErrorKind {
    /// Error during network request
    Network,
    /// Error validating JSON
    Validation,
    /// Error validating against schema
    Schema,
}

/// Defines the structure of the `json_schema` input port.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WalrusJsonSchema {
    /// The name of the schema. Must match `[a-zA-Z0-9-_]`, with a maximum
    /// length of 64.
    name: String,
    /// The JSON schema for the expected output.
    schema: schemars::Schema,
    /// A description of the expected format.
    description: Option<String>,
    /// Whether to enable strict schema adherence when validating the output.
    strict: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// The blob ID of the JSON file to read
    blob_id: String,
    /// The URL of the Walrus aggregator to read the JSON from
    #[serde(
        default,
        deserialize_with = "crate::utils::validation::deserialize_url_opt"
    )]
    aggregator_url: Option<String>,

    /// Optional JSON schema to validate the data against
    #[serde(default)]
    json_schema: Option<WalrusJsonSchema>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        /// The JSON data that was read
        json: Value,
    },
    Err {
        /// Detailed error message
        reason: String,
        /// Type of error (upload, validation, etc.)
        kind: ReadErrorKind,
        /// HTTP status code if available
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct ReadJson {
    target_policy: TargetPolicy,
}

impl NexusTool for ReadJson {
    type Input = Input;
    type Output = Output;

    fn description() -> &'static str {
        "Reads and optionally validates JSON from a Walrus blob."
    }

    async fn new() -> Self {
        Self {
            target_policy: TargetPolicy::PublicOnly,
        }
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.storage.walrus.read-json@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn timeout() -> Duration {
        Duration::from_secs(30)
    }

    fn path() -> &'static str {
        "/read-json"
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        match self
            .read(input.blob_id.clone(), input.aggregator_url.clone())
            .await
        {
            Ok(json_data) => {
                // If a JSON schema was provided, validate against it
                if let Some(schema_def) = input.json_schema.as_ref() {
                    // Validate JSON data against the provided schema
                    match validate(schema_def, &json_data) {
                        Ok(()) => {
                            // Schema validation passed
                            Output::Ok { json: json_data }
                        }
                        Err(e) => Output::Err {
                            reason: e.to_string(),
                            kind: ReadErrorKind::Schema,
                            status_code: None,
                        },
                    }
                } else {
                    // If we parsed valid JSON but no schema was provided
                    Output::Ok { json: json_data }
                }
            }
            Err(e) => {
                // Extract status code from WalrusError if available
                let status_code = match &e {
                    ReadJsonError::ReadError(WalrusError::ApiError { status_code, .. }) => {
                        Some(*status_code)
                    }
                    _ => None,
                };

                Output::Err {
                    reason: e.to_string(),
                    kind: if matches!(e, ReadJsonError::InvalidJson(_)) {
                        ReadErrorKind::Validation
                    } else {
                        ReadErrorKind::Network
                    },
                    status_code,
                }
            }
        }
    }
}

impl ReadJson {
    async fn read(
        &self,
        blob_id: String,
        aggregator_url: Option<String>,
    ) -> Result<Value, ReadJsonError> {
        let walrus_client = WalrusConfig::new()
            .with_aggregator_url(aggregator_url)
            .with_target_policy(self.target_policy)
            .build()
            .await?;

        walrus_client
            .read_json(&blob_id)
            .await
            .map_err(|error| match error {
                WalrusError::SerializationError(error) => {
                    ReadJsonError::InvalidJson(error.to_string())
                }
                error => ReadJsonError::ReadError(error),
            })
    }
}

fn validate(schema_def: &WalrusJsonSchema, json_data: &Value) -> Result<(), ReadJsonError> {
    // Extract the schema settings, using all fields
    let schema_name = &schema_def.name;
    let schema_description = schema_def
        .description
        .as_ref()
        .map(|desc| format!(": {}", desc))
        .unwrap_or_default();
    let strict_mode = schema_def.strict.unwrap_or(false);

    // Convert schema to JSON value for validation
    let schema_value = match serde_json::to_value(&schema_def.schema) {
        Ok(val) => val,
        Err(e) => {
            return Err(ReadJsonError::ValidationError(format!(
                "Schema serialization error: {}",
                e
            )));
        }
    };

    let validator = nexus_toolkit::schema::compile(&schema_value)
        .map_err(|e| ReadJsonError::ValidationError(e.to_string()))?;
    validator.validate(json_data).map_err(|errors| {
        // Validation failed with schema errors
        let error_message = format!(
            "Schema validation failed for '{}{}': {}{}",
            schema_name,
            schema_description,
            if strict_mode { "[STRICT MODE] " } else { "" },
            errors
        );

        ReadJsonError::ValidationError(error_message)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_and_external_schemas_return_errors() {
        for schema in [
            serde_json::json!({"type":42}),
            serde_json::json!({"$ref":"file:///unused.json"}),
        ] {
            let definition: WalrusJsonSchema = serde_json::from_value(serde_json::json!({
                "name":"test", "schema":schema
            }))
            .unwrap();
            assert!(validate(&definition, &serde_json::json!({})).is_err());
        }
    }

    use {super::*, mockito::Server, nexus_sdk::walrus::WalrusClient, serde_json::json};

    // Helper function to create test input
    fn create_test_input() -> Input {
        Input {
            blob_id: "test_blob_id".to_string(),
            aggregator_url: None,
            json_schema: None,
        }
    }

    // Helper function to create mock server and client
    async fn create_mock_server_and_client() -> (mockito::ServerGuard, WalrusClient) {
        let server = Server::new_async().await;
        let client = WalrusConfig::new()
            .with_aggregator_url(Some(server.url()))
            .with_target_policy(crate::client::TargetPolicy::Unrestricted)
            .build()
            .await
            .expect("test endpoints are unrestricted");

        (server, client)
    }

    #[tokio::test]
    async fn test_read_json_success() {
        let (mut server, client) = create_mock_server_and_client().await;
        let input = create_test_input();

        // Mock successful response
        let mock = server
            .mock("GET", "/v1/blobs/test_blob_id")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "name": "test",
                    "value": 123
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result: Result<serde_json::Value, WalrusError> =
            client.read_json::<serde_json::Value>(&input.blob_id).await;

        match result {
            Ok(json_data) => {
                assert_eq!(json_data["name"], "test");
                assert_eq!(json_data["value"], 123);
            }
            Err(e) => panic!("Expected successful JSON read, but got error: {}", e),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_read_json_not_found() {
        let (mut server, _client) = create_mock_server_and_client().await;

        // Set aggregator_url to the mock server URL
        let input = Input {
            blob_id: "test_blob_id".to_string(),
            aggregator_url: Some(server.url()),
            json_schema: None,
        };

        // Mock not found response
        let mock = server
            .mock("GET", "/v1/blobs/test_blob_id")
            .with_status(404)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "error": "Blob not found"
                })
                .to_string(),
            )
            .create_async()
            .await;

        let tool = ReadJson {
            target_policy: TargetPolicy::Unrestricted,
        };
        let output = tool.invoke(input).await;

        match output {
            Output::Ok { .. } => panic!("Expected error output, but got successful JSON read"),
            Output::Err {
                reason: _,
                kind,
                status_code,
            } => {
                assert_eq!(kind, ReadErrorKind::Network);
                assert_eq!(status_code, Some(404));
            }
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_read_json_server_error() {
        let (mut server, _client) = create_mock_server_and_client().await;

        // Set aggregator_url to the mock server URL
        let input = Input {
            blob_id: "test_blob_id".to_string(),
            aggregator_url: Some(server.url()),
            json_schema: None,
        };

        // Mock server error
        let mock = server
            .mock("GET", "/v1/blobs/test_blob_id")
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "error": "Internal server error"
                })
                .to_string(),
            )
            .create_async()
            .await;

        let tool = ReadJson {
            target_policy: TargetPolicy::Unrestricted,
        };
        let output = tool.invoke(input).await;

        match output {
            Output::Ok { .. } => panic!("Expected error output, but got successful JSON read"),
            Output::Err {
                reason: _,
                kind,
                status_code,
            } => {
                assert_eq!(kind, ReadErrorKind::Network);
                assert_eq!(status_code, Some(500));
            }
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_read_json_invalid_json() {
        let (mut server, _client) = create_mock_server_and_client().await;

        // Set aggregator_url to the mock server URL
        let _input = Input {
            blob_id: "test_blob_id".to_string(),
            aggregator_url: Some(server.url()),
            json_schema: None,
        };

        // Mock response with invalid JSON
        let mock = server
            .mock("GET", "/v1/blobs/test_blob_id")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("invalid json")
            .create_async()
            .await;

        // Call the tool directly with the input
        let tool = ReadJson {
            target_policy: TargetPolicy::Unrestricted,
        };
        let output = tool
            .invoke(Input {
                blob_id: "test_blob_id".to_string(),
                aggregator_url: Some(server.url()),
                json_schema: None,
            })
            .await;

        match output {
            Output::Ok { .. } => panic!("Expected error for invalid JSON, got OK response"),
            Output::Err { kind, reason, .. } => {
                assert_eq!(kind, ReadErrorKind::Validation);
                assert!(reason.contains("Invalid JSON data"));
            }
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_read_json_with_custom_aggregator() {
        let (mut server, client) = create_mock_server_and_client().await;
        let mut input = create_test_input();
        input.aggregator_url = Some(server.url());

        // Mock successful response
        let mock = server
            .mock("GET", "/v1/blobs/test_blob_id")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "name": "test",
                    "value": 123
                })
                .to_string(),
            )
            .create_async()
            .await;

        let result: Result<serde_json::Value, WalrusError> =
            client.read_json::<serde_json::Value>(&input.blob_id).await;
        assert!(result.is_ok());
        let json_data = result.unwrap();
        assert_eq!(json_data["name"], "test");
        assert_eq!(json_data["value"], 123);

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_read_json_with_schema_validation_success() {
        let (mut server, _client) = create_mock_server_and_client().await;

        // Use #[allow(dead_code)] to suppress warnings for test-only struct
        #[allow(dead_code)]
        #[derive(schemars::JsonSchema)]
        struct SimpleSchema {
            name: String,
            value: i32,
        }

        let schema = schemars::schema_for!(SimpleSchema);

        // Mock successful response with valid JSON according to schema
        // Need to make sure the Content-Type is application/json
        let mock = server
            .mock("GET", "/v1/blobs/test_blob_id")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "name": "test",
                    "value": 123
                })
                .to_string(),
            )
            .create_async()
            .await;

        // Call the tool directly with schema
        let tool = ReadJson {
            target_policy: TargetPolicy::Unrestricted,
        };
        let output = tool
            .invoke(Input {
                blob_id: "test_blob_id".to_string(),
                aggregator_url: Some(server.url()),
                json_schema: Some(WalrusJsonSchema {
                    name: "TestSchema".to_string(),
                    schema,
                    description: Some("A test schema for basic JSON validation".to_string()),
                    strict: Some(false),
                }),
            })
            .await;

        match output {
            Output::Ok { json } => assert_eq!(json, json!({"name": "test", "value": 123})),
            Output::Err { reason, .. } => panic!("Expected valid JSON: {reason}"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_read_json_with_schema_validation_failure() {
        let (mut server, _client) = create_mock_server_and_client().await;

        // Use #[allow(dead_code)] to suppress warnings for test-only struct
        #[allow(dead_code)]
        #[derive(schemars::JsonSchema)]
        struct TestSchemaWithRequiredField {
            name: String,
            value: i32,
            required_field: String,
        }

        let schema = schemars::schema_for!(TestSchemaWithRequiredField);

        // Mock response with JSON missing a required field
        let mock = server
            .mock("GET", "/v1/blobs/test_blob_id")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "name": "test",
                    "value": 123
                    // missing required_field
                })
                .to_string(),
            )
            .create_async()
            .await;

        // Call the tool directly with strict schema
        let tool = ReadJson {
            target_policy: TargetPolicy::Unrestricted,
        };
        let output = tool
            .invoke(Input {
                blob_id: "test_blob_id".to_string(),
                aggregator_url: Some(server.url()),
                json_schema: Some(WalrusJsonSchema {
                    name: "StrictSchema".to_string(),
                    schema,
                    description: Some("A schema requiring the required_field property".to_string()),
                    strict: Some(true),
                }),
            })
            .await;

        match output {
            Output::Ok { .. } => panic!("Expected schema validation error"),
            Output::Err { kind, reason, .. } => {
                assert_eq!(kind, ReadErrorKind::Schema);
                assert!(
                    reason.contains("Schema validation failed for 'StrictSchema:"),
                    "{reason}"
                );
                assert!(reason.contains("required_field"));
                assert!(reason.contains("[STRICT MODE]"));
            }
        }

        mock.assert_async().await;
    }
}
