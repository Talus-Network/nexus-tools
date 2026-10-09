use {
    async_trait::async_trait,
    hmac::{Hmac, Mac},
    reqwest::{header, StatusCode},
    rusqlite::{params, Connection, OptionalExtension, TransactionBehavior},
    serde::{de::DeserializeOwned, Deserialize, Serialize},
    serde_json::{json, Value},
    sha2::{Digest, Sha256},
    std::{
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    },
    thiserror::Error,
    warp::{http::HeaderMap, Filter, Rejection, Reply},
};

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider service is unavailable")]
    Unavailable,
    #[error("provider rejected the operation")]
    Rejected,
    #[error("provider does not support required idempotency semantics")]
    IdempotencyUnsupported,
    #[error("provider response is malformed")]
    Malformed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderQueryResult {
    pub result: Value,
    pub cumulative_usage: u64,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn verify_capabilities(&self) -> Result<(), ProviderError>;
    async fn create_key(&self, binding_id: &str) -> Result<String, ProviderError>;
    async fn credit(
        &self,
        key: &str,
        credit_units: u64,
        idempotency_key: &str,
    ) -> Result<(), ProviderError>;
    async fn query(
        &self,
        key: &str,
        payload: &Value,
        idempotency_key: &str,
    ) -> Result<ProviderQueryResult, ProviderError>;
    async fn disable(&self, key: &str) -> Result<(), ProviderError>;
    async fn final_usage(&self, key: &str) -> Result<u64, ProviderError>;
}

#[derive(Clone)]
pub struct HttpProvider {
    base_url: String,
    operator_key: Arc<String>,
    client: reqwest::Client,
}

impl HttpProvider {
    pub fn new(
        base_url: impl Into<String>,
        operator_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Self::with_timeout(base_url, operator_key, Duration::from_secs(10))
    }

    fn with_timeout(
        base_url: impl Into<String>,
        operator_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|_| ProviderError::Unavailable)?;
        Ok(Self {
            base_url,
            operator_key: Arc::new(operator_key.into()),
            client,
        })
    }

    async fn operator_post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ProviderError> {
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(self.operator_key.as_str())
            .json(body)
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        decode_response(response).await
    }

    async fn key_post<T: DeserializeOwned>(
        &self,
        path: &str,
        key: &str,
        body: &Value,
    ) -> Result<T, ProviderError> {
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(key)
            .json(body)
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        decode_response(response).await
    }

    pub async fn mock_remaining_credit_units(&self, key: &str) -> Result<u64, ProviderError> {
        let response: UsageResponse = self
            .operator_post("/v1/operator/keys/usage", &json!({"key": key}))
            .await?;
        response
            .remaining_credit_units
            .ok_or(ProviderError::Malformed)
    }
}

#[derive(Deserialize)]
struct Capabilities {
    idempotency: bool,
    key_creation: bool,
    credit: bool,
    disable: bool,
    usage: bool,
}

#[derive(Deserialize)]
struct KeyResponse {
    key: String,
}

#[derive(Deserialize)]
struct UsageResponse {
    cumulative_usage: u64,
    #[serde(default)]
    remaining_credit_units: Option<u64>,
}

#[derive(Deserialize)]
struct QueryResponse {
    result: Value,
    cumulative_usage: u64,
}

#[derive(Deserialize)]
struct EmptyResponse {}

#[async_trait]
impl Provider for HttpProvider {
    async fn verify_capabilities(&self) -> Result<(), ProviderError> {
        let response = self
            .client
            .get(format!("{}/v1/capabilities", self.base_url))
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        let capabilities: Capabilities = decode_response(response).await?;
        if !(capabilities.idempotency
            && capabilities.key_creation
            && capabilities.credit
            && capabilities.disable
            && capabilities.usage)
        {
            return Err(ProviderError::IdempotencyUnsupported);
        }
        Ok(())
    }

    async fn create_key(&self, binding_id: &str) -> Result<String, ProviderError> {
        let response: KeyResponse = self
            .operator_post(
                "/v1/operator/keys/create",
                &json!({"binding_id": binding_id}),
            )
            .await?;
        if response.key.is_empty() {
            return Err(ProviderError::Malformed);
        }
        Ok(response.key)
    }

    async fn credit(
        &self,
        key: &str,
        credit_units: u64,
        idempotency_key: &str,
    ) -> Result<(), ProviderError> {
        let _: EmptyResponse = self
            .operator_post(
                "/v1/operator/keys/credit",
                &json!({
                    "key": key, "credit_units": credit_units, "idempotency_key": idempotency_key
                }),
            )
            .await?;
        Ok(())
    }

    async fn query(
        &self,
        key: &str,
        payload: &Value,
        idempotency_key: &str,
    ) -> Result<ProviderQueryResult, ProviderError> {
        let response: QueryResponse = self
            .key_post(
                "/v1/query",
                key,
                &json!({
                    "payload": payload, "idempotency_key": idempotency_key
                }),
            )
            .await?;
        Ok(ProviderQueryResult {
            result: response.result,
            cumulative_usage: response.cumulative_usage,
        })
    }

    async fn disable(&self, key: &str) -> Result<(), ProviderError> {
        let _: EmptyResponse = self
            .operator_post("/v1/operator/keys/disable", &json!({"key": key}))
            .await?;
        Ok(())
    }

    async fn final_usage(&self, key: &str) -> Result<u64, ProviderError> {
        let response: UsageResponse = self
            .operator_post("/v1/operator/keys/usage", &json!({"key": key}))
            .await?;
        Ok(response.cumulative_usage)
    }
}

async fn decode_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ProviderError> {
    if !response.status().is_success() {
        return Err(match response.status() {
            StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::PAYMENT_REQUIRED
            | StatusCode::CONFLICT => ProviderError::Rejected,
            _ => ProviderError::Unavailable,
        });
    }
    response.json().await.map_err(|_| ProviderError::Malformed)
}

#[derive(Clone)]
struct MockState {
    connection: Arc<Mutex<Connection>>,
    operator_key: Arc<String>,
}

pub fn mock_routes(
    database_path: impl AsRef<Path>,
    operator_key: String,
) -> Result<
    impl Filter<Extract = (warp::reply::Response,), Error = Rejection> + Clone + 'static,
    rusqlite::Error,
> {
    let connection = Connection::open(database_path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS provider_keys (
             key_hash TEXT PRIMARY KEY,
             binding_id TEXT NOT NULL UNIQUE,
             credit_units TEXT NOT NULL DEFAULT '0',
             cumulative_usage TEXT NOT NULL DEFAULT '0',
             active INTEGER NOT NULL DEFAULT 1
         );
         CREATE TABLE IF NOT EXISTS provider_credits (
             key_hash TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             credit_units TEXT NOT NULL,
             PRIMARY KEY(key_hash, idempotency_key)
         );
         CREATE TABLE IF NOT EXISTS provider_queries (
             key_hash TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             request_hash TEXT NOT NULL,
             response_json TEXT NOT NULL,
             cumulative_usage TEXT NOT NULL,
             PRIMARY KEY(key_hash, idempotency_key)
         );",
    )?;
    let state = MockState {
        connection: Arc::new(Mutex::new(connection)),
        operator_key: Arc::new(operator_key),
    };
    let capabilities = warp::get().and(warp::path!("v1" / "capabilities")).map(|| {
        warp::reply::json(&json!({
            "idempotency": true,
            "key_creation": true,
            "credit": true,
            "disable": true,
            "usage": true
        }))
        .into_response()
    });
    let health = warp::get()
        .and(warp::path("health"))
        .and(warp::path::end())
        .map(|| warp::reply::json(&json!({"status": "ok"})).into_response());
    let dispatch = warp::any()
        .map(move || state.clone())
        .and(warp::method())
        .and(warp::path::full())
        .and(warp::header::headers_cloned())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::bytes())
        .and_then(mock_dispatch);
    Ok(capabilities.or(health).unify().or(dispatch).unify())
}

async fn mock_dispatch(
    state: MockState,
    method: warp::http::Method,
    path: warp::path::FullPath,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<warp::reply::Response, Rejection> {
    let result = dispatch(&state, &method, path.as_str(), &headers, &body);
    let (status, value) = match result {
        Ok(value) => (StatusCode::OK, value),
        Err((status, code)) => (status, json!({"error": code})),
    };
    let response = warp::reply::with_status(warp::reply::json(&value), status).into_response();
    Ok(response)
}

fn dispatch(
    state: &MockState,
    method: &warp::http::Method,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Value, (StatusCode, &'static str)> {
    if method == warp::http::Method::GET && path == "/v1/capabilities" {
        return Ok(
            json!({"idempotency": true, "key_creation": true, "credit": true, "disable": true, "usage": true}),
        );
    }
    if method == warp::http::Method::GET && path == "/health" {
        return Ok(json!({"status":"ok"}));
    }
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid_request"))?;
    let bearer = bearer_token(headers).ok_or((StatusCode::UNAUTHORIZED, "unauthorized"))?;
    match (method.as_str(), path) {
        ("POST", "/v1/operator/keys/create") => {
            require_operator(state, bearer)?;
            let binding_id = payload
                .get("binding_id")
                .and_then(Value::as_str)
                .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))?;
            let key = derive_mock_key(state.operator_key.as_bytes(), binding_id);
            let key_hash = hash_key(&key);
            let connection = state
                .connection
                .lock()
                .expect("mock provider database mutex poisoned");
            connection
                .execute(
                    "INSERT OR IGNORE INTO provider_keys (key_hash, binding_id) VALUES (?1, ?2)",
                    params![key_hash, binding_id],
                )
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
            let stored_hash: String = connection
                .query_row(
                    "SELECT key_hash FROM provider_keys WHERE binding_id = ?1",
                    [binding_id],
                    |row| row.get(0),
                )
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
            if stored_hash != key_hash {
                return Err((StatusCode::CONFLICT, "binding_conflict"));
            }
            Ok(json!({"key": key}))
        }
        ("POST", "/v1/operator/keys/credit") => {
            require_operator(state, bearer)?;
            let key = payload
                .get("key")
                .and_then(Value::as_str)
                .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))?;
            let amount = number(&payload, "credit_units")?;
            let idempotency = payload
                .get("idempotency_key")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))?;
            provider_credit(state, key, amount, idempotency)?;
            Ok(json!({}))
        }
        ("POST", "/v1/operator/keys/disable") => {
            require_operator(state, bearer)?;
            let key = payload
                .get("key")
                .and_then(Value::as_str)
                .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))?;
            with_key_update(state, key, |transaction, key_hash| {
                transaction
                    .execute(
                        "UPDATE provider_keys
                         SET active = 0, credit_units = cumulative_usage
                         WHERE key_hash = ?1",
                        [key_hash],
                    )
                    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
                Ok(())
            })?;
            Ok(json!({}))
        }
        ("POST", "/v1/operator/keys/usage") => {
            require_operator(state, bearer)?;
            let key = payload
                .get("key")
                .and_then(Value::as_str)
                .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))?;
            let usage = read_usage(state, key)?;
            Ok(json!({
                "cumulative_usage": usage.cumulative_usage,
                "remaining_credit_units": usage.remaining_credit_units
            }))
        }
        ("POST", "/v1/query") => {
            let payload_value = payload
                .get("payload")
                .cloned()
                .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))?;
            let idempotency = payload
                .get("idempotency_key")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))?;
            provider_query(state, bearer, &payload_value, idempotency)
        }
        _ => Err((StatusCode::NOT_FOUND, "not_found")),
    }
}

fn provider_credit(
    state: &MockState,
    key: &str,
    amount: u64,
    idempotency: &str,
) -> Result<(), (StatusCode, &'static str)> {
    let key_hash = hash_key(key);
    let mut connection = state
        .connection
        .lock()
        .expect("mock provider database mutex poisoned");
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    let account = transaction
        .query_row(
            "SELECT credit_units, active FROM provider_keys WHERE key_hash = ?1",
            [&key_hash],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?
        .ok_or((StatusCode::NOT_FOUND, "key_not_found"))?;
    let previous = transaction.query_row("SELECT credit_units FROM provider_credits WHERE key_hash = ?1 AND idempotency_key = ?2", params![key_hash, idempotency], |row| row.get::<_, String>(0))
        .optional().map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    if let Some(previous) = previous {
        if previous != amount.to_string() {
            return Err((StatusCode::CONFLICT, "idempotency_conflict"));
        }
        transaction
            .commit()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
        return Ok(());
    }
    if !account.1 {
        return Err((StatusCode::CONFLICT, "key_disabled"));
    }
    let total = account
        .0
        .parse::<u64>()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?
        .checked_add(amount)
        .ok_or((StatusCode::CONFLICT, "balance_overflow"))?;
    transaction.execute("INSERT INTO provider_credits (key_hash, idempotency_key, credit_units) VALUES (?1, ?2, ?3)", params![key_hash, idempotency, amount.to_string()])
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    transaction
        .execute(
            "UPDATE provider_keys SET credit_units = ?2 WHERE key_hash = ?1",
            params![key_hash, total.to_string()],
        )
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    transaction
        .commit()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    Ok(())
}

fn provider_query(
    state: &MockState,
    key: &str,
    payload: &Value,
    idempotency: &str,
) -> Result<Value, (StatusCode, &'static str)> {
    let key_hash = hash_key(key);
    let request_hash = hex::encode(Sha256::digest(
        serde_json::to_vec(payload).map_err(|_| (StatusCode::BAD_REQUEST, "invalid_request"))?,
    ));
    let mut connection = state
        .connection
        .lock()
        .expect("mock provider database mutex poisoned");
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    if let Some((stored_hash, response)) = transaction.query_row(
        "SELECT request_hash, response_json FROM provider_queries WHERE key_hash = ?1 AND idempotency_key = ?2",
        params![key_hash, idempotency],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).optional().map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))? {
        if stored_hash != request_hash { return Err((StatusCode::CONFLICT, "idempotency_conflict")); }
        transaction.commit().map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
        return serde_json::from_str(&response).map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"));
    }
    let (credits, usage, active) = transaction
        .query_row(
            "SELECT credit_units, cumulative_usage, active FROM provider_keys WHERE key_hash = ?1",
            [&key_hash],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?
        .ok_or((StatusCode::UNAUTHORIZED, "invalid_key"))?;
    if !active {
        return Err((StatusCode::FORBIDDEN, "key_disabled"));
    }
    let credits = credits
        .parse::<u64>()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    let usage = usage
        .parse::<u64>()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    if usage >= credits {
        return Err((StatusCode::PAYMENT_REQUIRED, "insufficient_credit"));
    }
    let next_usage = usage
        .checked_add(1)
        .ok_or((StatusCode::CONFLICT, "usage_overflow"))?;
    let result = json!({"accepted": true, "echo": payload});
    let response = json!({"result": result, "cumulative_usage": next_usage});
    transaction
        .execute(
            "UPDATE provider_keys SET cumulative_usage = ?2 WHERE key_hash = ?1",
            params![key_hash, next_usage.to_string()],
        )
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    transaction.execute(
        "INSERT INTO provider_queries (key_hash, idempotency_key, request_hash, response_json, cumulative_usage) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![key_hash, idempotency, request_hash, serde_json::to_string(&response).map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?, next_usage.to_string()],
    ).map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    transaction
        .commit()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    Ok(response)
}

struct UsageSnapshot {
    cumulative_usage: u64,
    remaining_credit_units: u64,
}

fn read_usage(state: &MockState, key: &str) -> Result<UsageSnapshot, (StatusCode, &'static str)> {
    let key_hash = hash_key(key);
    let connection = state
        .connection
        .lock()
        .expect("mock provider database mutex poisoned");
    let (credit_units, usage, active) = connection
        .query_row(
            "SELECT credit_units, cumulative_usage, active
             FROM provider_keys WHERE key_hash = ?1",
            [&key_hash],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?
        .ok_or((StatusCode::NOT_FOUND, "key_not_found"))?;
    let credit_units = credit_units
        .parse::<u64>()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    let cumulative_usage = usage
        .parse::<u64>()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    let remaining_credit_units = if active {
        credit_units
            .checked_sub(cumulative_usage)
            .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?
    } else {
        0
    };
    Ok(UsageSnapshot {
        cumulative_usage,
        remaining_credit_units,
    })
}

fn with_key_update(
    state: &MockState,
    key: &str,
    update: impl FnOnce(&rusqlite::Transaction<'_>, &str) -> Result<(), (StatusCode, &'static str)>,
) -> Result<(), (StatusCode, &'static str)> {
    let key_hash = hash_key(key);
    let mut connection = state
        .connection
        .lock()
        .expect("mock provider database mutex poisoned");
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    let exists: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_keys WHERE key_hash = ?1)",
            [&key_hash],
            |row| row.get(0),
        )
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    if !exists {
        return Err((StatusCode::NOT_FOUND, "key_not_found"));
    }
    update(&transaction, &key_hash)?;
    transaction
        .commit()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "provider_error"))?;
    Ok(())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn require_operator(state: &MockState, bearer: &str) -> Result<(), (StatusCode, &'static str)> {
    if bearer == state.operator_key.as_str() {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

fn number(payload: &Value, name: &str) -> Result<u64, (StatusCode, &'static str)> {
    payload
        .get(name)
        .and_then(Value::as_u64)
        .or_else(|| {
            payload
                .get(name)
                .and_then(Value::as_str)
                .and_then(|text| text.parse().ok())
        })
        .ok_or((StatusCode::BAD_REQUEST, "invalid_request"))
}

fn derive_mock_key(operator: &[u8], binding_id: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(operator).expect("HMAC accepts any key length");
    mac.update(b"talus-agent-api-mock-provider-key-v1:");
    mac.update(binding_id.as_bytes());
    format!("mock_{}", hex::encode(mac.finalize().into_bytes()))
}

fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_server(address: std::net::SocketAddr) {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("mock provider failed to listen");
    }

    #[tokio::test]
    async fn mock_provider_uses_operator_auth_and_durable_credit_and_query_idempotency() {
        let path = std::env::temp_dir().join(format!(
            "agent-api-provider-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let routes = mock_routes(&path, "operator-secret".to_owned()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;
        let response = reqwest::get(format!("http://{address}/v1/capabilities"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let provider = HttpProvider::new(format!("http://{address}"), "operator-secret").unwrap();
        provider.verify_capabilities().await.unwrap();
        let key = provider.create_key("binding-a").await.unwrap();
        assert_eq!(key, provider.create_key("binding-a").await.unwrap());
        provider.credit(&key, 2, "charge-event-1").await.unwrap();
        provider.credit(&key, 2, "charge-event-1").await.unwrap();
        let first = provider
            .query(&key, &json!({"text":"one"}), "grant-1")
            .await
            .unwrap();
        let repeated = provider
            .query(&key, &json!({"text":"one"}), "grant-1")
            .await
            .unwrap();
        assert_eq!(first, repeated);
        assert_eq!(first.cumulative_usage, 1);
        assert!(provider
            .query(&key, &json!({"text":"changed"}), "grant-1")
            .await
            .is_err());
        let owner_direct = reqwest::Client::new()
            .post(format!("http://{address}/v1/query"))
            .bearer_auth(&key)
            .json(&json!({"payload":{"text":"direct"},"idempotency_key":"owner-direct-1"}))
            .send()
            .await
            .unwrap();
        assert!(owner_direct.status().is_success());
        let owner_client = reqwest::Client::new();
        for (path, body) in [
            (
                "/v1/operator/keys/credit",
                json!({"key": key, "credit_units": 100, "idempotency_key": "forged-credit"}),
            ),
            ("/v1/operator/keys/disable", json!({"key": key})),
            ("/v1/operator/keys/usage", json!({"key": key})),
        ] {
            let response = owner_client
                .post(format!("http://{address}{path}"))
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let missing_operator = HttpProvider::new(format!("http://{address}"), "").unwrap();
        assert!(matches!(
            missing_operator
                .create_key("binding-missing-operator")
                .await,
            Err(ProviderError::Rejected)
        ));
        let wrong_operator =
            HttpProvider::new(format!("http://{address}"), "wrong-operator").unwrap();
        assert!(matches!(
            wrong_operator.create_key("binding-wrong-operator").await,
            Err(ProviderError::Rejected)
        ));
        let missing_header = owner_client
            .post(format!("http://{address}/v1/operator/keys/disable"))
            .json(&json!({"key": key}))
            .send()
            .await
            .unwrap();
        assert_eq!(missing_header.status(), StatusCode::UNAUTHORIZED);
        let durable_account = Connection::open(&path).unwrap();
        let (credited, used): (String, String) = durable_account
            .query_row(
                "SELECT credit_units, cumulative_usage FROM provider_keys WHERE key_hash = ?1",
                [hash_key(&key)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((credited.as_str(), used.as_str()), ("2", "2"));
        provider.disable(&key).await.unwrap();
        assert_eq!(provider.final_usage(&key).await.unwrap(), 2);
        assert!(provider
            .query(&key, &json!({"text":"after revoke"}), "grant-2")
            .await
            .is_err());
        server.abort();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn disable_retires_unused_credit_and_preserves_history_after_restart() {
        let path = std::env::temp_dir().join(format!(
            "agent-api-provider-disable-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let routes = mock_routes(&path, "operator-secret".to_owned()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;

        let provider = HttpProvider::new(format!("http://{address}"), "operator-secret").unwrap();
        let key = provider.create_key("binding-disable").await.unwrap();
        provider
            .credit(&key, 4, "charge-event-disable")
            .await
            .unwrap();
        let first = provider
            .query(&key, &json!({"text":"first"}), "request-first")
            .await
            .unwrap();
        provider
            .query(&key, &json!({"text":"second"}), "request-second")
            .await
            .unwrap();
        assert_eq!(first.cumulative_usage, 1);
        assert_eq!(provider.final_usage(&key).await.unwrap(), 2);
        assert_eq!(provider.mock_remaining_credit_units(&key).await.unwrap(), 2);

        provider.disable(&key).await.unwrap();
        assert_eq!(provider.final_usage(&key).await.unwrap(), 2);
        assert_eq!(provider.mock_remaining_credit_units(&key).await.unwrap(), 0);
        provider
            .credit(&key, 4, "charge-event-disable")
            .await
            .unwrap();
        assert!(matches!(
            provider.credit(&key, 1, "new-charge-after-disable").await,
            Err(ProviderError::Rejected)
        ));
        assert_eq!(
            provider
                .query(&key, &json!({"text":"first"}), "request-first")
                .await
                .unwrap(),
            first
        );
        assert!(matches!(
            provider
                .query(&key, &json!({"text":"new"}), "request-after-disable")
                .await,
            Err(ProviderError::Rejected)
        ));

        let account = Connection::open(&path).unwrap();
        let (credit_units, usage, active): (String, String, bool) = account
            .query_row(
                "SELECT credit_units, cumulative_usage, active FROM provider_keys WHERE key_hash = ?1",
                [hash_key(&key)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let (credit_events, query_events): (i64, i64) = account
            .query_row(
                "SELECT (SELECT COUNT(*) FROM provider_credits WHERE key_hash = ?1),
                        (SELECT COUNT(*) FROM provider_queries WHERE key_hash = ?1)",
                [hash_key(&key)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (credit_units.as_str(), usage.as_str(), active),
            ("2", "2", false)
        );
        assert_eq!((credit_events, query_events), (1, 2));
        drop(account);

        server.abort();
        let _ = server.await;
        drop(provider);

        let routes = mock_routes(&path, "operator-secret".to_owned()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let restarted_server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;
        let restarted = HttpProvider::new(format!("http://{address}"), "operator-secret").unwrap();
        assert_eq!(restarted.final_usage(&key).await.unwrap(), 2);
        assert_eq!(
            restarted.mock_remaining_credit_units(&key).await.unwrap(),
            0
        );
        restarted.disable(&key).await.unwrap();
        assert_eq!(
            restarted
                .query(&key, &json!({"text":"first"}), "request-first")
                .await
                .unwrap(),
            first
        );
        restarted
            .credit(&key, 4, "charge-event-disable")
            .await
            .unwrap();
        assert_eq!(
            restarted.mock_remaining_credit_units(&key).await.unwrap(),
            0
        );

        restarted_server.abort();
        let _ = restarted_server.await;
        drop(restarted);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    }

    #[tokio::test]
    async fn http_provider_classifies_malformed_status_and_timeout_failures() {
        let mut mock_server = mockito::Server::new_async().await;
        let _malformed = mock_server
            .mock("GET", "/v1/capabilities")
            .with_status(200)
            .with_body("not-json")
            .create_async()
            .await;
        let _rejected = mock_server
            .mock("POST", "/v1/operator/keys/create")
            .with_status(403)
            .with_body(r#"{"error":"forbidden"}"#)
            .create_async()
            .await;
        let _unavailable = mock_server
            .mock("POST", "/v1/operator/keys/disable")
            .with_status(503)
            .with_body(r#"{"error":"offline"}"#)
            .create_async()
            .await;
        let provider = HttpProvider::new(mock_server.url(), "operator").unwrap();
        assert!(matches!(
            provider.verify_capabilities().await,
            Err(ProviderError::Malformed)
        ));
        assert!(matches!(
            provider.create_key("binding-a").await,
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            provider.disable("key-a").await,
            Err(ProviderError::Unavailable)
        ));

        let delayed = warp::get()
            .and(warp::path!("v1" / "capabilities"))
            .and_then(|| async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok::<_, std::convert::Infallible>(warp::reply::json(&json!({
                    "idempotency": true,
                    "key_creation": true,
                    "credit": true,
                    "disable": true,
                    "usage": true
                })))
            });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(delayed).run(address));
        wait_for_server(address).await;
        let timed = HttpProvider::with_timeout(
            format!("http://{address}"),
            "operator",
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(matches!(
            timed.verify_capabilities().await,
            Err(ProviderError::Unavailable)
        ));
        server.abort();
    }
}
