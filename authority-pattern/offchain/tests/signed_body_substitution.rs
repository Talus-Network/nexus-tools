use {
    agent_api::{
        backend::{Backend, DeploymentConfig},
        crypto::{decrypt_export, seal_at_rest, KeyEnvelope, MasterKey},
        events::EventOrigin,
        provider::{mock_routes, HttpProvider, Provider},
        storage::{BindingRecord, GrantRecord, Store},
        tools::{
            query::{QueryInput, QueryOutput},
            retrieve_key::{RetrieveKeyInput, RetrieveKeyOutput},
        },
    },
    ed25519_dalek::SigningKey,
    nexus_sdk::{
        move_bindings::interface::meta_schema::MetaSchema,
        signed_http::v3::wire::sign_request,
        types::{NexusData, NexusValue, OffchainToolOutput},
    },
    schemars::{schema_for, JsonSchema},
    serde::Serialize,
    serde_json::{json, Value},
    std::{
        collections::HashMap,
        net::{SocketAddr, TcpListener},
        process::{Child, Command, Stdio},
        sync::Arc,
        time::Duration,
    },
    tokio::task::JoinHandle,
    warp::Filter,
    x25519_dalek::{PublicKey, StaticSecret},
};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct ServerGuard(JoinHandle<()>);

impl ServerGuard {
    async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn unused_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn rpc_event_json(digest: &str, event_seq: u64, type_tag: String, parsed_json: Value) -> Value {
    json!({
        "id": {"txDigest": digest, "eventSeq": event_seq.to_string()},
        "type": type_tag,
        "parsedJson": parsed_json
    })
}

async fn wait_for_http(url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client.get(url).send().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    anyhow::bail!("local test server did not become ready")
}

async fn wait_for_tool(child: &mut Child, address: SocketAddr) -> anyhow::Result<()> {
    let url = format!("http://{address}/health");
    let client = reqwest::Client::new();
    for _ in 0..200 {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("Toolkit process exited during startup: {status}");
        }
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    anyhow::bail!("Toolkit process did not become ready")
}

fn spawn_tool(
    address: SocketAddr,
    toolkit_config: &std::path::Path,
    store_db: &std::path::Path,
    provider_address: SocketAddr,
    deployments: &str,
) -> anyhow::Result<Child> {
    Ok(Command::new(env!("CARGO_BIN_EXE_agent-api"))
        .env_clear()
        .env("BIND_ADDR", address.to_string())
        .env("NEXUS_TOOLKIT_CONFIG_PATH", toolkit_config)
        .env("AGENT_API_DB_PATH", store_db)
        .env("AGENT_API_DEPLOYMENTS", deployments)
        .env("AGENT_API_MASTER_KEY", "5a".repeat(32))
        .env(
            "AGENT_API_PROVIDER_URL",
            format!("http://{provider_address}"),
        )
        .env(
            "AGENT_API_PROVIDER_OPERATOR_KEY",
            "synthetic-integration-operator",
        )
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

fn resolved_transport<Input: Serialize>(input: &Input) -> anyhow::Result<serde_json::Value> {
    let semantic = serde_json::to_value(input)?;
    let object = semantic
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("query input must serialize as an object"))?;
    let ports = object
        .iter()
        .map(|(name, value)| {
            let encoded = serde_json::to_vec(value)?;
            let nexus_value = NexusData::inline_data(encoded)?;
            Ok(json!({
                "port_name": name,
                "value": nexus_value.to_json_value()?
            }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(json!({"ports": ports}))
}

/// Reconstructs the signed input commitment with the pinned SDK schema encoder.
fn pinned_sdk_input_hash<Input, Output>(input: &Input) -> anyhow::Result<[u8; 32]>
where
    Input: Serialize + JsonSchema,
    Output: JsonSchema,
{
    let semantic = serde_json::to_value(input)?;
    let object = semantic
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Tool input must be an object"))?;
    let ports = object
        .iter()
        .map(|(name, value)| {
            let encoded = serde_json::to_vec(value)?;
            Ok((name.clone(), NexusData::inline_data(encoded)?))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;
    let input_schema = serde_json::to_vec(&schema_for!(Input))?;
    let output_schema = serde_json::to_vec(&schema_for!(Output))?;
    MetaSchema::from_offchain_json_schemas(&input_schema, &output_schema)?
        .canonical_inputs_sha256(&ports)
}

fn output_port_bytes(output: &OffchainToolOutput, name: &[u8]) -> anyhow::Result<Vec<u8>> {
    let port = output
        .ports
        .iter()
        .find(|port| port.port_name == name)
        .ok_or_else(|| anyhow::anyhow!("canonical output is missing {name:?}"))?;
    let [NexusValue::InlineData { bytes }] = port.values.as_slice() else {
        anyhow::bail!("canonical output port {name:?} must have one inline value")
    };
    Ok(bytes.clone())
}

#[tokio::test]
async fn signed_http_rejects_substitution_and_replays_completed_requests() -> anyhow::Result<()> {
    let temp = std::env::temp_dir().join(format!("agent-api-signed-http-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp)?;
    let provider_db = temp.join("provider.sqlite");
    let store_db = temp.join("agent-api.sqlite");
    let toolkit_config = temp.join("toolkit.json");
    let operator_key = "synthetic-integration-operator".to_owned();
    let provider_routes = mock_routes(&provider_db, operator_key.clone())?;
    let provider_address = unused_address();
    let provider_server = ServerGuard(tokio::spawn(
        warp::serve(provider_routes).run(provider_address),
    ));
    wait_for_http(&format!("http://{provider_address}/health")).await?;

    let provider = HttpProvider::new(format!("http://{provider_address}"), operator_key)?;
    let binding_id = "0x44";
    let provider_key = provider.create_key(binding_id).await?;
    provider.credit(&provider_key, 10, "signed-0x44:1").await?;
    let master_key = MasterKey::from_hex(&"5a".repeat(32))?;
    let owner_private_key = [0x41; 32];
    let owner_secret = StaticSecret::from(owner_private_key);
    let owner_public_key = PublicKey::from(&owner_secret).to_bytes().to_vec();
    let mut store = Store::open(&store_db)?;
    store.insert_registration(&BindingRecord {
        binding_id: binding_id.to_owned(),
        cashier_id: "0x22".to_owned(),
        wallet_id: "0x33".to_owned(),
        owner: "0x55".to_owned(),
        expected_agent_uid: "0x66".to_owned(),
        origin: Some(EventOrigin {
            package_id: "0xabc".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x2::sui::SUI".to_owned(),
            cashier_id: "0x22".to_owned(),
        }),
        rate: 1,
        export_enabled: true,
        owner_public_key,
        status: "active".to_owned(),
        provider_key_box: None,
        charged_coin_units: 0,
        credit_units: 0,
        settled_usage: 0,
        recognized_coin_units: 0,
        latest_provider_usage: 0,
        refunded: false,
    })?;
    store.set_provider_key_box(
        binding_id,
        &seal_at_rest(&master_key, binding_id, provider_key.as_bytes())?,
    )?;
    store.record_charge("signed-0x44:1", binding_id, 10, 10)?;

    let second_binding_id = "0x45";
    let second_provider_key = provider.create_key(second_binding_id).await?;
    provider
        .credit(&second_provider_key, 10, "signed-0x45:1")
        .await?;
    let second_owner_private_key = [0x42; 32];
    let second_owner_secret = StaticSecret::from(second_owner_private_key);
    let second_owner_public_key = PublicKey::from(&second_owner_secret).to_bytes().to_vec();
    store.insert_registration(&BindingRecord {
        binding_id: second_binding_id.to_owned(),
        cashier_id: "0x23".to_owned(),
        wallet_id: "0x34".to_owned(),
        owner: "0x56".to_owned(),
        expected_agent_uid: "0x67".to_owned(),
        origin: Some(EventOrigin {
            package_id: "0xdef".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x123::test_coin::TEST_COIN".to_owned(),
            cashier_id: "0x23".to_owned(),
        }),
        rate: 1,
        export_enabled: true,
        owner_public_key: second_owner_public_key,
        status: "active".to_owned(),
        provider_key_box: None,
        charged_coin_units: 0,
        credit_units: 0,
        settled_usage: 0,
        recognized_coin_units: 0,
        latest_provider_usage: 0,
        refunded: false,
    })?;
    store.set_provider_key_box(
        second_binding_id,
        &seal_at_rest(
            &master_key,
            second_binding_id,
            second_provider_key.as_bytes(),
        )?,
    )?;
    store.record_charge("signed-0x45:1", second_binding_id, 10, 10)?;

    let query_a = QueryInput {
        binding_id: binding_id.to_owned(),
        payload: json!({"prompt": "A"}),
    };
    let input_hash = pinned_sdk_input_hash::<QueryInput, QueryOutput>(&query_a)?;
    let nonce = [0x37; 32];
    store.insert_grant(&GrantRecord {
        nonce,
        binding_id: binding_id.to_owned(),
        operation: "query".to_owned(),
        input_hash,
        event_id: "signed-0x44:2".to_owned(),
        consumed: false,
    })?;
    let successful_query_nonce = [0x38; 32];
    store.insert_grant(&GrantRecord {
        nonce: successful_query_nonce,
        binding_id: binding_id.to_owned(),
        operation: "query".to_owned(),
        input_hash,
        event_id: "signed-0x44:3".to_owned(),
        consumed: false,
    })?;
    let retrieve_key = RetrieveKeyInput {
        binding_id: binding_id.to_owned(),
    };
    let retrieve_key_hash =
        pinned_sdk_input_hash::<RetrieveKeyInput, RetrieveKeyOutput>(&retrieve_key)?;
    let retrieve_key_nonce = [0x39; 32];
    store.insert_grant(&GrantRecord {
        nonce: retrieve_key_nonce,
        binding_id: binding_id.to_owned(),
        operation: "retrieve-key".to_owned(),
        input_hash: retrieve_key_hash,
        event_id: "signed-0x44:4".to_owned(),
        consumed: false,
    })?;

    let second_query = QueryInput {
        binding_id: second_binding_id.to_owned(),
        payload: json!({"prompt": "second deployment"}),
    };
    let second_query_hash = pinned_sdk_input_hash::<QueryInput, QueryOutput>(&second_query)?;
    let second_query_nonce = [0x3a; 32];
    store.insert_grant(&GrantRecord {
        nonce: second_query_nonce,
        binding_id: second_binding_id.to_owned(),
        operation: "query".to_owned(),
        input_hash: second_query_hash,
        event_id: "signed-0x45:2".to_owned(),
        consumed: false,
    })?;
    let second_retrieve_key = RetrieveKeyInput {
        binding_id: second_binding_id.to_owned(),
    };
    let second_retrieve_key_hash =
        pinned_sdk_input_hash::<RetrieveKeyInput, RetrieveKeyOutput>(&second_retrieve_key)?;
    let second_retrieve_key_nonce = [0x3b; 32];
    store.insert_grant(&GrantRecord {
        nonce: second_retrieve_key_nonce,
        binding_id: second_binding_id.to_owned(),
        operation: "retrieve-key".to_owned(),
        input_hash: second_retrieve_key_hash,
        event_id: "signed-0x45:3".to_owned(),
        consumed: false,
    })?;

    let signing_key = SigningKey::from_bytes(&[0x29; 32]);
    let allowed_key = hex::encode(signing_key.verifying_key().to_bytes());
    let config = json!({
        "signed_http": {
            "mode": "required",
            "allowed_leaders": {
                "version": 1,
                "leaders": [{
                    "leader_id": "integration-leader",
                    "keys": [{"kid": 0, "public_key": allowed_key}]
                }]
            },
            "tools": {
                "xyz.taluslabs.agent_api.query@1": {"replay_cache_ttl_ms": 60000},
                "xyz.taluslabs.agent_api.retrieve-key@1": {"replay_cache_ttl_ms": 60000}
            }
        }
    });
    std::fs::write(&toolkit_config, serde_json::to_vec(&config)?)?;
    let event_listener = TcpListener::bind("127.0.0.1:0")?;
    let event_address = event_listener.local_addr()?;
    drop(event_listener);
    let rpc_url = format!("http://{event_address}");
    let deployments = vec![
        DeploymentConfig {
            rpc_url: rpc_url.clone(),
            package_id: "0xabc".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x2::sui::SUI".to_owned(),
            cashier_id: "0x22".to_owned(),
            settlement_cap_id: "0xc1".to_owned(),
        },
        DeploymentConfig {
            rpc_url: rpc_url.clone(),
            package_id: "0xdef".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x123::test_coin::TEST_COIN".to_owned(),
            cashier_id: "0x23".to_owned(),
            settlement_cap_id: "0xc2".to_owned(),
        },
    ];
    let event_deployments = deployments.clone();
    let event_route = warp::post()
        .and(warp::body::json())
        .map(move |request: Value| {
            let package = request
                .pointer("/params/0/MoveEventModule/package")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let cursor = request
                .pointer("/params/1/eventSeq")
                .and_then(Value::as_str);
            let cursor_sequence = cursor.and_then(|sequence| sequence.parse::<u64>().ok());
            let config = event_deployments
                .iter()
                .find(|deployment| deployment.package_id == package);
            let (events, digest, next_sequence) = if let Some(deployment) = config {
                let (binding, wallet, owner, agent_uid, public_key, initial_last_sequence) =
                    if deployment.cashier_id == "0x22" {
                        (
                            "0x44",
                            "0x33",
                            "0x55",
                            "0x66",
                            PublicKey::from(&StaticSecret::from(owner_private_key))
                                .to_bytes()
                                .to_vec(),
                            4_u64,
                        )
                    } else {
                        (
                            "0x45",
                            "0x34",
                            "0x56",
                            "0x67",
                            PublicKey::from(&StaticSecret::from(second_owner_private_key))
                                .to_bytes()
                                .to_vec(),
                            3_u64,
                        )
                    };
                let digest = format!("signed-{binding}");
                let event_type = |name: &str| {
                    format!(
                        "{}::{}::{}<{}>",
                        deployment.package_id, deployment.module, name, deployment.coin_type
                    )
                };
                let (events, next_sequence) = if cursor_sequence.is_none() {
                    let mut events = vec![
                        rpc_event_json(
                            &digest,
                            0,
                            event_type("RegistrationEvent"),
                            json!({
                                "cashier_id": deployment.cashier_id,
                                "wallet_id": wallet,
                                "binding_id": binding,
                                "owner": owner,
                                "expected_agent_uid": agent_uid,
                                "rate": 1,
                                "export_enabled": true,
                                "owner_public_key": public_key
                            }),
                        ),
                        rpc_event_json(
                            &digest,
                            1,
                            event_type("ChargeEvent"),
                            json!({
                                "cashier_id": deployment.cashier_id,
                                "wallet_id": wallet,
                                "binding_id": binding,
                                "coin_units": 10,
                                "credit_units": 10,
                                "rate": 1
                            }),
                        ),
                    ];
                    if deployment.cashier_id == "0x22" {
                        events.extend([
                            rpc_event_json(
                                &digest,
                                2,
                                event_type("AuthorizationEvent"),
                                json!({
                                    "cashier_id": deployment.cashier_id,
                                    "binding_id": binding,
                                    "target_nonce": nonce,
                                    "input_hash": input_hash,
                                    "operation": b"query"
                                }),
                            ),
                            rpc_event_json(
                                &digest,
                                3,
                                event_type("AuthorizationEvent"),
                                json!({
                                    "cashier_id": deployment.cashier_id,
                                    "binding_id": binding,
                                    "target_nonce": successful_query_nonce,
                                    "input_hash": input_hash,
                                    "operation": b"query"
                                }),
                            ),
                            rpc_event_json(
                                &digest,
                                4,
                                event_type("AuthorizationEvent"),
                                json!({
                                    "cashier_id": deployment.cashier_id,
                                    "binding_id": binding,
                                    "target_nonce": retrieve_key_nonce,
                                    "input_hash": retrieve_key_hash,
                                    "operation": b"retrieve-key"
                                }),
                            ),
                        ]);
                    } else {
                        events.extend([
                            rpc_event_json(
                                &digest,
                                2,
                                event_type("AuthorizationEvent"),
                                json!({
                                    "cashier_id": deployment.cashier_id,
                                    "binding_id": binding,
                                    "target_nonce": second_query_nonce,
                                    "input_hash": second_query_hash,
                                    "operation": b"query"
                                }),
                            ),
                            rpc_event_json(
                                &digest,
                                3,
                                event_type("AuthorizationEvent"),
                                json!({
                                    "cashier_id": deployment.cashier_id,
                                    "binding_id": binding,
                                    "target_nonce": second_retrieve_key_nonce,
                                    "input_hash": second_retrieve_key_hash,
                                    "operation": b"retrieve-key"
                                }),
                            ),
                        ]);
                    }
                    (events, initial_last_sequence)
                } else if cursor_sequence == Some(initial_last_sequence) {
                    let sequence = initial_last_sequence + 1;
                    (
                        vec![rpc_event_json(
                            &digest,
                            sequence,
                            event_type("RevokedEvent"),
                            json!({
                                "cashier_id": deployment.cashier_id,
                                "wallet_id": wallet,
                                "binding_id": binding
                            }),
                        )],
                        sequence,
                    )
                } else if cursor_sequence == Some(initial_last_sequence + 1) {
                    let usage_sequence = initial_last_sequence + 2;
                    let refund_sequence = initial_last_sequence + 3;
                    (
                        vec![
                            rpc_event_json(
                                &digest,
                                usage_sequence,
                                event_type("UsageSettledEvent"),
                                json!({
                                    "cashier_id": deployment.cashier_id,
                                    "binding_id": binding,
                                    "cumulative_usage": 1,
                                    "coin_units_recognized": 1
                                }),
                            ),
                            rpc_event_json(
                                &digest,
                                refund_sequence,
                                event_type("RefundedEvent"),
                                json!({
                                    "cashier_id": deployment.cashier_id,
                                    "wallet_id": wallet,
                                    "binding_id": binding,
                                    "coin_units": 9
                                }),
                            ),
                        ],
                        refund_sequence,
                    )
                } else {
                    (Vec::new(), cursor_sequence.unwrap_or_default())
                };
                (events, digest, next_sequence.to_string())
            } else {
                (Vec::new(), "unconfigured".to_owned(), "0".to_owned())
            };
            warp::reply::json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "data": events,
                    "nextCursor": {"txDigest": digest, "eventSeq": next_sequence},
                    "hasNextPage": false
                }
            }))
        });
    let event_server = ServerGuard(tokio::spawn(warp::serve(event_route).run(event_address)));
    wait_for_http(&format!("http://{event_address}")).await?;
    let deployment_config = json!([
        {
            "rpc_url": rpc_url,
            "package_id": "0xabc",
            "module": "accounting",
            "coin_type": "0x2::sui::SUI",
            "cashier_id": "0x22",
            "settlement_cap_id": "0xc1"
        },
        {
            "rpc_url": rpc_url,
            "package_id": "0xdef",
            "module": "accounting",
            "coin_type": "0x123::test_coin::TEST_COIN",
            "cashier_id": "0x23",
            "settlement_cap_id": "0xc2"
        }
    ])
    .to_string();
    let mut backend = Backend::new(
        store.clone(),
        Arc::new(provider.clone()),
        master_key.clone(),
        deployments.clone(),
    )?;
    assert_eq!(backend.poll_configured_events().await?, 9);
    assert_eq!(store.binding(binding_id)?.unwrap().credit_units, 10);
    assert_eq!(store.binding(second_binding_id)?.unwrap().credit_units, 10);

    let tool_address = unused_address();
    let mut child = ChildGuard(spawn_tool(
        tool_address,
        &toolkit_config,
        &store_db,
        provider_address,
        &deployment_config,
    )?);
    wait_for_tool(&mut child.0, tool_address).await?;

    let signed_headers = sign_request("integration-leader", 0, input_hash, nonce, &signing_key);
    let query_b = QueryInput {
        binding_id: binding_id.to_owned(),
        payload: json!({"prompt": "B"}),
    };
    let substituted_body = resolved_transport(&query_b)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{tool_address}/agent-api/query/invoke"))
        .json(&substituted_body);
    for (name, value) in signed_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let response_body: serde_json::Value = response.json().await?;
    assert_eq!(response_body["error"], "input_integrity_error");
    assert_eq!(provider.final_usage(&provider_key).await?, 0);

    let successful_query_headers = sign_request(
        "integration-leader",
        0,
        input_hash,
        successful_query_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!("http://{tool_address}/agent-api/query/invoke"))
        .json(&resolved_transport(&query_a)?);
    for (name, value) in successful_query_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.nexus.canonical-tool-response+bcs")
    );
    let response_body = response.bytes().await?;
    let output: OffchainToolOutput = bcs::from_bytes(&response_body)?;
    assert_eq!(bcs::to_bytes(&output)?, response_body);
    assert_eq!(output.tag, b"ok");
    let result: serde_json::Value =
        serde_json::from_slice(&output_port_bytes(&output, b"result")?)?;
    let cumulative_usage: u64 =
        serde_json::from_slice(&output_port_bytes(&output, b"cumulative_usage")?)?;
    assert_eq!(result, json!({"accepted": true, "echo": {"prompt": "A"}}));
    assert_eq!(cumulative_usage, 1);
    assert_eq!(provider.final_usage(&provider_key).await?, 1);
    assert!(!String::from_utf8_lossy(&response_body).contains(&provider_key));
    let saved_query_response = response_body.clone();

    let second_query_headers = sign_request(
        "integration-leader",
        0,
        second_query_hash,
        second_query_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!("http://{tool_address}/agent-api/query/invoke"))
        .json(&resolved_transport(&second_query)?);
    for (name, value) in second_query_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let second_response_body = response.bytes().await?;
    let second_output: OffchainToolOutput = bcs::from_bytes(&second_response_body)?;
    assert_eq!(bcs::to_bytes(&second_output)?, second_response_body);
    assert_eq!(second_output.tag, b"ok");
    let second_result: serde_json::Value =
        serde_json::from_slice(&output_port_bytes(&second_output, b"result")?)?;
    let second_usage: u64 =
        serde_json::from_slice(&output_port_bytes(&second_output, b"cumulative_usage")?)?;
    assert_eq!(
        second_result,
        json!({"accepted": true, "echo": {"prompt": "second deployment"}})
    );
    assert_eq!(second_usage, 1);
    assert_eq!(provider.final_usage(&second_provider_key).await?, 1);
    assert!(!String::from_utf8_lossy(&second_response_body).contains(&second_provider_key));
    let saved_second_query_response = second_response_body.clone();

    let initial_retrieve_key_headers = sign_request(
        "integration-leader",
        0,
        retrieve_key_hash,
        retrieve_key_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!(
            "http://{tool_address}/agent-api/retrieve-key/invoke"
        ))
        .json(&resolved_transport(&retrieve_key)?);
    for (name, value) in initial_retrieve_key_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let initial_key_response = response.bytes().await?;
    assert!(!String::from_utf8_lossy(&initial_key_response).contains(&provider_key));
    let initial_key_output: OffchainToolOutput = bcs::from_bytes(&initial_key_response)?;
    let initial_key_envelope: KeyEnvelope =
        serde_json::from_slice(&output_port_bytes(&initial_key_output, b"envelope")?)?;
    assert_eq!(
        decrypt_export(&owner_private_key, &initial_key_envelope)?,
        provider_key.as_bytes()
    );

    let initial_second_key_headers = sign_request(
        "integration-leader",
        0,
        second_retrieve_key_hash,
        second_retrieve_key_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!(
            "http://{tool_address}/agent-api/retrieve-key/invoke"
        ))
        .json(&resolved_transport(&second_retrieve_key)?);
    for (name, value) in initial_second_key_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let initial_second_key_response = response.bytes().await?;
    assert!(!String::from_utf8_lossy(&initial_second_key_response).contains(&second_provider_key));
    let initial_second_key_output: OffchainToolOutput =
        bcs::from_bytes(&initial_second_key_response)?;
    let initial_second_key_envelope: KeyEnvelope =
        serde_json::from_slice(&output_port_bytes(&initial_second_key_output, b"envelope")?)?;
    assert_eq!(
        decrypt_export(&second_owner_private_key, &initial_second_key_envelope)?,
        second_provider_key.as_bytes()
    );

    child.0.kill()?;
    child.0.wait()?;
    assert_eq!(backend.poll_configured_events().await?, 2);
    assert_eq!(store.binding(binding_id)?.unwrap().status, "revoked");
    assert_eq!(store.binding(second_binding_id)?.unwrap().status, "revoked");
    assert_eq!(provider.final_usage(&provider_key).await?, 1);
    assert_eq!(provider.final_usage(&second_provider_key).await?, 1);
    let pending = store.pending_settlements()?;
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|settlement| {
        settlement.final_usage == 1 && settlement.refundable_coin_units == 9
    }));

    drop(backend);
    drop(store);
    store = Store::open(&store_db)?;
    backend = Backend::new(
        store.clone(),
        Arc::new(provider.clone()),
        master_key.clone(),
        deployments.clone(),
    )?;
    assert_eq!(backend.poll_configured_events().await?, 4);
    for binding in [binding_id, second_binding_id] {
        let refunded = store.binding(binding)?.unwrap();
        assert_eq!(refunded.status, "refunded");
        assert!(refunded.refunded);
        assert_eq!(refunded.recognized_coin_units, 1);
    }
    assert!(store.pending_settlements()?.is_empty());

    provider_server.stop().await;
    child.0 = spawn_tool(
        tool_address,
        &toolkit_config,
        &store_db,
        provider_address,
        &deployment_config,
    )?;
    wait_for_http(&format!("http://{tool_address}/health")).await?;

    let query_replay_headers = sign_request(
        "integration-leader",
        0,
        input_hash,
        successful_query_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!("http://{tool_address}/agent-api/query/invoke"))
        .json(&resolved_transport(&query_a)?);
    for (name, value) in query_replay_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.bytes().await?, saved_query_response);

    let second_query_replay_headers = sign_request(
        "integration-leader",
        0,
        second_query_hash,
        second_query_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!("http://{tool_address}/agent-api/query/invoke"))
        .json(&resolved_transport(&second_query)?);
    for (name, value) in second_query_replay_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.bytes().await?, saved_second_query_response);

    let retrieve_key_headers = sign_request(
        "integration-leader",
        0,
        retrieve_key_hash,
        retrieve_key_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!(
            "http://{tool_address}/agent-api/retrieve-key/invoke"
        ))
        .json(&resolved_transport(&retrieve_key)?);
    for (name, value) in retrieve_key_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response_body = response.bytes().await?;
    assert_eq!(response_body, initial_key_response);
    assert!(!String::from_utf8_lossy(&response_body).contains(&provider_key));
    let output: OffchainToolOutput = bcs::from_bytes(&response_body)?;
    assert_eq!(bcs::to_bytes(&output)?, response_body);
    assert_eq!(output.tag, b"ok");
    let envelope: KeyEnvelope = serde_json::from_slice(&output_port_bytes(&output, b"envelope")?)?;
    assert_eq!(
        decrypt_export(&owner_private_key, &envelope)?,
        provider_key.as_bytes()
    );

    let second_retrieve_key_headers = sign_request(
        "integration-leader",
        0,
        second_retrieve_key_hash,
        second_retrieve_key_nonce,
        &signing_key,
    );
    let mut request = reqwest::Client::new()
        .post(format!(
            "http://{tool_address}/agent-api/retrieve-key/invoke"
        ))
        .json(&resolved_transport(&second_retrieve_key)?);
    for (name, value) in second_retrieve_key_headers.to_pairs() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response_body = response.bytes().await?;
    assert_eq!(response_body, initial_second_key_response);
    assert!(!String::from_utf8_lossy(&response_body).contains(&second_provider_key));
    let output: OffchainToolOutput = bcs::from_bytes(&response_body)?;
    assert_eq!(bcs::to_bytes(&output)?, response_body);
    assert_eq!(output.tag, b"ok");
    let envelope: KeyEnvelope = serde_json::from_slice(&output_port_bytes(&output, b"envelope")?)?;
    assert_eq!(
        decrypt_export(&second_owner_private_key, &envelope)?,
        second_provider_key.as_bytes()
    );

    drop(child);
    event_server.stop().await;
    drop(backend);
    drop(store);
    std::fs::remove_dir_all(temp)?;
    Ok(())
}
