//! # `xyz.taluslabs.storage.walrus.read-json@1`
//!
//! Standard Nexus Tool that reads a JSON file from Walrus and returns the JSON data.

use {
    crate::{
        client::{TargetPolicy, WalrusConfig},
        utils::validation::EndpointError,
    },
    nexus_sdk::{
        execution_limits::MAX_RESOLVED_INPUT_BYTES,
        fqn,
        types::{NexusData, OffchainToolOutput},
        walrus::WalrusError,
        ToolFqn,
    },
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    serde_json::Value,
    sha2::{Digest as _, Sha256},
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
    #[error("Invalid blob reference: {0}")]
    InvalidBlob(String),
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
        #[schemars(with = "Value")]
        json: NexusData,
    },
    Err {
        /// Detailed error message
        reason: String,
        /// Type of error (upload, validation, etc.)
        kind: ReadErrorKind,
        /// HTTP status code if available
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
        match self.read(input).await {
            Ok(json) => Output::Ok { json },
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
                    kind: match e {
                        ReadJsonError::InvalidJson(_) | ReadJsonError::InvalidBlob(_) => {
                            ReadErrorKind::Validation
                        }
                        ReadJsonError::ValidationError(_) => ReadErrorKind::Schema,
                        _ => ReadErrorKind::Network,
                    },
                    status_code,
                }
            }
        }
    }

    fn encode_output(output: Self::Output) -> AnyResult<OffchainToolOutput> {
        match output {
            Output::Ok { json } => {
                OffchainToolOutput::from_ports(b"ok".to_vec(), [("json".into(), json)])
            }
            error => OffchainToolOutput::from_json(serde_json::to_value(error)?),
        }
    }
}

impl ReadJson {
    async fn read(&self, input: Input) -> Result<NexusData, ReadJsonError> {
        let walrus_client = WalrusConfig::new()
            .with_aggregator_url(input.aggregator_url)
            .with_target_policy(self.target_policy)
            .build()
            .await?;

        let bytes = walrus_client
            .read_file_bounded(&input.blob_id, MAX_RESOLVED_INPUT_BYTES)
            .await?;
        let json = serde_json::from_slice(&bytes)
            .map_err(|error| ReadJsonError::InvalidJson(error.to_string()))?;
        if let Some(schema) = input.json_schema.as_ref() {
            validate(schema, &json)?;
        }

        // The blob already holds the output. Commit its original bytes without
        // uploading or serializing the decoded JSON again.
        NexusData::walrus_data(input.blob_id.as_bytes(), Sha256::digest(&bytes).to_vec())
            .map_err(|error| ReadJsonError::InvalidBlob(error.to_string()))
    }
}

struct DenyExternalReferences;

impl jsonschema::Retrieve for DenyExternalReferences {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema references are disabled".into())
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

    // Explicitly deny retrieval even if another dependency enables it.
    let validator = jsonschema::options()
        .with_retriever(DenyExternalReferences)
        .build(schema_def.schema.as_value())
        .map_err(|_| {
            ReadJsonError::ValidationError(
                "Invalid JSON schema; external references are disabled".to_string(),
            )
        })?;
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
    #[tokio::test]
    async fn invalid_and_external_schemas_return_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("private-schema.json");
        std::fs::write(&file, "true").unwrap();
        let mut server = mockito::Server::new_async().await;
        let remote = server
            .mock("GET", "/schema")
            .with_body("true")
            .expect(0)
            .create_async()
            .await;
        for schema in [
            serde_json::json!({"type":42}),
            serde_json::json!({"type":"PRIVATE_TEST_MARKER"}),
            serde_json::json!({"$ref":reqwest::Url::from_file_path(&file).unwrap().as_str()}),
            serde_json::json!({"$ref":format!("{}/schema", server.url())}),
        ] {
            let definition: WalrusJsonSchema = serde_json::from_value(serde_json::json!({
                "name":"test", "schema":schema
            }))
            .unwrap();
            let error = validate(&definition, &serde_json::json!({})).unwrap_err();
            assert_eq!(
                error.to_string(),
                "JSON validation error: Invalid JSON schema; external references are disabled"
            );
        }
        remote.assert_async().await;
    }

    #[test]
    fn schema_references_within_the_document_work() {
        let definition: WalrusJsonSchema = serde_json::from_value(serde_json::json!({
            "name": "test", "schema": {
                "$defs": {"value": {"type": "integer"}}, "$ref": "#/$defs/value"
            }
        }))
        .unwrap();
        assert!(validate(&definition, &serde_json::json!(42)).is_ok());
        assert!(validate(&definition, &serde_json::json!("42")).is_err());
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
            .mock(
                "GET",
                "/v1/blobs/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            )
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
                blob_id: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
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
            Output::Ok { json } => assert_eq!(
                json,
                NexusData::walrus_data(
                    b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                    Sha256::digest(
                        serde_json::to_vec(&json!({"name": "test", "value": 123})).unwrap(),
                    )
                    .to_vec(),
                )
                .unwrap(),
            ),
            Output::Err { reason, .. } => panic!("Expected valid JSON: {reason}"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn uploaded_json_flows_as_a_reference_through_the_sdk_decoder() {
        use {
            crate::{client::test_support::WalrusEnv, upload_json::UploadJson},
            nexus_sdk::{
                move_bindings::interface::meta_schema::{
                    MetaSchema,
                    OutputVariantSchema,
                    PortSchema,
                    ValueKind,
                },
                types::NexusValue,
                walrus::WalrusReader,
            },
            std::collections::HashMap,
        };

        let mut server = Server::new_async().await;
        let _env = WalrusEnv::new(Some(&server.url()), None).await;
        let blob_id = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let document = format!(
            " \n[{}, {{\"count\": 7}}]\n",
            serde_json::to_string(&"🦭".repeat(30_000)).unwrap(),
        );
        assert!(NexusData::inline_data(document.as_bytes().to_vec()).is_err());
        let upload = server
            .mock("PUT", "/v1/blobs")
            .match_query("epochs=1")
            .match_body(mockito::Matcher::Exact(document.clone()))
            .with_header("content-type", "application/json")
            .with_body(
                json!({"newlyCreated": {"blobObject": {
                    "blobId": blob_id, "id": "0x123", "storage": {"endEpoch": 100}
                }}})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let read = server
            .mock("GET", format!("/v1/blobs/{blob_id}").as_str())
            .with_body(document.clone())
            .expect(2)
            .create_async()
            .await;

        let uploaded = UploadJson
            .invoke(
                serde_json::from_value(json!({
                    "json": document,
                }))
                .unwrap(),
            )
            .await;
        assert!(matches!(
            uploaded,
            crate::upload_json::Output::NewlyCreated { .. }
        ));
        let tool = ReadJson {
            target_policy: TargetPolicy::Unrestricted,
        };
        let output = tool
            .invoke(Input {
                blob_id: blob_id.into(),
                aggregator_url: Some(server.url()),
                json_schema: None,
            })
            .await;
        let encoded = ReadJson::encode_output(output).unwrap();
        let producer = MetaSchema::from_offchain_json_schemas(
            &serde_json::to_vec(&schemars::schema_for!(Input)).unwrap(),
            &serde_json::to_vec(&schemars::schema_for!(Output)).unwrap(),
        )
        .unwrap();
        let ports = producer.canonical_output_ports(&encoded).unwrap();
        assert_eq!(ports.len(), 1);
        let reference = ports[0].1.clone();
        assert!(
            !reference.is_many(),
            "a JSON array remains one JSON document"
        );
        assert!(reference.has_walrus());

        let consumer = MetaSchema::new(
            vec![PortSchema::new(b"json".to_vec(), false, ValueKind::Data)],
            vec![OutputVariantSchema::new(b"ok".to_vec(), vec![])],
        );
        let inputs = HashMap::from([("json".into(), reference)]);
        let commitment = consumer.canonical_inputs_sha256(&inputs).unwrap();
        let reader = WalrusReader::new(&server.url(), MAX_RESOLVED_INPUT_BYTES).unwrap();
        let resolved = reader.resolve_ports(inputs).await.unwrap();
        assert_eq!(
            resolved["json"],
            vec![NexusValue::InlineData {
                bytes: document.as_bytes().to_vec(),
            }]
        );
        let wire = consumer.resolved_inputs_to_json(&resolved).unwrap();
        let decoded = consumer.resolved_inputs_from_json(&wire).unwrap();
        assert_eq!(
            consumer.resolved_inputs_sha256(&decoded).unwrap(),
            commitment
        );
        assert_eq!(
            consumer.resolved_inputs_to_semantic_json(&decoded).unwrap()["json"],
            serde_json::from_str::<Value>(&document).unwrap(),
        );
        upload.assert_async().await;
        read.assert_async().await;
    }

    #[tokio::test]
    async fn oversized_json_is_rejected_before_output_encoding() {
        let mut server = Server::new_async().await;
        let read = server
            .mock(
                "GET",
                "/v1/blobs/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            )
            .with_body(vec![b' '; MAX_RESOLVED_INPUT_BYTES + 1])
            .create_async()
            .await;
        let tool = ReadJson {
            target_policy: TargetPolicy::Unrestricted,
        };
        let output = tool
            .invoke(Input {
                blob_id: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                aggregator_url: Some(server.url()),
                json_schema: None,
            })
            .await;
        assert!(matches!(output, Output::Err { ref reason, .. } if reason.contains("byte limit")));
        let encoded = ReadJson::encode_output(output).unwrap();
        assert_eq!(encoded.tag, b"err");
        let schema = nexus_sdk::move_bindings::interface::meta_schema::MetaSchema::from_offchain_json_schemas(
            &serde_json::to_vec(&schemars::schema_for!(Input)).unwrap(),
            &serde_json::to_vec(&schemars::schema_for!(Output)).unwrap(),
        ).unwrap();
        assert!(schema.canonical_output_ports(&encoded).is_ok());
        read.assert_async().await;
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
