//! Helper functions for HTTP tool

use {
    crate::models::{HttpJsonSchema, SchemaValidationDetails},
    serde_json::Value,
};

/// Validate JSON data against a schema and return detailed results
pub(crate) fn validate_schema_detailed(
    schema_def: &HttpJsonSchema,
    json_data: &Value,
) -> Result<SchemaValidationDetails, SchemaValidationDetails> {
    // Convert schema to JSON value for validation
    let schema_value =
        serde_json::to_value(&schema_def.schema).map_err(|_| SchemaValidationDetails {
            name: schema_def.name.clone(),
            description: schema_def.description.clone(),
            strict: schema_def.strict,
            valid: false,
            errors: vec!["Schema serialization failed".to_string()],
        })?;

    // Validate using jsonschema
    let validator =
        nexus_toolkit::schema::compile(&schema_value).map_err(|e| SchemaValidationDetails {
            name: schema_def.name.clone(),
            description: schema_def.description.clone(),
            strict: schema_def.strict,
            valid: false,
            errors: vec![format!("Schema compilation failed: {}", e)],
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
    #[test]
    fn external_schema_references_return_a_sanitized_error() {
        let schema: HttpJsonSchema = serde_json::from_value(serde_json::json!({
            "name": "test", "schema": {"$ref": "file:///unused.json#/secret"}
        }))
        .unwrap();
        let error = validate_schema_detailed(&schema, &serde_json::json!({})).unwrap_err();
        assert!(error
            .errors
            .iter()
            .all(|error| !error.contains("unused.json")));
        assert!(!error.valid);
    }
}
