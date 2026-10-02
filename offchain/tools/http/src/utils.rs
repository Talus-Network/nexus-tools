//! Helper functions for HTTP tool

use {
    crate::models::{HttpJsonSchema, SchemaValidationDetails},
    serde_json::Value,
};

struct DenyExternalReferences;

impl jsonschema::Retrieve for DenyExternalReferences {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema references are disabled".into())
    }
}

/// Validate JSON data against a schema and return detailed results
pub(crate) fn validate_schema_detailed(
    schema_def: &HttpJsonSchema,
    json_data: &Value,
) -> Result<SchemaValidationDetails, SchemaValidationDetails> {
    // Explicitly deny retrieval even if another dependency enables it.
    let validator = jsonschema::options()
        .with_retriever(DenyExternalReferences)
        .build(schema_def.schema.as_value())
        .map_err(|_| SchemaValidationDetails {
            name: schema_def.name.clone(),
            description: schema_def.description.clone(),
            strict: schema_def.strict,
            valid: false,
            errors: vec!["Invalid JSON schema; external references are disabled".to_string()],
        })?;

    let validation_result = validator.validate(json_data);
    match validation_result {
        Ok(_) => Ok(SchemaValidationDetails {
            name: schema_def.name.clone(),
            description: schema_def.description.clone(),
            strict: schema_def.strict,
            valid: true,
            errors: vec![],
        }),
        Err(_) => {
            let error_messages: Vec<String> = validator
                .iter_errors(json_data)
                .map(|e| e.to_string())
                .collect();
            Ok(SchemaValidationDetails {
                name: schema_def.name.clone(),
                description: schema_def.description.clone(),
                strict: schema_def.strict,
                valid: false,
                errors: error_messages,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn external_schema_references_return_a_sanitized_error() {
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
        for reference in [
            reqwest::Url::from_file_path(&file).unwrap().to_string(),
            format!("{}/schema", server.url()),
        ] {
            let schema: HttpJsonSchema = serde_json::from_value(serde_json::json!({
                "name": "test", "schema": {"$ref": reference}
            }))
            .unwrap();
            let error = validate_schema_detailed(&schema, &serde_json::json!({})).unwrap_err();
            assert_eq!(
                error.errors,
                ["Invalid JSON schema; external references are disabled"]
            );
            assert!(!error.valid);
        }
        remote.assert_async().await;
    }

    #[test]
    fn schemas_allow_internal_references_and_sanitize_invalid_values() {
        let schema: HttpJsonSchema = serde_json::from_value(serde_json::json!({
            "name": "test", "schema": {
                "$defs": {"value": {"type": "integer"}}, "$ref": "#/$defs/value"
            }
        }))
        .unwrap();
        assert!(
            validate_schema_detailed(&schema, &serde_json::json!(42))
                .unwrap()
                .valid
        );
        assert!(
            !validate_schema_detailed(&schema, &serde_json::json!("42"))
                .unwrap()
                .valid
        );
        let invalid: HttpJsonSchema = serde_json::from_value(serde_json::json!({
            "name": "test", "schema": {"type": "PRIVATE_TEST_MARKER"}
        }))
        .unwrap();
        let error = validate_schema_detailed(&invalid, &serde_json::json!({})).unwrap_err();
        assert!(error
            .errors
            .iter()
            .all(|error| !error.contains("PRIVATE_TEST_MARKER")));
    }
}
