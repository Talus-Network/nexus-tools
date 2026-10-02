//! # `xyz.taluslabs.storage.walrus.upload-file@1`
//!
//! Standard Nexus Tool that uploads a file to Walrus and returns the blob ID.

use {
    crate::{client::WalrusConfig, utils::validation::EndpointError},
    nexus_sdk::{
        fqn,
        walrus::{StorageInfo, WalrusError},
        ToolFqn,
    },
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    std::{path::PathBuf, time::Duration},
    thiserror::Error,
};

/// Errors that can occur during file upload
#[derive(Error, Debug)]
pub enum UploadFileError {
    #[error("Failed to upload file: {0}")]
    UploadError(#[from] WalrusError),
    #[error("Invalid file data: {0}")]
    InvalidFile(String),
    #[error("Refused endpoint: {0}")]
    Endpoint(#[from] EndpointError),
}

/// Types of errors that can occur during file upload
#[derive(Serialize, JsonSchema, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum UploadErrorKind {
    /// Error during network request
    Network,
    /// Error validating file
    Validation,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// The path to the file to upload
    file_path: String,
    /// The walrus publisher URL
    #[serde(
        default,
        deserialize_with = "crate::utils::validation::deserialize_url_opt"
    )]
    publisher_url: Option<String>,
    /// The number of epochs to store the file
    #[serde(default = "default_epochs")]
    epochs: u8,
    /// Optional address to which the created Blob object should be sent
    #[serde(default)]
    send_to: Option<String>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

pub(crate) struct UploadFile;

impl NexusTool for UploadFile {
    type Input = Input;
    type Output = Output;

    fn description() -> &'static str {
        "Uploads file bytes to Walrus and returns durable blob metadata."
    }

    async fn new() -> Self {
        Self {}
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.storage.walrus.upload-file@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn timeout() -> Duration {
        Duration::from_secs(30)
    }

    fn path() -> &'static str {
        "/upload-file"
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        match self.upload(input).await {
            Ok(storage_info) => handle_successful_upload(storage_info),
            Err(e) => {
                let (kind, status_code) = match &e {
                    UploadFileError::InvalidFile(_) | UploadFileError::Endpoint(_) => {
                        (UploadErrorKind::Validation, None)
                    }
                    UploadFileError::UploadError(err) => {
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

/// Handles the successful upload case by extracting the blob ID from the storage info
fn handle_successful_upload(storage_info: StorageInfo) -> Output {
    if let Some(newly_created) = storage_info.newly_created {
        Output::NewlyCreated {
            blob_id: newly_created.blob_object.blob_id,
            end_epoch: newly_created.blob_object.storage.end_epoch,
            sui_object_id: newly_created.blob_object.id,
        }
    } else if let Some(already_certified) = storage_info.already_certified {
        Output::AlreadyCertified {
            blob_id: already_certified.blob_id,
            end_epoch: already_certified.end_epoch,
            tx_digest: already_certified.event.tx_digest,
        }
    } else {
        Output::Err {
            reason: "Neither newly created nor already certified".to_string(),
            kind: UploadErrorKind::Validation,
            status_code: None,
        }
    }
}

/// See [`crate::utils::validation::resolve_upload_path_in`] for why this port is
/// off unless `WALRUS_UPLOAD_ROOT` is set.
fn resolve_file_path(file_path: &str) -> Result<PathBuf, UploadFileError> {
    crate::utils::validation::resolve_upload_path(file_path).map_err(UploadFileError::InvalidFile)
}

impl UploadFile {
    async fn upload(&self, input: Input) -> Result<StorageInfo, UploadFileError> {
        let file_path = resolve_file_path(&input.file_path)?;

        let walrus_client = WalrusConfig::new()
            .with_publisher_url(input.publisher_url)
            .build()
            .await?;

        let storage_info = crate::client::with_publisher_retry(|| {
            walrus_client.upload_file(&file_path, input.epochs, input.send_to.clone())
        })
        .await
        .map_err(UploadFileError::UploadError)?;

        Ok(storage_info)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        mockito::Server,
        nexus_sdk::walrus::WalrusClient,
        serde_json::json,
        tokio::sync::Mutex,
    };

    /// `WALRUS_UPLOAD_ROOT` is process-global, so these tests run one at a time.
    static UPLOAD_ROOT_LOCK: Mutex<()> = Mutex::const_new(());

    /// Points `WALRUS_UPLOAD_ROOT` at a fresh directory for one test.
    struct UploadRoot {
        _guard: tokio::sync::MutexGuard<'static, ()>,
        dir: PathBuf,
    }

    impl UploadRoot {
        async fn new(key: &str, files: &[(&str, &str)]) -> Self {
            let guard = UPLOAD_ROOT_LOCK.lock().await;
            let dir = std::env::temp_dir()
                .join("walrus-upload-file-tests")
                .join(key);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let dir = dir.canonicalize().unwrap();
            for (name, content) in files {
                std::fs::write(dir.join(name), content).unwrap();
            }
            std::env::set_var("WALRUS_UPLOAD_ROOT", &dir);
            Self { _guard: guard, dir }
        }
    }

    /// Holds the lock with `WALRUS_UPLOAD_ROOT` unset. Without the lock a
    /// concurrent test's root leaks in and the refusal reason changes.
    struct NoUploadRoot {
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    impl NoUploadRoot {
        async fn new() -> Self {
            let guard = UPLOAD_ROOT_LOCK.lock().await;
            std::env::remove_var("WALRUS_UPLOAD_ROOT");
            Self { _guard: guard }
        }
    }

    impl Drop for UploadRoot {
        fn drop(&mut self) {
            std::env::remove_var("WALRUS_UPLOAD_ROOT");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // Override upload method for testing
    impl UploadFile {
        // Helper method for testing
        fn with_custom_client() -> Self {
            Self {}
        }

        async fn upload_for_test(
            &self,
            input: Input,
            client: WalrusClient,
        ) -> Result<StorageInfo, UploadFileError> {
            let file_path = resolve_file_path(&input.file_path)?;

            let storage_info = client
                .upload_file(&file_path, input.epochs, input.send_to)
                .await
                .map_err(UploadFileError::UploadError)?;

            Ok(storage_info)
        }

        async fn create_server_and_input(file_path: &str) -> (mockito::ServerGuard, Input) {
            let server = Server::new_async().await;
            let server_url = server.url();

            // Set up test input with server URL
            let input = Input {
                file_path: file_path.to_string(),
                publisher_url: Some(server_url.clone()),
                epochs: 1,
                send_to: None,
            };

            (server, input)
        }
    }

    #[tokio::test]
    async fn test_upload_file_newly_created() {
        let file_path = "test.txt";
        let _root = UploadRoot::new("newly-created", &[(file_path, "test")]).await;

        // Create server and input
        let (mut server, input) = UploadFile::create_server_and_input(file_path).await;

        // Set up mock response for newly created blob
        let mock = server
            .mock("PUT", "/v1/blobs")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "newlyCreated": {
                        "blobObject": {
                            "blobId": "test_blob_id",
                            "id": "test_object_id",
                            "storage": {
                                "endEpoch": 100
                            }
                        }
                    },
                    "alreadyCertified": null
                })
                .to_string(),
            )
            .create_async()
            .await;

        // Create a client that points to our mock server
        let walrus_client = WalrusConfig::new()
            .with_publisher_url(Some(server.url()))
            .with_target_policy(crate::client::TargetPolicy::Unrestricted)
            .build()
            .await
            .expect("test endpoints are unrestricted");

        // Call the tool with our test client
        let tool = UploadFile::with_custom_client();
        let result = match tool.upload_for_test(input, walrus_client).await {
            Ok(storage_info) => handle_successful_upload(storage_info),
            Err(e) => Output::Err {
                reason: e.to_string(),
                kind: UploadErrorKind::Network,
                status_code: None,
            },
        };

        // Verify the result
        match result {
            Output::NewlyCreated {
                blob_id,
                end_epoch,
                sui_object_id,
            } => {
                assert_eq!(blob_id, "test_blob_id");
                assert_eq!(end_epoch, 100);
                assert_eq!(sui_object_id, "test_object_id");
            }
            Output::AlreadyCertified { .. } => {
                panic!("Expected NewlyCreated result, got AlreadyCertified");
            }
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                assert_eq!(reason, "Neither newly created nor already certified");
                assert_eq!(kind, UploadErrorKind::Validation);
                assert_eq!(status_code, None);
            }
        }

        // Verify that the mock was called
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_upload_file_already_certified() {
        // Create test file
        let file_path = "test_already_certified.txt";
        let _root = UploadRoot::new("already-certified", &[(file_path, "test")]).await;

        // Create server and input
        let (mut server, input) = UploadFile::create_server_and_input(file_path).await;

        // Set up mock response for already certified blob
        let mock = server
            .mock("PUT", "/v1/blobs")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "newlyCreated": null,
                    "alreadyCertified": {
                        "blobId": "certified_blob_id",
                        "endEpoch": 200,
                        "event": {
                            "txDigest": "certified_tx_digest",
                            "timestampMs": 12345678,
                            "suiAddress": "sui_address"
                        }
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        // Create a client that points to our mock server
        let walrus_client = WalrusConfig::new()
            .with_publisher_url(Some(server.url()))
            .with_target_policy(crate::client::TargetPolicy::Unrestricted)
            .build()
            .await
            .expect("test endpoints are unrestricted");

        // Call the tool with our test client
        let tool = UploadFile::with_custom_client();
        let result = match tool.upload_for_test(input, walrus_client).await {
            Ok(storage_info) => handle_successful_upload(storage_info),
            Err(e) => Output::Err {
                reason: e.to_string(),
                kind: UploadErrorKind::Network,
                status_code: None,
            },
        };

        // Verify the result
        match result {
            Output::NewlyCreated { .. } => {
                panic!("Expected AlreadyCertified result, got NewlyCreated");
            }
            Output::AlreadyCertified {
                blob_id,
                end_epoch,
                tx_digest,
            } => {
                assert_eq!(blob_id, "certified_blob_id");
                assert_eq!(end_epoch, 200);
                assert_eq!(tx_digest, "certified_tx_digest");
            }
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                assert_eq!(reason, "Neither newly created nor already certified");
                assert_eq!(kind, UploadErrorKind::Validation);
                assert_eq!(status_code, None);
            }
        }

        // Verify that the mock was called
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_upload_file_error() {
        // Create test file
        let file_path = "test_error.txt";
        let _root = UploadRoot::new("upload-error", &[(file_path, "test")]).await;

        // Create server and input
        let (mut server, input) = UploadFile::create_server_and_input(file_path).await;

        // Set up mock response for error
        let mock = server
            .mock("PUT", "/v1/blobs")
            .match_query(mockito::Matcher::Any)
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

        // Create a client that points to our mock server
        let walrus_client = WalrusConfig::new()
            .with_publisher_url(Some(server.url()))
            .with_target_policy(crate::client::TargetPolicy::Unrestricted)
            .build()
            .await
            .expect("test endpoints are unrestricted");

        // Call the tool with our test client
        let tool = UploadFile::with_custom_client();
        let output = match tool.upload_for_test(input, walrus_client).await {
            Ok(storage_info) => handle_successful_upload(storage_info),
            Err(e) => {
                let (kind, status_code) = match &e {
                    UploadFileError::InvalidFile(_) | UploadFileError::Endpoint(_) => {
                        (UploadErrorKind::Validation, None)
                    }
                    UploadFileError::UploadError(err) => {
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
        };

        // Verify the result
        match output {
            Output::NewlyCreated { .. } | Output::AlreadyCertified { .. } => {
                panic!("Expected error, but got success");
            }
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                assert!(reason.contains("500") || reason.contains("server error"));
                assert_eq!(kind, UploadErrorKind::Network);
                assert_eq!(status_code, Some(500));
            }
        }

        // Verify that the mock was called
        mock.assert_async().await;
    }

    /// The validation `reason`, failing the test on any other outcome.
    async fn invoke_expecting_validation_error(file_path: &str) -> String {
        let input = Input {
            file_path: file_path.to_string(),
            publisher_url: None,
            epochs: 1,
            send_to: None,
        };

        match UploadFile::with_custom_client().invoke(input).await {
            Output::Err {
                reason,
                kind,
                status_code,
            } => {
                assert_eq!(kind, UploadErrorKind::Validation);
                assert_eq!(status_code, None);
                reason
            }
            _ => panic!("expected a validation error for {file_path}"),
        }
    }

    #[tokio::test]
    async fn test_upload_invalid_file() {
        let _root = UploadRoot::new("invalid-file", &[]).await;

        let reason = invoke_expecting_validation_error("non_existent_file.txt").await;
        assert!(reason.contains("File does not exist"), "{reason}");
    }

    #[tokio::test]
    async fn test_local_upload_disabled_without_root() {
        let _no_root = NoUploadRoot::new().await;

        let reason = invoke_expecting_validation_error("/app/secrets/toolkit-config.json").await;
        assert!(reason.contains("disabled"), "{reason}");
    }

    /// The paths published to Walrus during the 2026-09-30 run. Configuring an
    /// upload root does not bring them back within reach.
    #[tokio::test]
    async fn test_exfiltration_paths_are_refused_with_a_root_configured() {
        let _root = UploadRoot::new("exfil-paths", &[]).await;

        for file_path in [
            "/app/secrets/toolkit-config.json",
            "/app/config/allowed-leaders.json",
            "/proc/self/environ",
            "/proc/1/environ",
            "/etc/passwd",
            "/etc/hostname",
            "../../../app/secrets/toolkit-config.json",
        ] {
            let reason = invoke_expecting_validation_error(file_path).await;
            assert!(
                reason.contains("relative path inside"),
                "{file_path}: {reason}"
            );
        }
    }

    /// The port is gated at deserialization, so a DAG naming a private target
    /// never reaches `invoke`.
    #[test]
    fn private_publishers_fail_input_deserialization() {
        for publisher_url in [
            "https://169.254.169.254/",
            "https://metadata.google.internal",
            "https://127.0.0.1:8080",
            "https://metadata/computeMetadata/v1/",
            // The 2026-09-30 probes, verbatim.
            "http://169.254.169.254/#",
            "http://metadata.google.internal/#",
        ] {
            let json = json!({ "file_path": "x", "publisher_url": publisher_url });
            assert!(
                serde_json::from_value::<Input>(json).is_err(),
                "expected {publisher_url} to be refused"
            );
        }
    }

    #[test]
    fn a_public_publisher_of_the_callers_choosing_is_accepted() {
        for publisher_url in [
            "https://publisher.walrus-testnet.walrus.space",
            "https://walrus-mainnet-publisher-1.staketab.org",
            "https://walrus.example.com:9000",
        ] {
            let json = json!({ "file_path": "x", "publisher_url": publisher_url });
            assert!(
                serde_json::from_value::<Input>(json).is_ok(),
                "expected {publisher_url} to be accepted"
            );
        }
    }
}
