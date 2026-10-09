use {
    agent_api::tools::{
        query::{QueryInput, QueryOutput},
        retrieve_key::{RetrieveKeyInput, RetrieveKeyOutput},
    },
    nexus_sdk::{
        move_bindings::{
            interface::meta_schema::{MetaSchema, ValueKind},
            primitives::data::NexusValue,
        },
        sui::grpc::{open_signature_body::Type as MoveType, OpenSignatureBody},
        types::NexusData,
    },
    std::collections::HashMap,
};

const REGISTER_MOVE_TEMPLATE: &str =
    include_str!("../../onchain/agent_api/templates/sources/register.move.in");

fn offchain_meta_schema<I: schemars::JsonSchema, O: schemars::JsonSchema>() -> MetaSchema {
    let input = serde_json::to_vec(&schemars::schema_for!(I)).expect("input schema serializes");
    let output = serde_json::to_vec(&schemars::schema_for!(O)).expect("output schema serializes");
    MetaSchema::from_offchain_json_schemas(&input, &output).expect("SDK accepts Tool schemas")
}

fn registration_ascii_move_type() -> String {
    let (move_path, alias) = REGISTER_MOVE_TEMPLATE
        .lines()
        .find_map(|line| {
            let line = line.trim().strip_prefix("use ")?.strip_suffix(';')?;
            let (move_path, alias) = line.split_once(" as ")?;
            move_path
                .starts_with("std::ascii::")
                .then_some((move_path, alias))
        })
        .expect("registration template imports its ASCII string type");
    assert_eq!(alias, "AsciiString");

    let mut path = move_path.split("::");
    assert_eq!(
        path.next(),
        Some("std"),
        "the Move standard library uses 0x1"
    );
    let module = path.next().expect("Move type has a module");
    let name = path.next().expect("Move type has a name");
    assert!(path.next().is_none(), "Move type import is a concrete type");
    format!("0x1::{module}::{name}")
}

fn registration_binding_id_move_type() -> String {
    let declared_type = REGISTER_MOVE_TEMPLATE
        .split("public enum Output")
        .nth(1)
        .and_then(|output| output.split("Registered {").nth(1))
        .and_then(|registered| registered.split('}').next())
        .and_then(|fields| fields.split_once("binding_id:"))
        .and_then(|(_, ty)| ty.split(',').next())
        .map(str::trim)
        .expect("public Registered output declares binding_id");
    assert_eq!(
        declared_type, "AsciiString",
        "binding_id uses its imported Move type"
    );

    registration_ascii_move_type()
}

fn registration_owner_key_move_type() -> String {
    let declared_type = REGISTER_MOVE_TEMPLATE
        .split("public fun execute(")
        .nth(1)
        .and_then(|signature| signature.split(") {").next())
        .and_then(|parameters| {
            parameters
                .lines()
                .find(|line| line.contains("owner_public_key_hex:"))
        })
        .and_then(|line| line.split_once(':'))
        .map(|(_, ty)| ty.trim().trim_end_matches(','))
        .expect("registration execute declares its owner-key input");
    assert_eq!(
        declared_type, "AsciiString",
        "registration owner key uses the imported scalar string type"
    );
    registration_ascii_move_type()
}

fn schema_for_move_type(type_name: &str) -> serde_json::Value {
    let move_type = OpenSignatureBody::default()
        .with_type(MoveType::Datatype)
        .with_type_name(type_name);
    nexus_sdk::onchain_schema_gen::convert_move_type_to_schema(&move_type)
        .expect("pinned SDK converts the registration Move output type")
}

fn registration_meta_schema(binding_id_schema: serde_json::Value) -> MetaSchema {
    let output = serde_json::json!({
        "registered": {
            "type": "variant",
            "description": "Registered variant",
            "fields": {
                "binding_id": binding_id_schema,
            },
        },
    })
    .to_string();
    MetaSchema::from_onchain_json_schemas("{}", &output).expect("SDK accepts registration schema")
}

fn inline_json(value: &serde_json::Value) -> NexusValue {
    NexusValue::InlineData {
        bytes: serde_json::to_vec(value).expect("JSON value serializes"),
    }
}

fn input_port<'a>(
    schema: &'a MetaSchema,
    name: &[u8],
) -> &'a nexus_sdk::move_bindings::interface::meta_schema::PortSchema {
    schema
        .input_ports
        .iter()
        .find(|port| port.port_name == name)
        .expect("Tool schema includes the requested input")
}

#[test]
fn registered_binding_id_is_projected_to_query_and_retrieve_key_strings() {
    let query = offchain_meta_schema::<QueryInput, QueryOutput>();
    let retrieve_key = offchain_meta_schema::<RetrieveKeyInput, RetrieveKeyOutput>();
    let binding_id = "0x0123456789abcdef";

    assert_eq!(
        input_port(&query, b"binding_id").value_kind,
        ValueKind::Data
    );
    assert_eq!(
        input_port(&retrieve_key, b"binding_id").value_kind,
        ValueKind::Data
    );

    let former_object_schema = schema_for_move_type("0x2::object::ID");
    assert_eq!(former_object_schema["type"], "object_id");
    let former_object_output = registration_meta_schema(former_object_schema);
    let former_object_port = &former_object_output.output_variants[0].ports[0];
    assert_eq!(
        former_object_port.value_kind,
        ValueKind::Object,
        "the pinned SDK derives Object from the former Move ID type"
    );
    assert_ne!(
        former_object_port.value_kind,
        input_port(&query, b"binding_id").value_kind,
        "the former object_value registration result cannot feed a Tool String/Data port"
    );

    let registration_schema = schema_for_move_type(&registration_binding_id_move_type());
    assert_eq!(registration_schema["type"], "string");
    let registration = registration_meta_schema(registration_schema);
    assert_eq!(registration.output_variants[0].variant_name, b"registered");
    let output_port = &registration.output_variants[0].ports[0];
    assert_eq!(output_port.port_name, b"binding_id");
    assert_eq!(output_port.value_kind, ValueKind::Data);
    assert_eq!(
        output_port.value_kind,
        input_port(&query, b"binding_id").value_kind
    );
    assert_eq!(
        output_port.value_kind,
        input_port(&retrieve_key, b"binding_id").value_kind
    );
    let emitted_value = inline_json(&serde_json::json!(binding_id));
    assert!(
        MetaSchema::conforms_resolved_port(output_port, std::slice::from_ref(&emitted_value)),
        "the public registration schema accepts its actual InlineData payload"
    );
    assert!(
        !MetaSchema::conforms_resolved_port(
            former_object_port,
            std::slice::from_ref(&emitted_value)
        ),
        "the former public Object schema rejects the emitted InlineData payload"
    );
    assert!(MetaSchema::conforms_resolved_port(
        input_port(&query, b"binding_id"),
        std::slice::from_ref(&emitted_value)
    ));
    assert!(MetaSchema::conforms_resolved_port(
        input_port(&retrieve_key, b"binding_id"),
        std::slice::from_ref(&emitted_value)
    ));

    let resolved = HashMap::from([
        (
            "binding_id".to_owned(),
            vec![inline_json(&serde_json::json!(binding_id))],
        ),
        (
            "payload".to_owned(),
            vec![inline_json(&serde_json::json!({"prompt": "local demo"}))],
        ),
    ]);
    let semantic = query
        .resolved_inputs_to_semantic_json(&resolved)
        .expect("SDK converts the registration InlineData output into Tool JSON");
    assert_eq!(semantic["binding_id"], binding_id);
    assert_eq!(semantic["payload"]["prompt"], "local demo");

    let retrieve_resolved = HashMap::from([(
        "binding_id".to_owned(),
        vec![inline_json(&serde_json::json!(binding_id))],
    )]);
    let semantic = retrieve_key
        .resolved_inputs_to_semantic_json(&retrieve_resolved)
        .expect("SDK converts the same registration output for RetrieveKey");
    assert_eq!(semantic["binding_id"], binding_id);
}

#[test]
fn registration_owner_key_is_one_scalar_data_string() {
    let owner_key_schema = schema_for_move_type(&registration_owner_key_move_type());
    assert_eq!(owner_key_schema["type"], "string");

    let input_json = serde_json::json!({
        "owner_public_key_hex": owner_key_schema,
    })
    .to_string();
    let registration =
        MetaSchema::from_onchain_json_schemas(&input_json, r#"{"registered":{"fields":{}}}"#)
            .expect("SDK converts the scalar registration owner-key input");
    let input_port = input_port(&registration, b"owner_public_key_hex");
    assert_eq!(input_port.value_kind, ValueKind::Data);
    assert!(!input_port.is_many);

    let disabled_input = NexusData::inline_data(serde_json::to_vec("").unwrap())
        .expect("empty owner key is one inline-data input");
    let enabled_key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let enabled_input = NexusData::inline_data(serde_json::to_vec(enabled_key_hex).unwrap())
        .expect("enabled owner key is one inline-data input");
    assert!(!disabled_input.is_many());
    assert!(!enabled_input.is_many());

    for value in ["", enabled_key_hex] {
        let resolved = inline_json(&serde_json::json!(value));
        assert!(MetaSchema::conforms_resolved_port(
            input_port,
            std::slice::from_ref(&resolved),
        ));
    }
}
