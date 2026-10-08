//! # `xyz.taluslabs.storage.walrus.upload-json@1`
//!
//! Standard Nexus Tool that uploads a JSON file to Walrus and returns the blob ID.

use {
    crate::{client::publisher_client, utils::validation::EndpointError},
    nexus_sdk::{
        fqn,
        walrus::{StorageInfo, WalrusError},
        ToolFqn,
    },
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    std::time::Duration,
    thiserror::Error,
};

/// Errors that can occur during JSON upload
#[derive(Error, Debug)]
pub enum UploadJsonError {
    #[error("Failed to upload JSON: {0}")]
    UploadError(#[from] WalrusError),
    #[error("Invalid JSON data: {0}")]
    InvalidJson(String),
    #[error("Refused endpoint: {0}")]
    Endpoint(#[from] EndpointError),
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// The JSON data to upload
    json: String,
    /// Number of epochs to store the data
    #[serde(default = "default_epochs")]
    epochs: u8,
    /// Optional address to which the created Blob object should be sent
    #[serde(default)]
    send_to_address: Option<String>,
}

fn default_epochs() -> u8 {
    1
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    AlreadyCertified {
        blob_id: String,
        end_epoch: u64,
        tx_digest: String,
    },
    NewlyCreated {
        blob_id: String,
        end_epoch: u64,
        sui_object_id: String,
    },
    Err {
        /// Detailed error message
        reason: String,
        /// Type of error (upload, validation, etc.)
        kind: UploadErrorKind,
        /// HTTP status code if available
        status_code: Option<u16>,
    },
}

/// Types of errors that can occur during JSON upload
#[derive(Serialize, JsonSchema, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum UploadErrorKind {
    /// Error during network request
    Network,
    /// Error validating JSON
    Validation,
}

pub(crate) struct UploadJson;

impl NexusTool for UploadJson {
    type Input = Input;
    type Output = Output;

    fn description() -> &'static str {
        "Validates and uploads JSON to Walrus and returns durable blob metadata."
    }

    async fn new() -> Self {
        Self {}
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.storage.walrus.upload-json@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn timeout() -> Duration {
        Duration::from_secs(30)
    }

    fn path() -> &'static str {
        "/json/upload"
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        match self.upload(input).await {
            Ok(storage_info) => {
                if let Some(ac) = &storage_info.already_certified {
                    Output::AlreadyCertified {
                        blob_id: ac.blob_id.clone(),
                        end_epoch: ac.end_epoch,
                        tx_digest: ac.event.tx_digest.clone(),
                    }
                } else {
                    let Some(created_blob) = storage_info.newly_created else {
                        return Output::Err {
                            reason: "Publisher response is missing a storage result".to_string(),
                            kind: UploadErrorKind::Network,
                            status_code: None,
                        };
                    };

                    Output::NewlyCreated {
                        blob_id: created_blob.blob_object.blob_id,
                        end_epoch: created_blob.blob_object.storage.end_epoch,
                        sui_object_id: created_blob.blob_object.id,
                    }
                }
            }
            Err(e) => {
                let (kind, status_code) = match &e {
                    UploadJsonError::InvalidJson(_) | UploadJsonError::Endpoint(_) => {
                        (UploadErrorKind::Validation, None)
                    }
                    UploadJsonError::UploadError(err) => {
                        let status_code = match err {
                            WalrusError::ApiError { status_code, .. } => Some(*status_code),
                            _ => None,
                        };
                        (UploadErrorKind::Network, status_code)
                    }
                };

                Output::Err {
                    reason: e.to_string(),
                    kind,
                    status_code,
                }
            }
        }
    }
}

impl UploadJson {
    async fn upload(&self, input: Input) -> Result<StorageInfo, UploadJsonError> {
        // Validate JSON before proceeding
        serde_json::from_str::<serde_json::Value>(&input.json)
            .map_err(|e| UploadJsonError::InvalidJson(e.to_string()))?;

        let walrus_client = publisher_client().await?;

        let storage_info = crate::client::with_publisher_retry(|| {
            walrus_client.upload_bytes(
                input.json.as_bytes().to_vec(),
                input.epochs,
                input.send_to_address.clone(),
            )
        })
        .await?;

        Ok(storage_info)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::client::test_support::WalrusEnv, mockito::Server, serde_json::json};

    fn input() -> Input {
        serde_json::from_value(json!({"json": "{\"value\":123}"})).unwrap()
    }

    #[test]
    fn upload_endpoints_are_absent_from_schema_and_rejected_as_inputs() {
        let schema = schemars::schema_for!(Input);
        for port in ["publisher_url", "aggregator_url"] {
            assert!(schema.as_value()["properties"].get(port).is_none());
            for value in [json!("https://caller.example.com"), json!(null)] {
                let mut request = json!({"json": "{}"});
                request[port] = value;
                let error = serde_json::from_value::<Input>(request).err().unwrap();
                assert!(error.to_string().contains("unknown field"), "{error}");
            }
        }
    }

    #[tokio::test]
    async fn test_upload_json_newly_created() {
        let mut publisher = Server::new_async().await;
        let _env = WalrusEnv::new(Some(&publisher.url()), Some("not an aggregator URL")).await;
        let upload = publisher
            .mock("PUT", "/v1/blobs")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("epochs".into(), "2".into()),
                mockito::Matcher::UrlEncoded("send_object_to".into(), "0x123".into()),
            ]))
            .match_body(mockito::Matcher::Exact("{\"value\":123}".into()))
            .with_header("content-type", "application/json")
            .with_body(
                json!({"newlyCreated": {"blobObject": {
                    "blobId": "test_blob_id", "id": "test_object_id",
                    "storage": {"endEpoch": 100}
                }}})
                .to_string(),
            )
            .create_async()
            .await;
        let mut input = input();
        input.epochs = 2;
        input.send_to_address = Some("0x123".into());

        match UploadJson.invoke(input).await {
            Output::NewlyCreated {
                blob_id,
                end_epoch,
                sui_object_id,
            } => {
                assert_eq!(blob_id, "test_blob_id");
                assert_eq!(end_epoch, 100);
                assert_eq!(sui_object_id, "test_object_id");
            }
            output => panic!(
                "unexpected output: {}",
                serde_json::to_value(output).unwrap()
            ),
        }
        upload.assert_async().await;
    }

    #[tokio::test]
    async fn test_upload_json_already_certified() {
        let mut publisher = Server::new_async().await;
        let _env = WalrusEnv::new(Some(&publisher.url()), None).await;
        let upload = publisher
            .mock("PUT", "/v1/blobs")
            .match_query("epochs=1")
            .with_header("content-type", "application/json")
            .with_body(
                json!({"alreadyCertified": {
                    "blobId": "certified_blob_id", "endEpoch": 200,
                    "event": {"txDigest": "certified_tx_digest"}
                }})
                .to_string(),
            )
            .create_async()
            .await;

        match UploadJson.invoke(input()).await {
            Output::AlreadyCertified {
                blob_id,
                end_epoch,
                tx_digest,
            } => {
                assert_eq!(blob_id, "certified_blob_id");
                assert_eq!(end_epoch, 200);
                assert_eq!(tx_digest, "certified_tx_digest");
            }
            output => panic!(
                "unexpected output: {}",
                serde_json::to_value(output).unwrap()
            ),
        }
        upload.assert_async().await;
    }

    #[tokio::test]
    async fn test_upload_json_error() {
        let mut publisher = Server::new_async().await;
        let _env = WalrusEnv::new(Some(&publisher.url()), None).await;
        let upload = publisher
            .mock("PUT", "/v1/blobs")
            .match_query("epochs=1")
            .with_status(500)
            .with_body("server error")
            .create_async()
            .await;

        match UploadJson.invoke(input()).await {
            Output::Err {
                kind, status_code, ..
            } => {
                assert_eq!(kind, UploadErrorKind::Network);
                assert_eq!(status_code, Some(500));
            }
            _ => panic!("expected a publisher error"),
        }
        upload.assert_async().await;
    }

    #[tokio::test]
    async fn uploads_require_an_explicit_publisher() {
        for publisher in [None, Some(""), Some("   ")] {
            let _env = WalrusEnv::new(publisher, None).await;
            match UploadJson.invoke(input()).await {
                Output::Err {
                    reason,
                    kind,
                    status_code,
                } => {
                    assert!(reason.contains("WALRUS_PUBLISHER_URL must be set for uploads"));
                    assert_eq!(kind, UploadErrorKind::Validation);
                    assert_eq!(status_code, None);
                }
                _ => panic!("expected a configuration error"),
            }
        }
    }

    #[tokio::test]
    async fn test_upload_invalid_json() {
        let mut input = input();
        input.json = "not JSON".into();
        match UploadJson.invoke(input).await {
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                assert!(reason.contains("Invalid JSON"));
                assert_eq!(kind, UploadErrorKind::Validation);
                assert_eq!(status_code, None);
            }
            _ => panic!("expected a validation error"),
        }
    }
}
