use {
    crate::events::EventOrigin,
    rusqlite::{params, Connection, OptionalExtension, TransactionBehavior},
    serde_json::Value,
    std::{
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    },
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("agent API database operation failed")]
    Database(#[from] rusqlite::Error),
    #[error("agent API binding state is inconsistent")]
    InconsistentBinding,
    #[error("authorization grant is inactive or does not match the request")]
    GrantMismatch,
    #[error("authorization grant has not been ingested")]
    GrantAbsent,
    #[error("request nonce has already been used for a different request")]
    NonceConflict,
    #[error("integer value exceeds the durable storage representation")]
    IntegerOverflow,
    #[error("provider usage regressed")]
    UsageRegression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRecord {
    pub binding_id: String,
    pub cashier_id: String,
    pub wallet_id: String,
    pub owner: String,
    pub expected_agent_uid: String,
    pub origin: Option<EventOrigin>,
    pub rate: u64,
    pub export_enabled: bool,
    pub owner_public_key: Vec<u8>,
    pub status: String,
    pub provider_key_box: Option<Vec<u8>>,
    pub charged_coin_units: u64,
    pub credit_units: u64,
    pub settled_usage: u64,
    pub recognized_coin_units: u64,
    pub latest_provider_usage: u64,
    pub refunded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementRecord {
    pub binding: BindingRecord,
    pub state: String,
    pub final_usage: u64,
    pub refundable_coin_units: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    pub nonce: [u8; 32],
    pub binding_id: String,
    pub operation: String,
    pub input_hash: [u8; 32],
    pub event_id: String,
    pub consumed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestClaim {
    Execute,
    Retry,
    Cached(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationEventDisposition {
    Stored,
    Rejected,
    Replayed,
}

pub(crate) struct RefundEventRecord<'a> {
    pub(crate) event_id: &'a str,
    pub(crate) event_type: &'a str,
    pub(crate) payload: &'a Value,
    pub(crate) binding_id: &'a str,
    pub(crate) cashier_id: &'a str,
    pub(crate) wallet_id: &'a str,
    pub(crate) coin_units: u64,
}

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    #[cfg(test)]
    pub(crate) fn hold_connection_for_test(
        &self,
        acquired: std::sync::mpsc::SyncSender<()>,
        duration: Duration,
    ) {
        let _connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        acquired
            .send(())
            .expect("test should observe the held Store connection");
        std::thread::sleep(duration);
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS bindings (
                 binding_id TEXT PRIMARY KEY,
                 cashier_id TEXT NOT NULL,
                 wallet_id TEXT NOT NULL,
                 owner TEXT NOT NULL,
                 expected_agent_uid TEXT NOT NULL,
                 rate TEXT NOT NULL,
                 export_enabled INTEGER NOT NULL,
                 owner_public_key BLOB NOT NULL,
                 status TEXT NOT NULL DEFAULT 'active',
                 provider_key_box BLOB,
                 charged_coin_units TEXT NOT NULL DEFAULT '0',
                 credit_units TEXT NOT NULL DEFAULT '0',
                 settled_usage TEXT NOT NULL DEFAULT '0',
                 recognized_coin_units TEXT NOT NULL DEFAULT '0',
                 latest_provider_usage TEXT NOT NULL DEFAULT '0',
                 refunded INTEGER NOT NULL DEFAULT 0,
                 origin_package_id TEXT,
                 origin_module TEXT,
                 origin_coin_type TEXT,
                 origin_cashier_id TEXT
             );
             CREATE TABLE IF NOT EXISTS chain_events (
                 event_id TEXT PRIMARY KEY,
                 event_type TEXT NOT NULL,
                 payload_json TEXT NOT NULL,
                 processed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE IF NOT EXISTS event_rejections (
                 event_id TEXT PRIMARY KEY REFERENCES chain_events(event_id),
                 reason TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS charges (
                 event_id TEXT PRIMARY KEY,
                 binding_id TEXT NOT NULL REFERENCES bindings(binding_id),
                 coin_units TEXT NOT NULL,
                 credit_units TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS grants (
                 nonce TEXT PRIMARY KEY,
                 binding_id TEXT NOT NULL REFERENCES bindings(binding_id),
                 operation TEXT NOT NULL,
                 input_hash TEXT NOT NULL,
                 event_id TEXT NOT NULL,
                 consumed INTEGER NOT NULL DEFAULT 0,
                 revoked INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS requests (
                 nonce TEXT PRIMARY KEY REFERENCES grants(nonce),
                 binding_id TEXT NOT NULL,
                 operation TEXT NOT NULL,
                 input_hash TEXT NOT NULL,
                 state TEXT NOT NULL,
                 response_json TEXT
             );
             CREATE TABLE IF NOT EXISTS settlements (
                 binding_id TEXT PRIMARY KEY REFERENCES bindings(binding_id),
                 state TEXT NOT NULL,
                 final_usage TEXT,
                 recognized_coin_units TEXT,
                 refundable_coin_units TEXT,
                 last_error TEXT
             );
             CREATE TABLE IF NOT EXISTS event_cursor (
                 source TEXT PRIMARY KEY,
                 cursor_json TEXT NOT NULL
             );",
        )?;
        for column in [
            "origin_package_id",
            "origin_module",
            "origin_coin_type",
            "origin_cashier_id",
        ] {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bindings') WHERE name = ?1)",
                [column],
                |row| row.get(0),
            )?;
            if !exists {
                connection
                    .execute_batch(&format!("ALTER TABLE bindings ADD COLUMN {column} TEXT;"))?;
            }
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn insert_registration(&self, record: &BindingRecord) -> Result<bool, StorageError> {
        let origin = record
            .origin
            .as_ref()
            .ok_or(StorageError::InconsistentBinding)?;
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO bindings
             (binding_id, cashier_id, wallet_id, owner, expected_agent_uid, rate,
              export_enabled, owner_public_key, status, origin_package_id,
              origin_module, origin_coin_type, origin_cashier_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?10, ?11, ?12)",
            params![
                record.binding_id,
                record.cashier_id,
                record.wallet_id,
                record.owner,
                record.expected_agent_uid,
                record.rate.to_string(),
                record.export_enabled,
                record.owner_public_key,
                origin.package_id,
                origin.module,
                origin.coin_type,
                origin.cashier_id,
            ],
        )?;
        let stored = load_binding(&transaction, &record.binding_id)?
            .ok_or(StorageError::InconsistentBinding)?;
        let same_origin = stored
            .origin
            .as_ref()
            .zip(record.origin.as_ref())
            .is_some_and(|(stored_origin, event_origin)| stored_origin.matches(event_origin));
        if stored.cashier_id != record.cashier_id
            || stored.wallet_id != record.wallet_id
            || stored.owner != record.owner
            || stored.expected_agent_uid != record.expected_agent_uid
            || stored.rate != record.rate
            || stored.export_enabled != record.export_enabled
            || stored.owner_public_key != record.owner_public_key
            || !same_origin
        {
            return Err(StorageError::InconsistentBinding);
        }
        transaction.commit()?;
        Ok(inserted == 1)
    }

    pub fn binding(&self, binding_id: &str) -> Result<Option<BindingRecord>, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        load_binding(&connection, binding_id)
    }

    pub fn bindings_pending_key(&self) -> Result<Vec<BindingRecord>, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT binding_id, cashier_id, wallet_id, owner, expected_agent_uid, rate,
                    export_enabled, owner_public_key, status, provider_key_box,
                    charged_coin_units, credit_units, settled_usage, recognized_coin_units,
                    latest_provider_usage, refunded, origin_package_id, origin_module,
                    origin_coin_type, origin_cashier_id
             FROM bindings WHERE provider_key_box IS NULL AND status = 'active'",
        )?;
        let rows = statement.query_map([], row_to_binding)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::Database)
    }

    pub fn bindings_requiring_tracking(&self) -> Result<Vec<BindingRecord>, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT binding_id FROM bindings WHERE status != 'refunded' OR refunded = 0",
        )?;
        let binding_ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        binding_ids
            .into_iter()
            .map(|binding_id| {
                load_binding(&connection, &binding_id)?.ok_or(StorageError::InconsistentBinding)
            })
            .collect()
    }

    pub fn set_provider_key_box(
        &self,
        binding_id: &str,
        sealed: &[u8],
    ) -> Result<(), StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let changed = connection.execute(
            "UPDATE bindings SET provider_key_box = COALESCE(provider_key_box, ?2) WHERE binding_id = ?1",
            params![binding_id, sealed],
        )?;
        if changed != 1 {
            return Err(StorageError::InconsistentBinding);
        }
        Ok(())
    }

    pub fn provider_key_box(&self, binding_id: &str) -> Result<Vec<u8>, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection
            .query_row(
                "SELECT provider_key_box FROM bindings WHERE binding_id = ?1 AND status = 'active'",
                [binding_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::InconsistentBinding)
    }

    pub fn record_charge(
        &self,
        event_id: &str,
        binding_id: &str,
        coin_units: u64,
        credit_units: u64,
    ) -> Result<bool, StorageError> {
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((stored_binding, stored_coin, stored_credit)) = transaction
            .query_row(
                "SELECT binding_id, coin_units, credit_units FROM charges WHERE event_id = ?1",
                [event_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        {
            if stored_binding != binding_id
                || stored_coin != coin_units.to_string()
                || stored_credit != credit_units.to_string()
            {
                return Err(StorageError::InconsistentBinding);
            }
            return Ok(false);
        }
        let binding =
            load_binding(&transaction, binding_id)?.ok_or(StorageError::InconsistentBinding)?;
        if binding.status != "active" || binding.refunded || binding.provider_key_box.is_none() {
            return Err(StorageError::InconsistentBinding);
        }
        let expected_credit = coin_units
            .checked_mul(binding.rate)
            .ok_or(StorageError::IntegerOverflow)?;
        if expected_credit != credit_units {
            return Err(StorageError::InconsistentBinding);
        }
        let charged = binding
            .charged_coin_units
            .checked_add(coin_units)
            .ok_or(StorageError::IntegerOverflow)?;
        let credits = binding
            .credit_units
            .checked_add(credit_units)
            .ok_or(StorageError::IntegerOverflow)?;
        transaction.execute(
            "INSERT INTO charges (event_id, binding_id, coin_units, credit_units) VALUES (?1, ?2, ?3, ?4)",
            params![event_id, binding_id, coin_units.to_string(), credit_units.to_string()],
        )?;
        transaction.execute(
            "UPDATE bindings SET charged_coin_units = ?2, credit_units = ?3 WHERE binding_id = ?1",
            params![binding_id, charged.to_string(), credits.to_string()],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn insert_grant(&self, grant: &GrantRecord) -> Result<bool, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let binding = load_binding(&connection, &grant.binding_id)?
            .ok_or(StorageError::InconsistentBinding)?;
        if binding.status != "active"
            || binding.refunded
            || binding.credit_units <= binding.settled_usage
        {
            return Err(StorageError::InconsistentBinding);
        }
        let nonce = hex::encode(grant.nonce);
        let input_hash = hex::encode(grant.input_hash);
        if let Some(existing) = connection
            .query_row(
                "SELECT binding_id, operation, input_hash, event_id FROM grants WHERE nonce = ?1",
                [&nonce],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        {
            if existing
                == (
                    grant.binding_id.clone(),
                    grant.operation.clone(),
                    input_hash,
                    grant.event_id.clone(),
                )
            {
                return Ok(false);
            }
            return Err(StorageError::NonceConflict);
        }
        connection.execute(
            "INSERT INTO grants (nonce, binding_id, operation, input_hash, event_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![nonce, grant.binding_id, grant.operation, input_hash, grant.event_id],
        )?;
        Ok(true)
    }

    pub fn record_authorization_event(
        &self,
        grant: &GrantRecord,
        event_type: &str,
        payload: &Value,
    ) -> Result<AuthorizationEventDisposition, StorageError> {
        let payload_json =
            serde_json::to_string(payload).map_err(|_| StorageError::InconsistentBinding)?;
        let nonce = hex::encode(grant.nonce);
        let input_hash = hex::encode(grant.input_hash);
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing_event: Option<(String, String)> = transaction
            .query_row(
                "SELECT event_type, payload_json FROM chain_events WHERE event_id = ?1",
                [&grant.event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_type, stored_payload)) = existing_event {
            if stored_type != event_type || stored_payload != payload_json {
                return Err(StorageError::InconsistentBinding);
            }
            transaction.commit()?;
            return Ok(AuthorizationEventDisposition::Replayed);
        }

        let binding = load_binding(&transaction, &grant.binding_id)?
            .ok_or(StorageError::InconsistentBinding)?;
        if binding.status != "active"
            || binding.refunded
            || binding.credit_units <= binding.settled_usage
        {
            return Err(StorageError::InconsistentBinding);
        }

        let existing_grant: Option<(String, String, String, String)> = transaction
            .query_row(
                "SELECT binding_id, operation, input_hash, event_id FROM grants WHERE nonce = ?1",
                [&nonce],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let grant_already_exists = existing_grant.is_some();
        let same_grant = existing_grant.as_ref().is_some_and(|existing| {
            existing
                == &(
                    grant.binding_id.clone(),
                    grant.operation.clone(),
                    input_hash.clone(),
                    grant.event_id.clone(),
                )
        });
        let conflicting_grant = grant_already_exists && !same_grant;

        transaction.execute(
            "INSERT INTO chain_events (event_id, event_type, payload_json) VALUES (?1, ?2, ?3)",
            params![grant.event_id, event_type, payload_json],
        )?;
        if conflicting_grant {
            transaction.execute(
                "INSERT INTO event_rejections (event_id, reason) VALUES (?1, ?2)",
                params![grant.event_id, "authorization nonce already has a grant"],
            )?;
            transaction.commit()?;
            return Ok(AuthorizationEventDisposition::Rejected);
        }

        if !grant_already_exists {
            transaction.execute(
                "INSERT INTO grants (nonce, binding_id, operation, input_hash, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    nonce,
                    grant.binding_id,
                    grant.operation,
                    input_hash,
                    grant.event_id
                ],
            )?;
        }
        transaction.commit()?;
        Ok(AuthorizationEventDisposition::Stored)
    }

    pub fn event_rejection_reason(&self, event_id: &str) -> Result<Option<String>, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection
            .query_row(
                "SELECT reason FROM event_rejections WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::Database)
    }

    pub fn grant_for_context(
        &self,
        nonce: &[u8; 32],
        operation: &str,
        input_hash: &[u8; 32],
    ) -> Result<GrantRecord, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let grant =
            load_grant(&connection, &hex::encode(nonce))?.ok_or(StorageError::GrantMismatch)?;
        let binding =
            load_binding(&connection, &grant.binding_id)?.ok_or(StorageError::GrantMismatch)?;
        if grant.operation != operation
            || grant.input_hash != *input_hash
            || binding.status != "active"
            || binding.refunded
        {
            return Err(StorageError::GrantMismatch);
        }
        Ok(grant)
    }

    pub fn grant_for_invocation(
        &self,
        nonce: &[u8; 32],
        operation: &str,
        input_hash: &[u8; 32],
    ) -> Result<GrantRecord, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let (grant, revoked) = load_grant_with_status(&connection, &hex::encode(nonce))?
            .ok_or(StorageError::GrantAbsent)?;
        if grant.operation != operation || grant.input_hash != *input_hash {
            return Err(StorageError::GrantMismatch);
        }
        let binding =
            load_binding(&connection, &grant.binding_id)?.ok_or(StorageError::GrantMismatch)?;
        if !revoked && binding.status == "active" && !binding.refunded {
            return Ok(grant);
        }
        if !grant.consumed || grant.event_id.is_empty() {
            return Err(StorageError::GrantMismatch);
        }
        let request = connection
            .query_row(
                "SELECT binding_id, operation, input_hash, state, response_json
                 FROM requests WHERE nonce = ?1",
                [hex::encode(nonce)],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((binding_id, stored_operation, stored_hash, state, response)) = request else {
            return Err(StorageError::GrantMismatch);
        };
        if binding_id != grant.binding_id
            || stored_operation != grant.operation
            || stored_hash != hex::encode(grant.input_hash)
            || state != "complete"
        {
            return Err(StorageError::GrantMismatch);
        }
        let response = response.ok_or(StorageError::InconsistentBinding)?;
        serde_json::from_str::<Value>(&response).map_err(|_| StorageError::InconsistentBinding)?;
        Ok(grant)
    }

    pub fn claim_request(
        &self,
        nonce: &[u8; 32],
        operation: &str,
        input_hash: &[u8; 32],
        binding_id: &str,
    ) -> Result<RequestClaim, StorageError> {
        let nonce_hex = hex::encode(nonce);
        let hash_hex = hex::encode(input_hash);
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (grant, revoked) =
            load_grant_with_status(&transaction, &nonce_hex)?.ok_or(StorageError::GrantMismatch)?;
        let binding = load_binding(&transaction, binding_id)?.ok_or(StorageError::GrantMismatch)?;
        if grant.binding_id != binding_id
            || grant.operation != operation
            || grant.input_hash != *input_hash
        {
            return Err(StorageError::GrantMismatch);
        }
        let existing = transaction
            .query_row(
                "SELECT binding_id, operation, input_hash, state, response_json FROM requests WHERE nonce = ?1",
                [&nonce_hex],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?)),
            )
            .optional()?;
        if let Some((stored_binding, stored_operation, stored_hash, state, response)) = existing {
            if stored_binding != binding_id
                || stored_operation != operation
                || stored_hash != hash_hex
            {
                return Err(StorageError::NonceConflict);
            }
            if state == "complete" {
                if !grant.consumed || grant.event_id.is_empty() {
                    return Err(StorageError::GrantMismatch);
                }
                let response = response.ok_or(StorageError::InconsistentBinding)?;
                let cached = serde_json::from_str(&response)
                    .map_err(|_| StorageError::InconsistentBinding)?;
                transaction.commit()?;
                return Ok(RequestClaim::Cached(cached));
            }
            if state != "pending" || revoked || binding.status != "active" || binding.refunded {
                return Err(StorageError::GrantMismatch);
            }
            transaction.commit()?;
            return Ok(RequestClaim::Retry);
        }
        if revoked || binding.status != "active" || binding.refunded {
            return Err(StorageError::GrantMismatch);
        }
        if grant.consumed {
            return Err(StorageError::NonceConflict);
        }
        transaction.execute(
            "UPDATE grants SET consumed = 1 WHERE nonce = ?1",
            [&nonce_hex],
        )?;
        transaction.execute(
            "INSERT INTO requests (nonce, binding_id, operation, input_hash, state) VALUES (?1, ?2, ?3, ?4, 'pending')",
            params![nonce_hex, binding_id, operation, hash_hex],
        )?;
        transaction.commit()?;
        Ok(RequestClaim::Execute)
    }

    pub fn finish_request(
        &self,
        nonce: &[u8; 32],
        input_hash: &[u8; 32],
        binding_id: &str,
        result: &Value,
    ) -> Result<Value, StorageError> {
        let nonce_hex = hex::encode(nonce);
        let hash_hex = hex::encode(input_hash);
        let serialized =
            serde_json::to_string(result).map_err(|_| StorageError::InconsistentBinding)?;
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (grant, _) = load_grant_with_status(&transaction, &nonce_hex)?
            .ok_or(StorageError::InconsistentBinding)?;
        let _binding =
            load_binding(&transaction, binding_id)?.ok_or(StorageError::InconsistentBinding)?;
        if grant.binding_id != binding_id
            || grant.operation != "retrieve-key"
            || grant.input_hash != *input_hash
            || !grant.consumed
            || grant.event_id.is_empty()
        {
            return Err(StorageError::NonceConflict);
        }
        let request = transaction
            .query_row(
                "SELECT binding_id, operation, input_hash, state, response_json
                 FROM requests WHERE nonce = ?1",
                [&nonce_hex],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StorageError::InconsistentBinding)?;
        let (stored_binding, stored_operation, stored_hash, state, response) = request;
        if stored_binding != binding_id
            || stored_operation != "retrieve-key"
            || stored_hash != hash_hex
        {
            return Err(StorageError::NonceConflict);
        }
        if state == "complete" {
            let cached = response.ok_or(StorageError::InconsistentBinding)?;
            let cached =
                serde_json::from_str(&cached).map_err(|_| StorageError::InconsistentBinding)?;
            transaction.commit()?;
            return Ok(cached);
        }
        if state != "pending" {
            return Err(StorageError::InconsistentBinding);
        }
        let changed = transaction.execute(
            "UPDATE requests SET state = 'complete', response_json = ?2
             WHERE nonce = ?1 AND state = 'pending'",
            params![nonce_hex, serialized],
        )?;
        if changed != 1 {
            return Err(StorageError::InconsistentBinding);
        }
        transaction.commit()?;
        Ok(result.clone())
    }

    pub fn finish_query_request(
        &self,
        nonce: &[u8; 32],
        input_hash: &[u8; 32],
        binding_id: &str,
        cumulative_usage: u64,
        result: &Value,
    ) -> Result<Value, StorageError> {
        let nonce_hex = hex::encode(nonce);
        let hash_hex = hex::encode(input_hash);
        let serialized =
            serde_json::to_string(result).map_err(|_| StorageError::InconsistentBinding)?;
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (grant, _) = load_grant_with_status(&transaction, &nonce_hex)?
            .ok_or(StorageError::InconsistentBinding)?;
        let binding =
            load_binding(&transaction, binding_id)?.ok_or(StorageError::InconsistentBinding)?;
        if grant.binding_id != binding_id
            || grant.operation != "query"
            || grant.input_hash != *input_hash
            || !grant.consumed
            || grant.event_id.is_empty()
        {
            return Err(StorageError::NonceConflict);
        }
        if cumulative_usage > binding.credit_units {
            return Err(StorageError::InconsistentBinding);
        }
        let request = transaction
            .query_row(
                "SELECT binding_id, operation, input_hash, state, response_json
                 FROM requests WHERE nonce = ?1",
                [&nonce_hex],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StorageError::InconsistentBinding)?;
        let (stored_binding, stored_operation, stored_hash, state, response) = request;
        if stored_binding != binding_id || stored_operation != "query" || stored_hash != hash_hex {
            return Err(StorageError::NonceConflict);
        }
        if state == "complete" {
            let cached = response.ok_or(StorageError::InconsistentBinding)?;
            let cached =
                serde_json::from_str(&cached).map_err(|_| StorageError::InconsistentBinding)?;
            transaction.commit()?;
            return Ok(cached);
        }
        if state != "pending" {
            return Err(StorageError::InconsistentBinding);
        }
        let latest_usage = binding.latest_provider_usage.max(cumulative_usage);
        transaction.execute(
            "UPDATE bindings SET latest_provider_usage = ?2 WHERE binding_id = ?1",
            params![binding_id, latest_usage.to_string()],
        )?;
        let changed = transaction.execute(
            "UPDATE requests SET state = 'complete', response_json = ?2
             WHERE nonce = ?1 AND state = 'pending'",
            params![nonce_hex, serialized],
        )?;
        if changed != 1 {
            return Err(StorageError::InconsistentBinding);
        }
        transaction.commit()?;
        Ok(result.clone())
    }

    pub fn update_provider_usage(&self, binding_id: &str, usage: u64) -> Result<(), StorageError> {
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding =
            load_binding(&transaction, binding_id)?.ok_or(StorageError::InconsistentBinding)?;
        if usage < binding.latest_provider_usage {
            return Err(StorageError::UsageRegression);
        }
        if usage > binding.credit_units {
            return Err(StorageError::InconsistentBinding);
        }
        transaction.execute(
            "UPDATE bindings SET latest_provider_usage = ?2 WHERE binding_id = ?1",
            params![binding_id, usage.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_usage_settled(
        &self,
        binding_id: &str,
        cumulative_usage: u64,
        recognized_coin_units: u64,
    ) -> Result<(), StorageError> {
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding =
            load_binding(&transaction, binding_id)?.ok_or(StorageError::InconsistentBinding)?;
        if !matches!(binding.status.as_str(), "active" | "revoked")
            || binding.refunded
            || cumulative_usage < binding.settled_usage
            || cumulative_usage > binding.credit_units
            || recognized_coin_units < binding.recognized_coin_units
            || recognized_coin_units > binding.charged_coin_units
        {
            return Err(StorageError::InconsistentBinding);
        }
        transaction.execute(
            "UPDATE bindings SET settled_usage = ?2, recognized_coin_units = ?3 WHERE binding_id = ?1",
            params![binding_id, cumulative_usage.to_string(), recognized_coin_units.to_string()],
        )?;
        if binding.status == "revoked" {
            transaction.execute(
                "UPDATE settlements SET
                     state = CASE WHEN state IN ('refund_submitting', 'refund_submitted', 'refunded')
                                  THEN state ELSE 'settled' END,
                     final_usage = ?2,
                     recognized_coin_units = ?3,
                     last_error = NULL
                 WHERE binding_id = ?1 AND refundable_coin_units IS NOT NULL",
                params![
                    binding_id,
                    cumulative_usage.to_string(),
                    recognized_coin_units.to_string()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn record_refund_without_event_for_test(
        &self,
        binding_id: &str,
        wallet_id: &str,
        coin_units: u64,
    ) -> Result<(), StorageError> {
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding =
            load_binding(&transaction, binding_id)?.ok_or(StorageError::InconsistentBinding)?;
        if binding.wallet_id != wallet_id
            || binding.status != "revoked"
            || binding.refunded
            || binding
                .charged_coin_units
                .checked_sub(binding.recognized_coin_units)
                != Some(coin_units)
        {
            return Err(StorageError::InconsistentBinding);
        }
        transaction.execute(
            "UPDATE bindings SET status = 'refunded', refunded = 1 WHERE binding_id = ?1",
            [binding_id],
        )?;
        transaction.execute(
            "INSERT INTO settlements (binding_id, state, final_usage, recognized_coin_units, refundable_coin_units)
             VALUES (?1, 'refunded', ?2, ?3, ?4)
             ON CONFLICT(binding_id) DO UPDATE SET state = 'refunded', refundable_coin_units = excluded.refundable_coin_units, last_error = NULL",
            params![binding_id, binding.latest_provider_usage.to_string(), binding.recognized_coin_units.to_string(), coin_units.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn record_refund_event(
        &self,
        event: RefundEventRecord<'_>,
    ) -> Result<(), StorageError> {
        let RefundEventRecord {
            event_id,
            event_type,
            payload,
            binding_id,
            cashier_id,
            wallet_id,
            coin_units,
        } = event;
        let payload_json =
            serde_json::to_string(payload).map_err(|_| StorageError::InconsistentBinding)?;
        let mut connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing_event: Option<(String, String)> = transaction
            .query_row(
                "SELECT event_type, payload_json FROM chain_events WHERE event_id = ?1",
                [event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_type, stored_payload)) = existing_event {
            if stored_type != event_type || stored_payload != payload_json {
                return Err(StorageError::InconsistentBinding);
            }
            transaction.commit()?;
            return Ok(());
        }

        let binding =
            load_binding(&transaction, binding_id)?.ok_or(StorageError::InconsistentBinding)?;
        if binding.cashier_id != cashier_id || binding.wallet_id != wallet_id {
            return Err(StorageError::InconsistentBinding);
        }
        if binding.refunded {
            let recorded_refund: Option<String> = transaction
                .query_row(
                    "SELECT refundable_coin_units FROM settlements WHERE binding_id = ?1 AND state = 'refunded'",
                    [binding_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            let expected_refund = coin_units.to_string();
            if binding.status != "refunded"
                || recorded_refund.as_deref() != Some(expected_refund.as_str())
            {
                return Err(StorageError::InconsistentBinding);
            }
        } else {
            if binding.status != "revoked"
                || binding
                    .charged_coin_units
                    .checked_sub(binding.recognized_coin_units)
                    != Some(coin_units)
            {
                return Err(StorageError::InconsistentBinding);
            }
            transaction.execute(
                "UPDATE bindings SET status = 'refunded', refunded = 1 WHERE binding_id = ?1",
                [binding_id],
            )?;
            transaction.execute(
                "INSERT INTO settlements (binding_id, state, final_usage, recognized_coin_units, refundable_coin_units)
                 VALUES (?1, 'refunded', ?2, ?3, ?4)
                 ON CONFLICT(binding_id) DO UPDATE SET state = 'refunded', refundable_coin_units = excluded.refundable_coin_units, last_error = NULL",
                params![
                    binding_id,
                    binding.latest_provider_usage.to_string(),
                    binding.recognized_coin_units.to_string(),
                    coin_units.to_string()
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO chain_events (event_id, event_type, payload_json) VALUES (?1, ?2, ?3)",
            params![event_id, event_type, payload_json],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn queue_settlement(
        &self,
        binding_id: &str,
        final_usage: u64,
        refundable_coin_units: u64,
    ) -> Result<(), StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection.execute(
            "INSERT INTO settlements (binding_id, state, final_usage, refundable_coin_units)
             VALUES (?1, 'pending', ?2, ?3)
             ON CONFLICT(binding_id) DO UPDATE SET
             state = CASE WHEN state IN ('refund_submitting', 'refund_submitted', 'refunded')
                          THEN state ELSE 'pending' END,
             final_usage = excluded.final_usage, refundable_coin_units = excluded.refundable_coin_units",
            params![binding_id, final_usage.to_string(), refundable_coin_units.to_string()],
        )?;
        Ok(())
    }

    pub fn pending_settlements(&self) -> Result<Vec<SettlementRecord>, StorageError> {
        let rows = {
            let connection = self
                .connection
                .lock()
                .expect("agent API database mutex poisoned");
            let mut statement = connection.prepare(
                "SELECT binding_id, state, final_usage, refundable_coin_units
                 FROM settlements
                 WHERE state IN ('pending', 'settling', 'settled', 'refund_submitting', 'refund_submitted')
                 ORDER BY binding_id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        rows.into_iter()
            .map(|(binding_id, state, final_usage, refundable_coin_units)| {
                let binding = self
                    .binding(&binding_id)?
                    .ok_or(StorageError::InconsistentBinding)?;
                let final_usage = final_usage
                    .ok_or(StorageError::InconsistentBinding)?
                    .parse()
                    .map_err(|_| StorageError::InconsistentBinding)?;
                let refundable_coin_units = refundable_coin_units
                    .ok_or(StorageError::InconsistentBinding)?
                    .parse()
                    .map_err(|_| StorageError::InconsistentBinding)?;
                Ok(SettlementRecord {
                    binding,
                    state,
                    final_usage,
                    refundable_coin_units,
                })
            })
            .collect()
    }

    pub fn set_settlement_state(
        &self,
        binding_id: &str,
        state: &str,
        last_error: Option<&str>,
    ) -> Result<(), StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection.execute(
            "UPDATE settlements SET state = ?2, last_error = ?3 WHERE binding_id = ?1 AND state != 'refunded'",
            params![binding_id, state, last_error],
        )?;
        Ok(())
    }

    pub fn revoke_binding(&self, binding_id: &str) -> Result<(), StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection.execute(
            "UPDATE bindings SET status = 'revoked' WHERE binding_id = ?1 AND status != 'refunded'",
            [binding_id],
        )?;
        connection.execute(
            "UPDATE grants SET revoked = 1 WHERE binding_id = ?1",
            [binding_id],
        )?;
        Ok(())
    }

    pub fn has_chain_event(&self, event_id: &str) -> Result<bool, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM chain_events WHERE event_id = ?1)",
                [event_id],
                |row| row.get(0),
            )
            .map_err(StorageError::Database)
    }

    pub fn chain_event_matches(
        &self,
        event_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<bool, StorageError> {
        let payload =
            serde_json::to_string(payload).map_err(|_| StorageError::InconsistentBinding)?;
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let stored: Option<(String, String)> = connection
            .query_row(
                "SELECT event_type, payload_json FROM chain_events WHERE event_id = ?1",
                [event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(stored.is_some_and(|(stored_type, stored_payload)| {
            stored_type == event_type && stored_payload == payload
        }))
    }

    pub(crate) fn provider_key_box_for_lifecycle(
        &self,
        binding_id: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection
            .query_row(
                "SELECT provider_key_box FROM bindings WHERE binding_id = ?1",
                [binding_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::InconsistentBinding)
    }

    pub fn record_chain_event(
        &self,
        event_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<bool, StorageError> {
        let payload =
            serde_json::to_string(payload).map_err(|_| StorageError::InconsistentBinding)?;
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        let inserted = connection.execute(
            "INSERT OR IGNORE INTO chain_events (event_id, event_type, payload_json) VALUES (?1, ?2, ?3)",
            params![event_id, event_type, payload],
        )?;
        Ok(inserted == 1)
    }

    pub fn event_cursor(&self, source: &str) -> Result<Option<Value>, StorageError> {
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection
            .query_row(
                "SELECT cursor_json FROM event_cursor WHERE source = ?1",
                [source],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).map_err(|_| StorageError::InconsistentBinding))
            .transpose()
    }

    pub fn set_event_cursor(&self, source: &str, cursor: &Value) -> Result<(), StorageError> {
        let cursor =
            serde_json::to_string(cursor).map_err(|_| StorageError::InconsistentBinding)?;
        let connection = self
            .connection
            .lock()
            .expect("agent API database mutex poisoned");
        connection.execute(
            "INSERT INTO event_cursor (source, cursor_json) VALUES (?1, ?2)
             ON CONFLICT(source) DO UPDATE SET cursor_json = excluded.cursor_json",
            params![source, cursor],
        )?;
        Ok(())
    }
}

fn load_binding(
    connection: &Connection,
    binding_id: &str,
) -> Result<Option<BindingRecord>, StorageError> {
    connection
        .query_row(
            "SELECT binding_id, cashier_id, wallet_id, owner, expected_agent_uid, rate,
                    export_enabled, owner_public_key, status, provider_key_box,
                    charged_coin_units, credit_units, settled_usage, recognized_coin_units,
                    latest_provider_usage, refunded, origin_package_id, origin_module,
                    origin_coin_type, origin_cashier_id
             FROM bindings WHERE binding_id = ?1",
            [binding_id],
            row_to_binding,
        )
        .optional()
        .map_err(StorageError::Database)
}

fn row_to_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<BindingRecord> {
    let parse = |index| -> rusqlite::Result<u64> {
        row.get::<_, String>(index)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidQuery)
    };
    let origin_parts = (
        row.get::<_, Option<String>>(16)?,
        row.get::<_, Option<String>>(17)?,
        row.get::<_, Option<String>>(18)?,
        row.get::<_, Option<String>>(19)?,
    );
    let origin = match origin_parts {
        (Some(package_id), Some(module), Some(coin_type), Some(cashier_id)) => Some(EventOrigin {
            package_id,
            module,
            coin_type,
            cashier_id,
        }),
        (None, None, None, None) => None,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(BindingRecord {
        binding_id: row.get(0)?,
        cashier_id: row.get(1)?,
        wallet_id: row.get(2)?,
        owner: row.get(3)?,
        expected_agent_uid: row.get(4)?,
        origin,
        rate: parse(5)?,
        export_enabled: row.get(6)?,
        owner_public_key: row.get(7)?,
        status: row.get(8)?,
        provider_key_box: row.get(9)?,
        charged_coin_units: parse(10)?,
        credit_units: parse(11)?,
        settled_usage: parse(12)?,
        recognized_coin_units: parse(13)?,
        latest_provider_usage: parse(14)?,
        refunded: row.get(15)?,
    })
}

fn load_grant(connection: &Connection, nonce: &str) -> Result<Option<GrantRecord>, StorageError> {
    connection
        .query_row(
            "SELECT nonce, binding_id, operation, input_hash, event_id, consumed, revoked
             FROM grants WHERE nonce = ?1 AND revoked = 0",
            [nonce],
            |row| {
                let nonce = decode_hex_32(&row.get::<_, String>(0)?)?;
                let input_hash = decode_hex_32(&row.get::<_, String>(3)?)?;
                Ok(GrantRecord {
                    nonce,
                    binding_id: row.get(1)?,
                    operation: row.get(2)?,
                    input_hash,
                    event_id: row.get(4)?,
                    consumed: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(StorageError::Database)
}

fn load_grant_with_status(
    connection: &Connection,
    nonce: &str,
) -> Result<Option<(GrantRecord, bool)>, StorageError> {
    connection
        .query_row(
            "SELECT nonce, binding_id, operation, input_hash, event_id, consumed, revoked
             FROM grants WHERE nonce = ?1",
            [nonce],
            |row| {
                let nonce = decode_hex_32(&row.get::<_, String>(0)?)?;
                let input_hash = decode_hex_32(&row.get::<_, String>(3)?)?;
                Ok((
                    GrantRecord {
                        nonce,
                        binding_id: row.get(1)?,
                        operation: row.get(2)?,
                        input_hash,
                        event_id: row.get(4)?,
                        consumed: row.get(5)?,
                    },
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(StorageError::Database)
}

fn decode_hex_32(value: &str) -> rusqlite::Result<[u8; 32]> {
    let bytes = hex::decode(value).map_err(|_| rusqlite::Error::InvalidQuery)?;
    bytes.try_into().map_err(|_| rusqlite::Error::InvalidQuery)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{
            path::PathBuf,
            sync::{Arc, Barrier},
            thread,
        },
    };

    fn db_path() -> PathBuf {
        std::env::temp_dir().join(format!("agent-api-storage-{}.sqlite", uuid::Uuid::new_v4()))
    }

    fn binding() -> BindingRecord {
        BindingRecord {
            binding_id: "binding-a".to_owned(),
            cashier_id: "cashier-a".to_owned(),
            wallet_id: "wallet-a".to_owned(),
            owner: "owner-a".to_owned(),
            expected_agent_uid: "agent-a".to_owned(),
            origin: Some(EventOrigin {
                package_id: "0xabc".to_owned(),
                module: "accounting".to_owned(),
                coin_type: "0x2::sui::SUI".to_owned(),
                cashier_id: "0x2".to_owned(),
            }),
            rate: 2,
            export_enabled: false,
            owner_public_key: Vec::new(),
            status: "active".to_owned(),
            provider_key_box: None,
            charged_coin_units: 0,
            credit_units: 0,
            settled_usage: 0,
            recognized_coin_units: 0,
            latest_provider_usage: 0,
            refunded: false,
        }
    }

    fn grant(nonce: u8, input_hash: u8, event_id: &str) -> GrantRecord {
        GrantRecord {
            nonce: [nonce; 32],
            binding_id: "binding-a".to_owned(),
            operation: "query".to_owned(),
            input_hash: [input_hash; 32],
            event_id: event_id.to_owned(),
            consumed: false,
        }
    }

    #[test]
    fn invocation_grant_lookup_distinguishes_absence_from_present_mismatches() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.grant_for_invocation(&[7; 32], "query", &[8; 32]),
            Err(StorageError::GrantAbsent)
        ));

        store.insert_registration(&binding()).unwrap();
        store
            .set_provider_key_box("binding-a", b"sealed-test-key")
            .unwrap();
        store
            .record_charge("charge-1", "binding-a", 10, 20)
            .unwrap();
        let actual_grant = grant(7, 8, "authorized-1");
        store.insert_grant(&actual_grant).unwrap();
        assert!(matches!(
            store.grant_for_invocation(
                &actual_grant.nonce,
                "retrieve-key",
                &actual_grant.input_hash
            ),
            Err(StorageError::GrantMismatch)
        ));
        assert!(matches!(
            store.grant_for_invocation(&actual_grant.nonce, "query", &[9; 32]),
            Err(StorageError::GrantMismatch)
        ));

        assert_eq!(
            store
                .claim_request(
                    &actual_grant.nonce,
                    "query",
                    &actual_grant.input_hash,
                    "binding-a",
                )
                .unwrap(),
            RequestClaim::Execute
        );
        store.revoke_binding("binding-a").unwrap();
        assert!(matches!(
            store.grant_for_invocation(&actual_grant.nonce, "query", &actual_grant.input_hash),
            Err(StorageError::GrantMismatch)
        ));
    }

    #[test]
    fn conflicting_authorization_event_is_atomically_journaled_without_replacing_grant() {
        let path = db_path();
        let store = Store::open(&path).unwrap();
        store.insert_registration(&binding()).unwrap();
        let mut second_binding = binding();
        second_binding.binding_id = "binding-b".to_owned();
        store.insert_registration(&second_binding).unwrap();
        store
            .set_provider_key_box("binding-a", b"sealed-key-a")
            .unwrap();
        store
            .set_provider_key_box("binding-b", b"sealed-key-b")
            .unwrap();
        store
            .record_charge("charge-a", "binding-a", 100, 200)
            .unwrap();
        store
            .record_charge("charge-b", "binding-b", 100, 200)
            .unwrap();

        let first = grant(7, 8, "authorized-first");
        let first_payload = serde_json::json!({
            "binding_id": "binding-a",
            "operation": "query",
            "input_hash": "08"
        });
        assert_eq!(
            store
                .record_authorization_event(&first, "AuthorizationEvent", &first_payload)
                .unwrap(),
            AuthorizationEventDisposition::Stored
        );

        let conflicting = GrantRecord {
            binding_id: "binding-b".to_owned(),
            operation: "retrieve-key".to_owned(),
            input_hash: [9; 32],
            event_id: "authorized-conflict".to_owned(),
            ..first.clone()
        };
        let conflicting_payload = serde_json::json!({
            "binding_id": "binding-b",
            "operation": "retrieve-key",
            "input_hash": "09"
        });
        assert_eq!(
            store
                .record_authorization_event(
                    &conflicting,
                    "AuthorizationEvent",
                    &conflicting_payload,
                )
                .unwrap(),
            AuthorizationEventDisposition::Rejected
        );
        assert_eq!(
            store.event_rejection_reason("authorized-conflict").unwrap(),
            Some("authorization nonce already has a grant".to_owned())
        );
        assert!(store.has_chain_event("authorized-conflict").unwrap());
        assert!(store
            .chain_event_matches(
                "authorized-conflict",
                "AuthorizationEvent",
                &conflicting_payload,
            )
            .unwrap());

        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened
                .grant_for_context(&first.nonce, "query", &first.input_hash)
                .unwrap()
                .binding_id,
            "binding-a"
        );
        assert_eq!(
            reopened
                .record_authorization_event(
                    &conflicting,
                    "AuthorizationEvent",
                    &conflicting_payload,
                )
                .unwrap(),
            AuthorizationEventDisposition::Replayed
        );
        assert_eq!(
            reopened
                .event_rejection_reason("authorized-conflict")
                .unwrap(),
            Some("authorization nonce already has a grant".to_owned())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pending_key_retrieval_keeps_first_randomized_envelope_after_revoke_and_restart() {
        let path = db_path();
        let store = Store::open(&path).unwrap();
        let mut export_binding = binding();
        let owner_secret = x25519_dalek::StaticSecret::from([0x31; 32]);
        let owner_public = x25519_dalek::PublicKey::from(&owner_secret);
        export_binding.export_enabled = true;
        export_binding.owner_public_key = owner_public.as_bytes().to_vec();
        store.insert_registration(&export_binding).unwrap();
        store
            .set_provider_key_box("binding-a", b"sealed-key")
            .unwrap();
        store
            .record_charge("charge-key-retrieval", "binding-a", 1, 2)
            .unwrap();

        let mut authorization = grant(0x5a, 0x6b, "retrieve-key-authorization");
        authorization.operation = "retrieve-key".to_owned();
        store.insert_grant(&authorization).unwrap();
        assert_eq!(
            store
                .claim_request(
                    &authorization.nonce,
                    "retrieve-key",
                    &authorization.input_hash,
                    "binding-a"
                )
                .unwrap(),
            RequestClaim::Execute
        );
        assert_eq!(
            store
                .claim_request(
                    &authorization.nonce,
                    "retrieve-key",
                    &authorization.input_hash,
                    "binding-a"
                )
                .unwrap(),
            RequestClaim::Retry
        );

        let first_envelope = serde_json::to_value(
            crate::crypto::encrypt_for_owner(owner_public.as_bytes(), b"synthetic-provider-key")
                .unwrap(),
        )
        .unwrap();
        let second_envelope = serde_json::to_value(
            crate::crypto::encrypt_for_owner(owner_public.as_bytes(), b"synthetic-provider-key")
                .unwrap(),
        )
        .unwrap();
        assert_ne!(first_envelope, second_envelope);
        assert_eq!(
            store
                .finish_request(
                    &authorization.nonce,
                    &authorization.input_hash,
                    "binding-a",
                    &first_envelope,
                )
                .unwrap(),
            first_envelope
        );
        assert_eq!(
            store
                .finish_request(
                    &authorization.nonce,
                    &authorization.input_hash,
                    "binding-a",
                    &second_envelope,
                )
                .unwrap(),
            first_envelope
        );
        store.revoke_binding("binding-a").unwrap();

        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened
                .claim_request(
                    &authorization.nonce,
                    "retrieve-key",
                    &authorization.input_hash,
                    "binding-a"
                )
                .unwrap(),
            RequestClaim::Cached(first_envelope.clone())
        );
        assert_eq!(
            reopened
                .finish_request(
                    &authorization.nonce,
                    &authorization.input_hash,
                    "binding-a",
                    &second_envelope,
                )
                .unwrap(),
            first_envelope
        );
        drop(reopened);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    }

    #[test]
    fn durable_grants_replay_once_and_revoke_invalidates_inserted_grants() {
        let path = db_path();
        let store = Store::open(&path).unwrap();
        store.insert_registration(&binding()).unwrap();
        store
            .set_provider_key_box("binding-a", b"sealed-key")
            .unwrap();
        store
            .record_charge("charge-1", "binding-a", 100, 200)
            .unwrap();
        let actual_grant = grant(7, 8, "authorized-1");
        assert!(store.insert_grant(&actual_grant).unwrap());
        assert!(!store.insert_grant(&actual_grant).unwrap());
        assert!(matches!(
            store.insert_grant(&grant(7, 9, "authorized-conflict")),
            Err(StorageError::NonceConflict)
        ));
        assert_eq!(
            store
                .claim_request(
                    &actual_grant.nonce,
                    "query",
                    &actual_grant.input_hash,
                    "binding-a"
                )
                .unwrap(),
            RequestClaim::Execute
        );
        store
            .finish_query_request(
                &actual_grant.nonce,
                &actual_grant.input_hash,
                "binding-a",
                3,
                &serde_json::json!({"usage": 3}),
            )
            .unwrap();
        assert!(store
            .record_chain_event("charge-1", "ChargeEvent", &serde_json::json!({"coin": 100}))
            .unwrap());
        store
            .set_event_cursor("sui:accounting", &serde_json::json!({"checkpoint": 42}))
            .unwrap();

        drop(store);
        let reopened = Store::open(&path).unwrap();
        let recovered = reopened.binding("binding-a").unwrap().unwrap();
        assert_eq!(
            (recovered.charged_coin_units, recovered.credit_units),
            (100, 200)
        );
        let mut consumed_grant = actual_grant.clone();
        consumed_grant.consumed = true;
        assert_eq!(
            reopened
                .grant_for_context(&actual_grant.nonce, "query", &actual_grant.input_hash)
                .unwrap(),
            consumed_grant
        );
        assert_eq!(
            reopened
                .claim_request(
                    &actual_grant.nonce,
                    "query",
                    &actual_grant.input_hash,
                    "binding-a"
                )
                .unwrap(),
            RequestClaim::Cached(serde_json::json!({"usage": 3}))
        );
        assert_eq!(
            reopened.event_cursor("sui:accounting").unwrap(),
            Some(serde_json::json!({"checkpoint": 42}))
        );
        assert!(reopened.has_chain_event("charge-1").unwrap());

        let pending_grant = grant(9, 10, "authorized-2");
        assert!(reopened.insert_grant(&pending_grant).unwrap());
        assert_eq!(
            reopened
                .claim_request(
                    &pending_grant.nonce,
                    "query",
                    &pending_grant.input_hash,
                    "binding-a"
                )
                .unwrap(),
            RequestClaim::Execute
        );
        let unused_grant = grant(10, 11, "authorized-3");
        assert!(reopened.insert_grant(&unused_grant).unwrap());
        reopened.revoke_binding("binding-a").unwrap();
        assert!(reopened
            .grant_for_context(&pending_grant.nonce, "query", &pending_grant.input_hash)
            .is_err());
        assert!(reopened
            .grant_for_invocation(&pending_grant.nonce, "query", &pending_grant.input_hash)
            .is_err());
        assert!(reopened
            .claim_request(
                &pending_grant.nonce,
                "query",
                &pending_grant.input_hash,
                "binding-a"
            )
            .is_err());
        assert!(reopened
            .grant_for_invocation(&unused_grant.nonce, "query", &unused_grant.input_hash)
            .is_err());
        assert!(reopened
            .claim_request(
                &unused_grant.nonce,
                "query",
                &unused_grant.input_hash,
                "binding-a"
            )
            .is_err());
        assert!(reopened
            .grant_for_context(&actual_grant.nonce, "query", &actual_grant.input_hash)
            .is_err());
        assert_eq!(
            reopened
                .grant_for_invocation(&actual_grant.nonce, "query", &actual_grant.input_hash)
                .unwrap(),
            consumed_grant
        );
        assert_eq!(
            reopened
                .claim_request(
                    &actual_grant.nonce,
                    "query",
                    &actual_grant.input_hash,
                    "binding-a"
                )
                .unwrap(),
            RequestClaim::Cached(serde_json::json!({"usage": 3}))
        );
        assert!(reopened
            .claim_request(
                &actual_grant.nonce,
                "retrieve-key",
                &actual_grant.input_hash,
                "binding-a"
            )
            .is_err());
        assert!(reopened
            .claim_request(&actual_grant.nonce, "query", &[0xff; 32], "binding-a")
            .is_err());
        assert!(reopened
            .claim_request(
                &actual_grant.nonce,
                "query",
                &actual_grant.input_hash,
                "binding-b"
            )
            .is_err());
        assert!(reopened.binding("binding-a").unwrap().unwrap().status == "revoked");
        drop(reopened);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }

    #[test]
    fn out_of_order_query_samples_preserve_completion_and_strict_final_usage() {
        let store = Store::open_in_memory().unwrap();
        store.insert_registration(&binding()).unwrap();
        store
            .set_provider_key_box("binding-a", b"sealed-key")
            .unwrap();
        store
            .record_charge("charge-1", "binding-a", 100, 200)
            .unwrap();
        let first = grant(21, 31, "authorization-first");
        let second = grant(22, 32, "authorization-second");
        for authorized in [&first, &second] {
            store.insert_grant(authorized).unwrap();
            assert_eq!(
                store
                    .claim_request(
                        &authorized.nonce,
                        "query",
                        &authorized.input_hash,
                        "binding-a"
                    )
                    .unwrap(),
                RequestClaim::Execute
            );
        }

        let second_result = serde_json::json!({"result": "second", "cumulative_usage": 2});
        assert_eq!(
            store
                .finish_query_request(
                    &second.nonce,
                    &second.input_hash,
                    "binding-a",
                    2,
                    &second_result
                )
                .unwrap(),
            second_result
        );
        let first_result = serde_json::json!({"result": "first", "cumulative_usage": 1});
        assert_eq!(
            store
                .finish_query_request(
                    &first.nonce,
                    &first.input_hash,
                    "binding-a",
                    1,
                    &first_result
                )
                .unwrap(),
            first_result
        );
        assert_eq!(
            store
                .binding("binding-a")
                .unwrap()
                .unwrap()
                .latest_provider_usage,
            2
        );
        assert!(matches!(
            store.update_provider_usage("binding-a", 1),
            Err(StorageError::UsageRegression)
        ));
    }

    #[test]
    fn concurrent_identical_claims_have_one_executor_and_conflicting_hash_fails() {
        let path = db_path();
        let store = Store::open(&path).unwrap();
        store.insert_registration(&binding()).unwrap();
        store
            .set_provider_key_box("binding-a", b"sealed-key")
            .unwrap();
        store
            .record_charge("charge-1", "binding-a", 10, 20)
            .unwrap();
        let grant = grant(11, 12, "authorized-1");
        store.insert_grant(&grant).unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                let nonce = grant.nonce;
                let hash = grant.input_hash;
                thread::spawn(move || {
                    barrier.wait();
                    store
                        .claim_request(&nonce, "query", &hash, "binding-a")
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let claims = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == RequestClaim::Execute)
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == RequestClaim::Retry)
                .count(),
            1
        );
        assert!(matches!(
            store.claim_request(&grant.nonce, "query", &[13; 32], "binding-a"),
            Err(StorageError::GrantMismatch)
        ));
        drop(store);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }
}
