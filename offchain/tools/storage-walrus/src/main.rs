#![doc = include_str!("../README.md")]

use nexus_toolkit::bootstrap;

mod client;
mod read_file;
mod read_json;
mod upload_file;
mod upload_json;
mod utils;
mod verify_blob;

#[tokio::main]
async fn main() {
    bootstrap!([
        upload_file::UploadFile,
        upload_json::UploadJson,
        read_json::ReadJson,
        verify_blob::VerifyBlob,
        read_file::ReadFile,
    ])
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        nexus_sdk::move_bindings::interface::meta_schema::MetaSchema,
        nexus_toolkit::NexusTool,
        serde_json::json,
    };

    async fn assert_error_ports<T: NexusTool>(input: serde_json::Value) {
        let output = T::new()
            .await
            .invoke(serde_json::from_value(input).unwrap())
            .await;
        let encoded = T::encode_output(output).unwrap();
        assert_eq!(encoded.tag, b"err");
        let schema = MetaSchema::from_offchain_json_schemas(
            &serde_json::to_vec(&schemars::schema_for!(T::Input)).unwrap(),
            &serde_json::to_vec(&schemars::schema_for!(T::Output)).unwrap(),
        )
        .unwrap();
        assert!(schema.canonical_output_ports(&encoded).is_ok());
    }

    #[tokio::test]
    async fn configuration_errors_remain_valid_tool_outputs() {
        let _env = client::test_support::WalrusEnv::new(None, Some("not an aggregator URL")).await;
        assert_error_ports::<upload_json::UploadJson>(json!({"json": "{}"})).await;
        assert_error_ports::<upload_file::UploadFile>(json!({"file_path": "/missing"})).await;
        let input = json!({"blob_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"});
        assert_error_ports::<read_json::ReadJson>(input.clone()).await;
        assert_error_ports::<read_file::ReadFile>(input.clone()).await;
        assert_error_ports::<verify_blob::VerifyBlob>(input).await;
    }
}
