pub mod query;
pub mod retrieve_key;

#[cfg(test)]
use serde_json::Value;
use {
    anyhow::{Context, Result},
    nexus_sdk::{move_bindings::interface::meta_schema::MetaSchema, types::NexusData},
    schemars::{schema_for, JsonSchema},
    serde::Serialize,
    std::collections::HashMap,
};

pub fn canonical_input_hash<Input, Output>(input: &Input) -> Result<[u8; 32]>
where
    Input: Serialize + JsonSchema,
    Output: JsonSchema,
{
    let semantic = serde_json::to_value(input).context("Tool input could not be serialized")?;
    let object = semantic
        .as_object()
        .context("Tool input must be an object")?;
    let ports = object
        .iter()
        .map(|(name, value)| {
            let encoded = serde_json::to_vec(value).context("Tool port could not be serialized")?;
            Ok((name.clone(), NexusData::inline_data(encoded)?))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let input_schema = serde_json::to_vec(&schema_for!(Input))
        .context("Tool input schema could not be serialized")?;
    let output_schema = serde_json::to_vec(&schema_for!(Output))
        .context("Tool output schema could not be serialized")?;
    let schema = MetaSchema::from_offchain_json_schemas(&input_schema, &output_schema)
        .context("Toolkit schema could not be reconstructed")?;
    schema
        .canonical_inputs_sha256(&ports)
        .context("canonical Tool input hash could not be reconstructed")
}

#[cfg(test)]
pub(crate) fn canonical_hash_from_semantic_json<Input, Output>(
    input: &serde_json::Value,
) -> Result<[u8; 32]>
where
    Input: JsonSchema,
    Output: JsonSchema,
{
    let object = input.as_object().context("Tool input must be an object")?;
    let ports = object
        .iter()
        .map(|(name, value)| {
            let encoded = serde_json::to_vec(value).context("Tool port could not be serialized")?;
            Ok((name.clone(), NexusData::inline_data(encoded)?))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let input_schema = serde_json::to_vec(&schema_for!(Input))
        .context("Tool input schema could not be serialized")?;
    let output_schema = serde_json::to_vec(&schema_for!(Output))
        .context("Tool output schema could not be serialized")?;
    let schema = MetaSchema::from_offchain_json_schemas(&input_schema, &output_schema)
        .context("Toolkit schema could not be reconstructed")?;
    schema
        .canonical_inputs_sha256(&ports)
        .context("canonical Tool input hash could not be reconstructed")
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        schemars::JsonSchema,
        serde::{Deserialize, Serialize},
        serde_json::json,
    };

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Input {
        binding_id: String,
        payload: Value,
    }

    #[derive(Debug, Serialize, JsonSchema)]
    #[allow(dead_code)]
    #[serde(rename_all = "snake_case")]
    enum Output {
        Ok { result: Value },
        Err { reason: String },
    }

    #[test]
    fn canonical_hash_changes_with_the_parsed_request_body() {
        let a = Input {
            binding_id: "0x1".into(),
            payload: json!({"q":"A"}),
        };
        let b = Input {
            binding_id: "0x1".into(),
            payload: json!({"q":"B"}),
        };
        assert_ne!(
            canonical_input_hash::<Input, Output>(&a).unwrap(),
            canonical_input_hash::<Input, Output>(&b).unwrap()
        );
        let hash = canonical_input_hash::<Input, Output>(&a).unwrap();
        let semantic = serde_json::to_value(a).unwrap();
        assert_eq!(
            canonical_hash_from_semantic_json::<Input, Output>(&semantic).unwrap(),
            hash
        );
    }
}
