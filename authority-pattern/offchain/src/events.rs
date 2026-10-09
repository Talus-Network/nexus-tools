use {
    anyhow::{anyhow, bail, Context, Result},
    nexus_sdk::sui::types::{Address, TypeTag},
    serde::{Deserialize, Serialize},
    serde_json::{json, Value},
    std::str::FromStr,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFilter {
    pub rpc_url: String,
    pub package_id: String,
    pub module: String,
    pub coin_type: String,
    pub cashier_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventOrigin {
    pub package_id: String,
    pub module: String,
    pub coin_type: String,
    pub cashier_id: String,
}

impl EventOrigin {
    pub fn from_filter(filter: &EventFilter) -> Result<Self> {
        let package_id = Address::from_str(&filter.package_id)
            .context("configured event package address is malformed")?
            .to_string();
        let cashier_id = Address::from_str(&filter.cashier_id)
            .context("configured event cashier address is malformed")?
            .to_string();
        let coin_type = TypeTag::from_str(&filter.coin_type)
            .context("configured event payment coin type is malformed")?
            .to_string();
        Ok(Self {
            package_id,
            module: filter.module.clone(),
            coin_type,
            cashier_id,
        })
    }

    pub fn matches(&self, other: &Self) -> bool {
        let Ok(self_coin_type) = TypeTag::from_str(&self.coin_type) else {
            return false;
        };
        let Ok(other_coin_type) = TypeTag::from_str(&other.coin_type) else {
            return false;
        };
        canonical_address(&self.package_id) == canonical_address(&other.package_id)
            && self.module == other.module
            && self_coin_type == other_coin_type
            && canonical_address(&self.cashier_id) == canonical_address(&other.cashier_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEvent {
    pub event_id: String,
    pub type_tag: String,
    pub fields: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AgentApiEvent {
    Registered {
        cashier_id: String,
        wallet_id: String,
        binding_id: String,
        owner: String,
        expected_agent_uid: String,
        rate: u64,
        export_enabled: bool,
        owner_public_key: Vec<u8>,
    },
    Charged {
        cashier_id: String,
        wallet_id: String,
        binding_id: String,
        coin_units: u64,
        credit_units: u64,
        rate: u64,
    },
    Authorized {
        cashier_id: String,
        binding_id: String,
        target_nonce: [u8; 32],
        operation: String,
        input_hash: [u8; 32],
    },
    Revoked {
        cashier_id: String,
        wallet_id: String,
        binding_id: String,
    },
    UsageSettled {
        cashier_id: String,
        binding_id: String,
        cumulative_usage: u64,
        coin_units_recognized: u64,
    },
    Refunded {
        cashier_id: String,
        wallet_id: String,
        binding_id: String,
        coin_units: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedEvent {
    pub event_id: String,
    pub type_tag: String,
    pub event: AgentApiEvent,
}

impl DecodedEvent {
    pub fn origin(&self) -> Result<EventOrigin> {
        let (package_id, module, _, coin_type) = parse_event_type(&self.type_tag)?;
        Ok(EventOrigin {
            package_id: canonical_address(&package_id),
            module,
            coin_type: coin_type.to_string(),
            cashier_id: canonical_address(event_cashier_id(&self.event)),
        })
    }
}

fn event_cashier_id(event: &AgentApiEvent) -> &str {
    match event {
        AgentApiEvent::Registered { cashier_id, .. }
        | AgentApiEvent::Charged { cashier_id, .. }
        | AgentApiEvent::Authorized { cashier_id, .. }
        | AgentApiEvent::Revoked { cashier_id, .. }
        | AgentApiEvent::UsageSettled { cashier_id, .. }
        | AgentApiEvent::Refunded { cashier_id, .. } => cashier_id,
    }
}

pub fn decode_event(filter: &EventFilter, event: ChainEvent) -> Result<DecodedEvent> {
    let (module_package, module_name, struct_name, actual_coin) =
        parse_event_type(&event.type_tag)?;
    if canonical_address(&module_package) != canonical_address(&filter.package_id)
        || module_name != filter.module
    {
        bail!("event package or module is not configured");
    }
    let expected_coin = TypeTag::from_str(&filter.coin_type)
        .context("configured event payment coin type is malformed")?;
    if actual_coin != expected_coin {
        bail!("event payment coin type does not match configuration");
    }

    let fields = &event.fields;
    let cashier_id = id_field(fields, "cashier_id")?;
    if canonical_address(&cashier_id) != canonical_address(&filter.cashier_id) {
        bail!("event cashier does not match configuration");
    }
    let decoded = match struct_name.as_str() {
        "RegistrationEvent" => {
            let export_enabled = bool_field(fields, "export_enabled")?;
            let owner_public_key = bytes_field(fields, "owner_public_key")?;
            if (export_enabled && owner_public_key.len() != 32)
                || (!export_enabled && !owner_public_key.is_empty())
            {
                bail!("registration export key does not match its policy");
            }
            AgentApiEvent::Registered {
                cashier_id,
                wallet_id: id_field(fields, "wallet_id")?,
                binding_id: id_field(fields, "binding_id")?,
                owner: id_field(fields, "owner")?,
                expected_agent_uid: id_field(fields, "expected_agent_uid")?,
                rate: u64_field(fields, "rate")?,
                export_enabled,
                owner_public_key,
            }
        }
        "ChargeEvent" => AgentApiEvent::Charged {
            cashier_id,
            wallet_id: id_field(fields, "wallet_id")?,
            binding_id: id_field(fields, "binding_id")?,
            coin_units: u64_field(fields, "coin_units")?,
            credit_units: u64_field(fields, "credit_units")?,
            rate: u64_field(fields, "rate")?,
        },
        "AuthorizationEvent" => {
            let operation = String::from_utf8(bytes_field(fields, "operation")?)
                .context("authorization operation is not UTF-8")?;
            if operation != "query" && operation != "retrieve-key" {
                bail!("authorization operation is unsupported");
            }
            AgentApiEvent::Authorized {
                cashier_id,
                binding_id: id_field(fields, "binding_id")?,
                target_nonce: array32(bytes_field(fields, "target_nonce")?)?,
                operation,
                input_hash: array32(bytes_field(fields, "input_hash")?)?,
            }
        }
        "RevokedEvent" => AgentApiEvent::Revoked {
            cashier_id,
            wallet_id: id_field(fields, "wallet_id")?,
            binding_id: id_field(fields, "binding_id")?,
        },
        "UsageSettledEvent" => AgentApiEvent::UsageSettled {
            cashier_id,
            binding_id: id_field(fields, "binding_id")?,
            cumulative_usage: u64_field(fields, "cumulative_usage")?,
            coin_units_recognized: u64_field(fields, "coin_units_recognized")?,
        },
        "RefundedEvent" => AgentApiEvent::Refunded {
            cashier_id,
            wallet_id: id_field(fields, "wallet_id")?,
            binding_id: id_field(fields, "binding_id")?,
            coin_units: u64_field(fields, "coin_units")?,
        },
        _ => bail!("event type is not an Agent API lifecycle event"),
    };
    Ok(DecodedEvent {
        event_id: event.event_id,
        type_tag: event.type_tag,
        event: decoded,
    })
}

fn parse_event_type(type_tag: &str) -> Result<(String, String, String, TypeTag)> {
    let (module_path, generic_with_close) = type_tag
        .split_once('<')
        .ok_or_else(|| anyhow!("generic event type is required"))?;
    let generic = generic_with_close
        .strip_suffix('>')
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("event coin type argument is malformed"))?;
    let coin_type = TypeTag::from_str(generic).context("event coin type argument is malformed")?;
    let mut parts = module_path.split("::");
    let package = parts
        .next()
        .ok_or_else(|| anyhow!("event package is missing"))?;
    let module = parts
        .next()
        .ok_or_else(|| anyhow!("event module is missing"))?;
    let name = parts
        .next()
        .ok_or_else(|| anyhow!("event struct is missing"))?;
    if parts.next().is_some() {
        bail!("event type has an unexpected module path");
    }
    Ok((
        package.to_owned(),
        module.to_owned(),
        name.to_owned(),
        coin_type,
    ))
}

fn canonical_address(value: &str) -> String {
    let trimmed = value.trim().to_ascii_lowercase();
    let digits = trimmed
        .strip_prefix("0x")
        .unwrap_or(&trimmed)
        .trim_start_matches('0');
    format!("0x{}", if digits.is_empty() { "0" } else { digits })
}

fn id_field(fields: &Value, name: &str) -> Result<String> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("event field {name} is missing or malformed"))
}

fn bool_field(fields: &Value, name: &str) -> Result<bool> {
    fields
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("event field {name} is missing or malformed"))
}

fn u64_field(fields: &Value, name: &str) -> Result<u64> {
    let value = fields
        .get(name)
        .ok_or_else(|| anyhow!("event field {name} is missing"))?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|string| string.parse().ok()))
        .ok_or_else(|| anyhow!("event field {name} is malformed"))
}

fn bytes_field(fields: &Value, name: &str) -> Result<Vec<u8>> {
    let values = fields
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("event field {name} must be a byte array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| u8::try_from(number).ok())
                .ok_or_else(|| anyhow!("event field {name} contains an invalid byte"))
        })
        .collect()
}

fn array32(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| anyhow!("event hash or nonce must contain 32 bytes"))
}

#[derive(Clone)]
pub struct SuiEventSource {
    client: reqwest::Client,
    filter: EventFilter,
    source_name: String,
}

#[derive(Debug, Clone)]
pub struct EventPage {
    pub events: Vec<ChainEvent>,
    pub next_cursor: Option<Value>,
    pub has_next_page: bool,
}

impl SuiEventSource {
    pub fn new(filter: EventFilter) -> Result<Self> {
        let source_name = format!(
            "{}:{}:{}:{}",
            filter.package_id, filter.module, filter.coin_type, filter.cashier_id
        );
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
            filter,
            source_name,
        })
    }

    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    pub fn filter(&self) -> &EventFilter {
        &self.filter
    }

    pub async fn fetch_page(&self, cursor: Option<Value>) -> Result<EventPage> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "suix_queryEvents",
            "params": [
                { "MoveEventModule": { "package": self.filter.package_id, "module": self.filter.module } },
                cursor,
                100,
                false
            ]
        });
        let response = self
            .client
            .post(&self.filter.rpc_url)
            .json(&request)
            .send()
            .await
            .context("Sui event query failed")?;
        if !response.status().is_success() {
            bail!("Sui event query returned an HTTP error");
        }
        let body: Value = response
            .json()
            .await
            .context("Sui event response was malformed")?;
        if let Some(error) = body.get("error") {
            bail!(
                "Sui event query failed: {}",
                error
                    .get("code")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
            );
        }
        let result = body
            .get("result")
            .ok_or_else(|| anyhow!("Sui event response has no result"))?;
        let data = result
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Sui event response has no event list"))?;
        let events = data
            .iter()
            .map(parse_rpc_event)
            .collect::<Result<Vec<_>>>()?;
        Ok(EventPage {
            events,
            next_cursor: result
                .get("nextCursor")
                .cloned()
                .filter(|value| !value.is_null()),
            has_next_page: result
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct RpcEvent {
    id: RpcEventId,
    #[serde(rename = "type")]
    type_tag: String,
    #[serde(rename = "parsedJson")]
    fields: Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct RpcEventId {
    #[serde(rename = "txDigest")]
    tx_digest: String,
    #[serde(rename = "eventSeq")]
    event_seq: String,
}

fn parse_rpc_event(value: &Value) -> Result<ChainEvent> {
    let event: RpcEvent =
        serde_json::from_value(value.clone()).context("Sui event entry was malformed")?;
    Ok(ChainEvent {
        event_id: format!("{}:{}", event.id.tx_digest, event.id.event_seq),
        type_tag: event.type_tag,
        fields: event.fields,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{
            net::TcpListener,
            sync::{Arc, Mutex},
            time::Duration,
        },
        warp::Filter,
    };

    fn filter() -> EventFilter {
        EventFilter {
            rpc_url: "http://127.0.0.1:9000".to_owned(),
            package_id: "0xabc".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x2::sui::SUI".to_owned(),
            cashier_id: "0x11".to_owned(),
        }
    }

    #[test]
    fn decoder_accepts_only_the_configured_package_module_coin_and_cashier() {
        let event = ChainEvent {
            event_id: "tx:1".to_owned(),
            type_tag: "0x000abc::accounting::AuthorizationEvent<0x2::sui::SUI>".to_owned(),
            fields: json!({
                "cashier_id": "0x11", "binding_id": "0x22",
                "target_nonce": vec![7; 32], "input_hash": vec![8; 32], "operation": [113,117,101,114,121]
            }),
        };
        let decoded = decode_event(&filter(), event.clone()).unwrap();
        assert!(
            matches!(decoded.event, AgentApiEvent::Authorized { operation, .. } if operation == "query")
        );
        let wrong_cashier = ChainEvent {
            fields: json!({
                "cashier_id": "0x12", "binding_id": "0x22",
                "target_nonce": vec![7; 32], "input_hash": vec![8; 32], "operation": [113,117,101,114,121]
            }),
            ..event.clone()
        };
        assert!(decode_event(&filter(), wrong_cashier).is_err());
        let wrong_coin = ChainEvent {
            type_tag: "0xabc::accounting::AuthorizationEvent<0x2::other::COIN>".to_owned(),
            ..event
        };
        assert!(decode_event(&filter(), wrong_coin).is_err());
    }

    #[test]
    fn decoder_rejects_malformed_nonce_and_unapproved_export_key() {
        let malformed_nonce = ChainEvent {
            event_id: "tx:1".to_owned(),
            type_tag: "0xabc::accounting::AuthorizationEvent<0x2::sui::SUI>".to_owned(),
            fields: json!({"cashier_id":"0x11","binding_id":"0x22","target_nonce":[1],"input_hash":vec![8;32],"operation":[113,117,101,114,121]}),
        };
        assert!(decode_event(&filter(), malformed_nonce).is_err());
        let registration = ChainEvent {
            event_id: "tx:2".to_owned(),
            type_tag: "0xabc::accounting::RegistrationEvent<0x2::sui::SUI>".to_owned(),
            fields: json!({"cashier_id":"0x11","wallet_id":"0x3","binding_id":"0x4","owner":"0x5","expected_agent_uid":"0x6","rate":2,"export_enabled":true,"owner_public_key":[1]}),
        };
        assert!(decode_event(&filter(), registration).is_err());
    }

    #[test]
    fn decoder_accepts_nested_coin_types_with_canonicalized_addresses() {
        let mut filter = filter();
        filter.coin_type = "0x123::test_coin::Wrapper<vector<0x2::sui::SUI>>".to_owned();
        let event = ChainEvent {
            event_id: "tx:3".to_owned(),
            type_tag: "0x000abc::accounting::ChargeEvent<0x0000000000000000000000000000000000000000000000000000000000000123::test_coin::Wrapper<vector<0x2::sui::SUI>>>".to_owned(),
            fields: json!({
                "cashier_id": "0x11", "wallet_id": "0x22", "binding_id": "0x33",
                "coin_units": 1, "credit_units": 2, "rate": 2
            }),
        };

        assert!(decode_event(&filter, event).is_ok());
    }

    #[tokio::test]
    async fn sui_event_source_queries_move_event_module_and_preserves_registration_cursor() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let route = warp::post()
            .and(warp::body::json())
            .map(move |request: Value| {
                captured.lock().unwrap().push(request);
                warp::reply::json(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "data": [{
                            "id": {"txDigest": "registration-tx", "eventSeq": "0"},
                            "type": "0xabc::accounting::RegistrationEvent<0x2::sui::SUI>",
                            "parsedJson": {
                                "cashier_id": "0x11",
                                "wallet_id": "0x3",
                                "binding_id": "0x4",
                                "owner": "0x5",
                                "expected_agent_uid": "0x6",
                                "rate": 2,
                                "export_enabled": true,
                                "owner_public_key": vec![7; 32]
                            }
                        }],
                        "nextCursor": {"txDigest": "registration-tx", "eventSeq": "0"},
                        "hasNextPage": true
                    }
                }))
            });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(route).run(address));
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let filter = EventFilter {
            rpc_url: format!("http://{address}"),
            ..filter()
        };
        let source = SuiEventSource::new(filter.clone()).unwrap();
        let page = source.fetch_page(None).await.unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event_id, "registration-tx:0");
        assert_eq!(
            page.next_cursor,
            Some(json!({"txDigest": "registration-tx", "eventSeq": "0"}))
        );
        assert!(page.has_next_page);
        let decoded = decode_event(&filter, page.events[0].clone()).unwrap();
        assert_eq!(
            decoded.event,
            AgentApiEvent::Registered {
                cashier_id: "0x11".to_owned(),
                wallet_id: "0x3".to_owned(),
                binding_id: "0x4".to_owned(),
                owner: "0x5".to_owned(),
                expected_agent_uid: "0x6".to_owned(),
                rate: 2,
                export_enabled: true,
                owner_public_key: vec![7; 32],
            }
        );
        let next_page = source.fetch_page(page.next_cursor.clone()).await.unwrap();
        assert_eq!(next_page.events.len(), 1);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request["method"], "suix_queryEvents");
            assert_eq!(request["params"][0]["MoveEventModule"]["package"], "0xabc");
            assert_eq!(
                request["params"][0]["MoveEventModule"]["module"],
                "accounting"
            );
            assert!(request["params"][0].get("MoveModule").is_none());
            assert_eq!(request["params"][2], 100);
            assert_eq!(request["params"][3], false);
        }
        assert!(requests[0]["params"][1].is_null());
        assert_eq!(requests[1]["params"][1], page.next_cursor.unwrap());
        server.abort();
    }
}
