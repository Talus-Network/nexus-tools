use {
    crate::{
        accounting::refundable_coin_units,
        crypto::{open_at_rest, seal_at_rest, MasterKey},
        events::{
            AgentApiEvent,
            DecodedEvent,
            EventFilter,
            EventOrigin,
            EventPage,
            SuiEventSource,
        },
        provider::{HttpProvider, Provider, ProviderError, ProviderQueryResult},
        storage::{
            BindingRecord,
            GrantRecord,
            RefundEventRecord,
            RequestClaim,
            StorageError,
            Store,
        },
    },
    nexus_sdk::sui::types::Address,
    nexus_toolkit::AuthContext,
    serde::Deserialize,
    serde_json::Value,
    std::{collections::HashSet, str::FromStr, sync::Arc, time::Duration},
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("agent API environment is incomplete or malformed")]
    Configuration,
    #[error("agent API durable state operation failed")]
    Storage(#[from] StorageError),
    #[error("agent API provider operation failed")]
    Provider(#[from] ProviderError),
    #[error("agent API key protection failed")]
    Crypto(#[from] crate::crypto::CryptoError),
    #[error("chain event conflicts with the durable binding")]
    EventConflict,
    #[error("one or more configured event streams failed: {0}")]
    EventStreams(String),
    #[error("authorization request does not match its on-chain grant")]
    Authorization,
}

const INVOCATION_GRANT_WAIT_LIMIT: Duration = Duration::from_secs(10);
const INVOCATION_GRANT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const INVOCATION_GRANT_WAIT_SCHEDULING_SLACK: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    pub rpc_url: String,
    pub package_id: String,
    pub module: String,
    pub coin_type: String,
    pub cashier_id: String,
    pub settlement_cap_id: String,
}

impl DeploymentConfig {
    pub fn event_filter(&self) -> EventFilter {
        EventFilter {
            rpc_url: self.rpc_url.clone(),
            package_id: self.package_id.clone(),
            module: self.module.clone(),
            coin_type: self.coin_type.clone(),
            cashier_id: self.cashier_id.clone(),
        }
    }

    pub fn origin(&self) -> Result<EventOrigin, BackendError> {
        EventOrigin::from_filter(&self.event_filter()).map_err(|_| BackendError::Configuration)
    }
}

fn parse_deployment_configs(value: &str) -> Result<Vec<DeploymentConfig>, BackendError> {
    let deployments: Vec<DeploymentConfig> =
        serde_json::from_str(value).map_err(|_| BackendError::Configuration)?;
    validate_deployment_configs(deployments)
}

fn validate_deployment_configs(
    deployments: Vec<DeploymentConfig>,
) -> Result<Vec<DeploymentConfig>, BackendError> {
    let mut cashiers = HashSet::new();
    let mut capabilities = HashSet::new();
    let mut query_streams = HashSet::new();
    let mut event_streams = HashSet::new();
    for deployment in &deployments {
        let rpc =
            reqwest::Url::parse(&deployment.rpc_url).map_err(|_| BackendError::Configuration)?;
        if !matches!(rpc.scheme(), "http" | "https") || rpc.host_str().is_none() {
            return Err(BackendError::Configuration);
        }
        let origin = deployment.origin()?;
        let package = origin.package_id;
        let cashier = origin.cashier_id;
        let capability = Address::from_str(&deployment.settlement_cap_id)
            .map_err(|_| BackendError::Configuration)?
            .to_string();
        let coin = origin.coin_type;
        let mut module_chars = deployment.module.chars();
        let valid_module = module_chars
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
            && module_chars.all(|character| character == '_' || character.is_ascii_alphanumeric());
        if !valid_module
            || !cashiers.insert(cashier.clone())
            || !capabilities.insert(capability)
            || !query_streams.insert(format!("{package}:{}", deployment.module))
            || !event_streams.insert(format!("{package}:{}:{coin}:{cashier}", deployment.module))
        {
            return Err(BackendError::Configuration);
        }
    }
    Ok(deployments)
}

#[derive(Clone)]
pub struct Backend {
    pub store: Store,
    provider: Arc<dyn Provider>,
    master_key: MasterKey,
    deployments: Vec<DeploymentConfig>,
}

impl Backend {
    pub fn from_env() -> Result<Self, BackendError> {
        let store_path =
            std::env::var("AGENT_API_DB_PATH").unwrap_or_else(|_| "./agent-api.sqlite".to_owned());
        let provider_url = std::env::var("AGENT_API_PROVIDER_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8091".to_owned());
        let operator_key = std::env::var("AGENT_API_PROVIDER_OPERATOR_KEY")
            .map_err(|_| BackendError::Configuration)?;
        let master_key = MasterKey::from_hex(
            &std::env::var("AGENT_API_MASTER_KEY").map_err(|_| BackendError::Configuration)?,
        )?;
        let provider = HttpProvider::new(provider_url, operator_key)?;
        let deployments = match std::env::var("AGENT_API_DEPLOYMENTS") {
            Ok(value) => parse_deployment_configs(&value)?,
            Err(std::env::VarError::NotPresent) => Vec::new(),
            Err(std::env::VarError::NotUnicode(_)) => return Err(BackendError::Configuration),
        };
        Self::new(
            Store::open(store_path)?,
            Arc::new(provider),
            master_key,
            deployments,
        )
    }

    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        master_key: MasterKey,
        deployments: Vec<DeploymentConfig>,
    ) -> Result<Self, BackendError> {
        let deployments = validate_deployment_configs(deployments)?;
        for binding in store.bindings_requiring_tracking()? {
            let origin = binding.origin.as_ref().ok_or(BackendError::Configuration)?;
            if !deployments.iter().any(|deployment| {
                deployment
                    .origin()
                    .is_ok_and(|configured_origin| configured_origin.matches(origin))
            }) {
                return Err(BackendError::Configuration);
            }
        }
        Ok(Self {
            store,
            provider,
            master_key,
            deployments,
        })
    }

    pub async fn health(&self) -> bool {
        !self.deployments.is_empty() && self.provider.verify_capabilities().await.is_ok()
    }

    pub fn check_grant(
        &self,
        context: &AuthContext,
        operation: &str,
    ) -> Result<GrantRecord, BackendError> {
        self.store
            .grant_for_context(&context.nonce, operation, &context.input_hash)
            .map_err(|_| BackendError::Authorization)
    }

    pub fn check_invocation_grant(
        &self,
        context: &AuthContext,
        operation: &str,
    ) -> Result<GrantRecord, BackendError> {
        self.store
            .grant_for_invocation(&context.nonce, operation, &context.input_hash)
            .map_err(|_| BackendError::Authorization)
    }

    pub(crate) async fn wait_for_invocation_grant(
        &self,
        context: &AuthContext,
        operation: &str,
    ) -> Result<GrantRecord, BackendError> {
        let deadline = tokio::time::Instant::now() + INVOCATION_GRANT_WAIT_LIMIT
            - INVOCATION_GRANT_WAIT_SCHEDULING_SLACK;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(BackendError::Authorization);
            }
            let store = self.store.clone();
            let nonce = context.nonce;
            let input_hash = context.input_hash;
            let operation = operation.to_owned();
            let lookup = tokio::task::spawn_blocking(move || {
                store.grant_for_invocation(&nonce, &operation, &input_hash)
            });
            let lookup_result = match tokio::time::timeout_at(deadline, lookup).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) | Err(_) => return Err(BackendError::Authorization),
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(BackendError::Authorization);
            }
            match lookup_result {
                Ok(grant) => {
                    return Ok(grant);
                }
                Err(StorageError::GrantAbsent) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Err(BackendError::Authorization);
                    }
                    tokio::time::sleep(INVOCATION_GRANT_POLL_INTERVAL.min(deadline - now)).await;
                }
                Err(_) => return Err(BackendError::Authorization),
            }
        }
    }

    pub async fn query(
        &self,
        context: &AuthContext,
        input_hash: [u8; 32],
        binding_id: &str,
        payload: &Value,
    ) -> Result<ProviderQueryResult, BackendError> {
        if context.input_hash != input_hash {
            return Err(BackendError::Authorization);
        }
        self.check_invocation_grant(context, "query")?;
        let claim = self
            .store
            .claim_request(&context.nonce, "query", &input_hash, binding_id)
            .map_err(|_| BackendError::Authorization)?;
        if let RequestClaim::Cached(response) = claim {
            let cached: ProviderQueryResult =
                serde_json::from_value(response).map_err(|_| BackendError::EventConflict)?;
            return Ok(cached);
        }
        let binding = self
            .store
            .binding(binding_id)?
            .ok_or(BackendError::EventConflict)?;
        self.ensure_binding_origin_configured(&binding)?;
        self.provider.verify_capabilities().await?;
        let sealed = self.store.provider_key_box(binding_id)?;
        let key = open_at_rest(&self.master_key, binding_id, &sealed)?;
        let key = std::str::from_utf8(&key).map_err(|_| BackendError::EventConflict)?;
        let result = self
            .provider
            .query(key, payload, &hex::encode(context.nonce))
            .await?;
        if result.cumulative_usage > binding.credit_units {
            return Err(BackendError::EventConflict);
        }
        let response = self.store.finish_query_request(
            &context.nonce,
            &input_hash,
            binding_id,
            result.cumulative_usage,
            &serde_json::to_value(&result).map_err(|_| BackendError::EventConflict)?,
        )?;
        serde_json::from_value(response).map_err(|_| BackendError::EventConflict)
    }

    pub async fn retrieve_key(
        &self,
        context: &AuthContext,
        input_hash: [u8; 32],
        binding_id: &str,
    ) -> Result<crate::crypto::KeyEnvelope, BackendError> {
        if context.input_hash != input_hash {
            return Err(BackendError::Authorization);
        }
        let claim = self
            .store
            .claim_request(&context.nonce, "retrieve-key", &input_hash, binding_id)
            .map_err(|_| BackendError::Authorization)?;
        if let RequestClaim::Cached(response) = claim {
            return serde_json::from_value(response).map_err(|_| BackendError::EventConflict);
        }
        let binding = self
            .store
            .binding(binding_id)?
            .ok_or(BackendError::EventConflict)?;
        self.ensure_binding_origin_configured(&binding)?;
        if !binding.export_enabled || binding.owner_public_key.len() != 32 {
            return Err(BackendError::Authorization);
        }
        let sealed = self.store.provider_key_box(binding_id)?;
        let key = open_at_rest(&self.master_key, binding_id, &sealed)?;
        let envelope = crate::crypto::encrypt_for_owner(&binding.owner_public_key, &key)?;
        let response = self.store.finish_request(
            &context.nonce,
            &input_hash,
            binding_id,
            &serde_json::to_value(&envelope).map_err(|_| BackendError::EventConflict)?,
        )?;
        serde_json::from_value(response).map_err(|_| BackendError::EventConflict)
    }

    pub async fn process_event(&self, decoded: DecodedEvent) -> Result<(), BackendError> {
        let origin = decoded.origin().map_err(|_| BackendError::EventConflict)?;
        if self
            .deployments
            .iter()
            .filter(|deployment| {
                deployment
                    .origin()
                    .is_ok_and(|value| value.matches(&origin))
            })
            .count()
            != 1
        {
            return Err(BackendError::Configuration);
        }
        if let AgentApiEvent::Refunded {
            cashier_id,
            wallet_id,
            binding_id,
            coin_units,
        } = &decoded.event
        {
            let binding = self
                .store
                .binding(binding_id)?
                .ok_or(BackendError::EventConflict)?;
            if !binding
                .origin
                .as_ref()
                .is_some_and(|binding_origin| binding_origin.matches(&origin))
            {
                return Err(BackendError::EventConflict);
            }
            let payload =
                serde_json::to_value(&decoded.event).map_err(|_| BackendError::EventConflict)?;
            self.store.record_refund_event(RefundEventRecord {
                event_id: &decoded.event_id,
                event_type: &decoded.type_tag,
                payload: &payload,
                binding_id,
                cashier_id,
                wallet_id,
                coin_units: *coin_units,
            })?;
            return Ok(());
        }

        if self.store.has_chain_event(&decoded.event_id)? {
            return if self.store.chain_event_matches(
                &decoded.event_id,
                &decoded.type_tag,
                &serde_json::to_value(&decoded.event).map_err(|_| BackendError::EventConflict)?,
            )? {
                Ok(())
            } else {
                Err(BackendError::EventConflict)
            };
        }
        match &decoded.event {
            AgentApiEvent::Registered {
                cashier_id,
                wallet_id,
                binding_id,
                owner,
                expected_agent_uid,
                rate,
                export_enabled,
                owner_public_key,
            } => {
                let record = BindingRecord {
                    binding_id: binding_id.clone(),
                    cashier_id: cashier_id.clone(),
                    wallet_id: wallet_id.clone(),
                    owner: owner.clone(),
                    expected_agent_uid: expected_agent_uid.clone(),
                    origin: Some(origin.clone()),
                    rate: *rate,
                    export_enabled: *export_enabled,
                    owner_public_key: owner_public_key.clone(),
                    status: "active".to_owned(),
                    provider_key_box: None,
                    charged_coin_units: 0,
                    credit_units: 0,
                    settled_usage: 0,
                    recognized_coin_units: 0,
                    latest_provider_usage: 0,
                    refunded: false,
                };
                self.store.insert_registration(&record)?;
                self.provider.verify_capabilities().await?;
                let provider_key = self.provider.create_key(binding_id).await?;
                let sealed = seal_at_rest(&self.master_key, binding_id, provider_key.as_bytes())?;
                self.store.set_provider_key_box(binding_id, &sealed)?;
            }
            AgentApiEvent::Charged {
                cashier_id,
                wallet_id,
                binding_id,
                coin_units,
                credit_units,
                rate,
            } => {
                let binding = self
                    .store
                    .binding(binding_id)?
                    .ok_or(BackendError::EventConflict)?;
                let expected = coin_units
                    .checked_mul(*rate)
                    .ok_or(BackendError::EventConflict)?;
                if binding.cashier_id != *cashier_id
                    || !binding
                        .origin
                        .as_ref()
                        .is_some_and(|binding_origin| binding_origin.matches(&origin))
                    || binding.wallet_id != *wallet_id
                    || binding.rate != *rate
                    || binding.status != "active"
                    || expected != *credit_units
                {
                    return Err(BackendError::EventConflict);
                }
                self.provider.verify_capabilities().await?;
                let key = self.provider_key(binding_id)?;
                self.provider
                    .credit(&key, *credit_units, &decoded.event_id)
                    .await?;
                self.store.record_charge(
                    &decoded.event_id,
                    binding_id,
                    *coin_units,
                    *credit_units,
                )?;
            }
            AgentApiEvent::Authorized {
                cashier_id,
                binding_id,
                target_nonce,
                operation,
                input_hash,
            } => {
                let binding = self
                    .store
                    .binding(binding_id)?
                    .ok_or(BackendError::EventConflict)?;
                if binding.cashier_id != *cashier_id {
                    return Err(BackendError::EventConflict);
                }
                if !binding
                    .origin
                    .as_ref()
                    .is_some_and(|binding_origin| binding_origin.matches(&origin))
                {
                    return Err(BackendError::EventConflict);
                }
                let payload = serde_json::to_value(&decoded.event)
                    .map_err(|_| BackendError::EventConflict)?;
                self.store.record_authorization_event(
                    &GrantRecord {
                        nonce: *target_nonce,
                        binding_id: binding_id.clone(),
                        operation: operation.clone(),
                        input_hash: *input_hash,
                        event_id: decoded.event_id.clone(),
                        consumed: false,
                    },
                    &decoded.type_tag,
                    &payload,
                )?;
                return Ok(());
            }
            AgentApiEvent::Revoked {
                cashier_id,
                wallet_id,
                binding_id,
            } => {
                let binding = self
                    .store
                    .binding(binding_id)?
                    .ok_or(BackendError::EventConflict)?;
                if binding.cashier_id != *cashier_id
                    || binding.wallet_id != *wallet_id
                    || !binding
                        .origin
                        .as_ref()
                        .is_some_and(|binding_origin| binding_origin.matches(&origin))
                {
                    return Err(BackendError::EventConflict);
                }
                self.store.revoke_binding(binding_id)?;
                self.provider.verify_capabilities().await?;
                let key = self.provider_key(binding_id)?;
                self.provider.disable(&key).await?;
                let usage = self.provider.final_usage(&key).await?;
                let updated = self
                    .store
                    .binding(binding_id)?
                    .ok_or(BackendError::EventConflict)?;
                let refund = refundable_coin_units(updated.charged_coin_units, usage, updated.rate)
                    .map_err(|_| BackendError::EventConflict)?;
                self.store.update_provider_usage(binding_id, usage)?;
                self.store.queue_settlement(binding_id, usage, refund)?;
            }
            AgentApiEvent::UsageSettled {
                cashier_id,
                binding_id,
                cumulative_usage,
                coin_units_recognized,
            } => {
                let binding = self
                    .store
                    .binding(binding_id)?
                    .ok_or(BackendError::EventConflict)?;
                if binding.cashier_id != *cashier_id
                    || !binding
                        .origin
                        .as_ref()
                        .is_some_and(|binding_origin| binding_origin.matches(&origin))
                {
                    return Err(BackendError::EventConflict);
                }
                self.store.record_usage_settled(
                    binding_id,
                    *cumulative_usage,
                    *coin_units_recognized,
                )?;
            }
            AgentApiEvent::Refunded { .. } => {
                return Err(BackendError::EventConflict);
            }
        }
        let payload =
            serde_json::to_value(&decoded.event).map_err(|_| BackendError::EventConflict)?;
        self.store
            .record_chain_event(&decoded.event_id, &decoded.type_tag, &payload)?;
        Ok(())
    }

    pub async fn poll_chain_once(&self, source: &SuiEventSource) -> Result<usize, BackendError> {
        let filter = source.filter();
        if !self
            .deployments
            .iter()
            .any(|deployment| deployment.event_filter() == *filter)
        {
            return Err(BackendError::Configuration);
        }
        let cursor = self.store.event_cursor(source.source_name())?;
        let page: EventPage = source
            .fetch_page(cursor)
            .await
            .map_err(|_| BackendError::Configuration)?;
        let count = page.events.len();
        for event in page.events {
            let decoded = crate::events::decode_event(filter, event)
                .map_err(|_| BackendError::EventConflict)?;
            self.process_event(decoded).await?;
        }
        if let Some(cursor) = page.next_cursor {
            self.store.set_event_cursor(source.source_name(), &cursor)?;
        }
        Ok(count)
    }

    pub async fn poll_configured_events(&self) -> Result<usize, BackendError> {
        let sources = self.event_sources()?;
        let mut count = 0;
        let mut failures = Vec::new();
        for source in &sources {
            match self.poll_chain_once(source).await {
                Ok(processed) => count += processed,
                Err(error) => failures.push(format!("{}: {error}", source.source_name())),
            }
        }
        if !failures.is_empty() {
            return Err(BackendError::EventStreams(failures.join("; ")));
        }
        Ok(count)
    }

    fn ensure_binding_origin_configured(
        &self,
        binding: &BindingRecord,
    ) -> Result<(), BackendError> {
        let origin = binding.origin.as_ref().ok_or(BackendError::Configuration)?;
        if self.deployments.iter().any(|deployment| {
            deployment
                .origin()
                .is_ok_and(|configured_origin| configured_origin.matches(origin))
        }) {
            Ok(())
        } else {
            Err(BackendError::Configuration)
        }
    }

    fn provider_key(&self, binding_id: &str) -> Result<String, BackendError> {
        let sealed = self.store.provider_key_box_for_lifecycle(binding_id)?;
        let key = open_at_rest(&self.master_key, binding_id, &sealed)?;
        String::from_utf8(key).map_err(|_| BackendError::EventConflict)
    }

    pub fn event_sources(&self) -> Result<Vec<SuiEventSource>, BackendError> {
        self.deployments
            .iter()
            .map(|deployment| SuiEventSource::new(deployment.event_filter()))
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(|_| BackendError::Configuration)
    }

    pub fn deployments(&self) -> &[DeploymentConfig] {
        &self.deployments
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            crypto::MasterKey,
            events::{AgentApiEvent, DecodedEvent, EventFilter},
            provider::{mock_routes, ProviderError, ProviderQueryResult},
            settlement::{
                build_settlement,
                OwnedObjectRef,
                SettlementError,
                SettlementPlan,
                SharedObjectRef,
            },
            storage::BindingRecord,
            worker::{
                process_pending_settlements,
                SettlementExecutor,
                SettlementRoute,
                SettlementWorker,
            },
        },
        async_trait::async_trait,
        nexus_sdk::sui::types::TypeTag,
        std::{
            net::TcpListener,
            path::PathBuf,
            sync::{
                atomic::{AtomicBool, AtomicUsize, Ordering},
                Arc,
                Mutex,
            },
        },
        tokio::sync::Barrier,
        warp::Filter,
    };

    fn db_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agent-api-{label}-{}.sqlite", uuid::Uuid::new_v4()))
    }

    fn deployment(
        rpc_url: &str,
        package_id: &str,
        coin_type: &str,
        cashier_id: &str,
        settlement_cap_id: &str,
    ) -> DeploymentConfig {
        DeploymentConfig {
            rpc_url: rpc_url.to_owned(),
            package_id: package_id.to_owned(),
            module: "accounting".to_owned(),
            coin_type: coin_type.to_owned(),
            cashier_id: cashier_id.to_owned(),
            settlement_cap_id: settlement_cap_id.to_owned(),
        }
    }

    fn test_deployments() -> Vec<DeploymentConfig> {
        vec![deployment(
            "http://127.0.0.1:9000",
            "0xabc",
            "0x2::sui::SUI",
            "0x2",
            "0xc1",
        )]
    }

    #[derive(Default)]
    struct HealthProbeProvider {
        capability_checks: AtomicUsize,
        query_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for HealthProbeProvider {
        async fn verify_capabilities(&self) -> Result<(), ProviderError> {
            self.capability_checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn create_key(&self, _: &str) -> Result<String, ProviderError> {
            Err(ProviderError::Unavailable)
        }

        async fn credit(&self, _: &str, _: u64, _: &str) -> Result<(), ProviderError> {
            Err(ProviderError::Unavailable)
        }

        async fn query(
            &self,
            _: &str,
            _: &Value,
            _: &str,
        ) -> Result<ProviderQueryResult, ProviderError> {
            self.query_calls.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::Unavailable)
        }

        async fn disable(&self, _: &str) -> Result<(), ProviderError> {
            Err(ProviderError::Unavailable)
        }

        async fn final_usage(&self, _: &str) -> Result<u64, ProviderError> {
            Err(ProviderError::Unavailable)
        }
    }

    fn test_origin(cashier_id: &str) -> EventOrigin {
        deployment(
            "http://127.0.0.1:9000",
            "0xabc",
            "0x2::sui::SUI",
            cashier_id,
            "0xc1",
        )
        .origin()
        .unwrap()
    }

    fn test_binding_for(
        deployment: &DeploymentConfig,
        binding_id: &str,
        wallet_id: &str,
    ) -> BindingRecord {
        BindingRecord {
            binding_id: binding_id.to_owned(),
            cashier_id: deployment.cashier_id.clone(),
            wallet_id: wallet_id.to_owned(),
            owner: "0x5".to_owned(),
            expected_agent_uid: "0x6".to_owned(),
            origin: Some(deployment.origin().unwrap()),
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

    fn new_backend(
        store: Store,
        provider: Arc<dyn Provider>,
        master_key: MasterKey,
        deployments: Vec<DeploymentConfig>,
    ) -> Backend {
        Backend::new(store, provider, master_key, deployments)
            .expect("test deployments cover every live binding")
    }

    fn backend_for_grant_wait() -> (Backend, Store, Arc<HealthProbeProvider>) {
        let store = Store::open_in_memory().unwrap();
        let provider = Arc::new(HealthProbeProvider::default());
        let deployments = test_deployments();
        let backend = new_backend(
            store.clone(),
            provider.clone(),
            MasterKey::from_hex(&"a6".repeat(32)).unwrap(),
            deployments.clone(),
        );
        store
            .insert_registration(&test_binding_for(&deployments[0], "0x4", "0x3"))
            .unwrap();
        store
            .set_provider_key_box("0x4", b"grant-wait-sealed-key")
            .unwrap();
        store
            .record_charge("grant-wait-charge", "0x4", 100, 200)
            .unwrap();
        (backend, store, provider)
    }

    fn grant_wait_context(nonce: [u8; 32], input_hash: [u8; 32]) -> AuthContext {
        AuthContext {
            leader_id: "grant-wait-test".to_owned(),
            leader_key_id: 0,
            input_hash,
            leader_signature: [0; 64],
            nonce,
        }
    }

    #[tokio::test]
    async fn present_mismatched_invocation_grant_fails_without_waiting() {
        let (backend, store, _) = backend_for_grant_wait();
        let context = grant_wait_context([0x51; 32], [0x61; 32]);
        store
            .insert_grant(&GrantRecord {
                nonce: context.nonce,
                binding_id: "0x4".to_owned(),
                operation: "retrieve-key".to_owned(),
                input_hash: context.input_hash,
                event_id: "authorization-wrong-operation".to_owned(),
                consumed: false,
            })
            .unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            backend.wait_for_invocation_grant(&context, "query"),
        )
        .await
        .expect("a present mismatched grant must not wait for the deadline");
        assert!(matches!(result, Err(BackendError::Authorization)));
    }

    #[tokio::test]
    async fn absent_invocation_grant_times_out_without_provider_call_or_request_claim() {
        let (backend, store, provider) = backend_for_grant_wait();
        let context = grant_wait_context([0x71; 32], [0x72; 32]);

        let result = tokio::time::timeout(
            Duration::from_secs(12),
            backend.wait_for_invocation_grant(&context, "query"),
        )
        .await
        .expect("an absent grant must resolve within the ten-second wait limit");
        assert!(matches!(result, Err(BackendError::Authorization)));
        assert!(matches!(
            store.grant_for_invocation(&context.nonce, "query", &context.input_hash),
            Err(StorageError::GrantAbsent)
        ));
        assert!(matches!(
            backend
                .query(
                    &context,
                    context.input_hash,
                    "0x4",
                    &serde_json::json!({"prompt": "must not be sent"}),
                )
                .await,
            Err(BackendError::Authorization)
        ));
        assert_eq!(provider.query_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            store.claim_request(&context.nonce, "query", &context.input_hash, "0x4"),
            Err(StorageError::GrantMismatch)
        ));
    }

    #[tokio::test]
    async fn mutex_contention_cannot_extend_invocation_grant_wait_past_deadline() {
        let (backend, store, provider) = backend_for_grant_wait();
        let context = grant_wait_context([0x91; 32], [0x92; 32]);
        store
            .insert_grant(&GrantRecord {
                nonce: context.nonce,
                binding_id: "0x4".to_owned(),
                operation: "query".to_owned(),
                input_hash: context.input_hash,
                event_id: "grant-wait-lock-contention".to_owned(),
                consumed: false,
            })
            .unwrap();

        let (acquired_tx, acquired_rx) = std::sync::mpsc::sync_channel(0);
        let blocker_store = store.clone();
        let blocker = std::thread::spawn(move || {
            blocker_store.hold_connection_for_test(acquired_tx, Duration::from_millis(10_500));
        });
        acquired_rx.recv().expect("Store mutex should be held");

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            INVOCATION_GRANT_WAIT_LIMIT + Duration::from_secs(2),
            backend.wait_for_invocation_grant(&context, "query"),
        )
        .await
        .expect("authorization admission should finish within the ten-second deadline");
        let elapsed = started.elapsed();

        assert!(matches!(result, Err(BackendError::Authorization)));
        assert!(
            elapsed <= INVOCATION_GRANT_WAIT_LIMIT,
            "authorization admission took {elapsed:?} with the Store mutex held"
        );
        assert!(
            !blocker.is_finished(),
            "the Store mutex should still be held when authorization times out"
        );
        assert_eq!(provider.query_calls.load(Ordering::SeqCst), 0);

        blocker.join().unwrap();
        assert_eq!(
            store
                .claim_request(&context.nonce, "query", &context.input_hash, "0x4")
                .unwrap(),
            RequestClaim::Execute,
            "the timed-out authorization lookup must leave the grant unclaimed"
        );
    }

    fn rpc_event(digest: &str, event_seq: u64, type_tag: String, fields: Value) -> Value {
        serde_json::json!({
            "id": {"txDigest": digest, "eventSeq": event_seq.to_string()},
            "type": type_tag,
            "parsedJson": fields
        })
    }

    #[test]
    fn deployment_config_accepts_one_or_two_coin_streams_and_rejects_ambiguity() {
        let one = serde_json::json!([{
            "rpc_url": "http://127.0.0.1:9000",
            "package_id": "0xabc",
            "module": "accounting",
            "coin_type": "0x2::sui::SUI",
            "cashier_id": "0x22",
            "settlement_cap_id": "0xc1"
        }]);
        assert_eq!(parse_deployment_configs(&one.to_string()).unwrap().len(), 1);

        let two = serde_json::json!([
            one[0].clone(),
            {
                "rpc_url": "http://127.0.0.1:9000",
                "package_id": "0xdef",
                "module": "accounting",
                "coin_type": "0x123::test_coin::TEST_COIN",
                "cashier_id": "0x23",
                "settlement_cap_id": "0xc2"
            }
        ]);
        assert_eq!(parse_deployment_configs(&two.to_string()).unwrap().len(), 2);

        let duplicate_query = serde_json::json!([one[0].clone(), {
            "rpc_url": "http://127.0.0.1:9000",
            "package_id": "0x000abc",
            "module": "accounting",
            "coin_type": "0x123::test_coin::TEST_COIN",
            "cashier_id": "0x23",
            "settlement_cap_id": "0xc2"
        }]);
        assert!(parse_deployment_configs(&duplicate_query.to_string()).is_err());

        let duplicate_cashier = serde_json::json!([one[0].clone(), {
            "rpc_url": "http://127.0.0.1:9000",
            "package_id": "0xdef",
            "module": "accounting",
            "coin_type": "0x123::test_coin::TEST_COIN",
            "cashier_id": "0x0022",
            "settlement_cap_id": "0xc2"
        }]);
        assert!(parse_deployment_configs(&duplicate_cashier.to_string()).is_err());
        assert!(parse_deployment_configs("[]").unwrap().is_empty());
    }

    #[tokio::test]
    async fn readiness_requires_deployments_before_provider_capability_checks() {
        let path = db_path("readiness-deployments");
        let store = Store::open(&path).unwrap();
        let provider = Arc::new(HealthProbeProvider::default());
        let master_key = MasterKey::from_hex(&"a4".repeat(32)).unwrap();
        let empty = Backend::new(
            store.clone(),
            provider.clone(),
            master_key.clone(),
            Vec::new(),
        )
        .unwrap();

        assert!(!empty.health().await);
        assert_eq!(provider.capability_checks.load(Ordering::SeqCst), 0);

        let configured = Backend::new(
            store.clone(),
            provider.clone(),
            master_key,
            test_deployments(),
        )
        .unwrap();
        assert!(configured.health().await);
        assert_eq!(provider.capability_checks.load(Ordering::SeqCst), 1);

        drop(configured);
        drop(empty);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn restart_rejects_removed_origins_until_all_bindings_are_retired() {
        let path = db_path("deployment-retirement");
        let binding = BindingRecord {
            binding_id: "retirement-binding".to_owned(),
            cashier_id: "0x2".to_owned(),
            wallet_id: "0x3".to_owned(),
            owner: "0x5".to_owned(),
            expected_agent_uid: "0x6".to_owned(),
            origin: Some(test_origin("0x2")),
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
        };
        let provider = Arc::new(HttpProvider::new("http://127.0.0.1:1", "unused").unwrap());
        let master_key = MasterKey::from_hex(&"a3".repeat(32)).unwrap();
        let store = Store::open(&path).unwrap();
        store.insert_registration(&binding).unwrap();
        store
            .set_provider_key_box(&binding.binding_id, b"synthetic-retirement-key")
            .unwrap();
        store
            .record_charge("retirement-charge", &binding.binding_id, 100, 200)
            .unwrap();
        assert!(Backend::new(
            store.clone(),
            provider.clone(),
            master_key.clone(),
            test_deployments(),
        )
        .is_ok());

        drop(store);
        let restarted = Store::open(&path).unwrap();
        assert!(Backend::new(
            restarted.clone(),
            provider.clone(),
            master_key.clone(),
            Vec::new(),
        )
        .is_err());
        let changed_origin = vec![deployment(
            "http://127.0.0.1:9001",
            "0xdef",
            "0x2::sui::SUI",
            "0x9",
            "0xc9",
        )];
        assert!(Backend::new(
            restarted.clone(),
            provider.clone(),
            master_key.clone(),
            changed_origin,
        )
        .is_err());

        restarted.revoke_binding(&binding.binding_id).unwrap();
        restarted
            .update_provider_usage(&binding.binding_id, 0)
            .unwrap();
        restarted
            .queue_settlement(&binding.binding_id, 0, 100)
            .unwrap();
        assert!(Backend::new(
            restarted.clone(),
            provider.clone(),
            master_key.clone(),
            Vec::new(),
        )
        .is_err());
        assert!(Backend::new(
            restarted.clone(),
            provider.clone(),
            master_key.clone(),
            test_deployments(),
        )
        .is_ok());

        restarted
            .record_refund_without_event_for_test(&binding.binding_id, "0x3", 100)
            .unwrap();
        drop(restarted);
        let retired = Store::open(&path).unwrap();
        assert!(Backend::new(retired, provider, master_key, Vec::new()).is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn grant_nonce_conflict_does_not_stall_event_cursor_or_later_events() {
        let store_path = db_path("grant-conflict-cursor");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let rpc_url = format!("http://{address}");
        let deployments = vec![
            deployment(&rpc_url, "0xabc", "0x2::sui::SUI", "0x2", "0xc1"),
            deployment(&rpc_url, "0xdef", "0x2::sui::SUI", "0x7", "0xc2"),
        ];
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "data": [
                    rpc_event(
                        "grant-conflict-page",
                        0,
                        "0xdef::accounting::AuthorizationEvent<0x2::sui::SUI>".to_owned(),
                        serde_json::json!({
                            "cashier_id": "0x7",
                            "binding_id": "0x42",
                            "target_nonce": vec![0x37; 32],
                            "input_hash": vec![0x58; 32],
                            "operation": b"query"
                        }),
                    ),
                    rpc_event(
                        "grant-conflict-page",
                        1,
                        "0xdef::accounting::UsageSettledEvent<0x2::sui::SUI>".to_owned(),
                        serde_json::json!({
                            "cashier_id": "0x7",
                            "binding_id": "0x42",
                            "cumulative_usage": 2,
                            "coin_units_recognized": 1
                        }),
                    )
                ],
                "nextCursor": {"txDigest": "grant-conflict-page", "eventSeq": "1"},
                "hasNextPage": false
            }
        });
        let route = warp::post()
            .and(warp::body::json())
            .map(move |_request: Value| warp::reply::json(&response));
        let server = tokio::spawn(warp::serve(route).run(address));
        wait_for_server(address).await;

        let store = Store::open(&store_path).unwrap();
        for (deployment, binding_id, wallet_id) in [
            (&deployments[0], "0x41", "0xa1"),
            (&deployments[1], "0x42", "0xa2"),
        ] {
            store
                .insert_registration(&test_binding_for(deployment, binding_id, wallet_id))
                .unwrap();
            store
                .set_provider_key_box(binding_id, b"synthetic-event-key")
                .unwrap();
            store
                .record_charge(&format!("charge-{binding_id}"), binding_id, 100, 200)
                .unwrap();
        }
        store
            .insert_grant(&GrantRecord {
                nonce: [0x37; 32],
                binding_id: "0x41".to_owned(),
                operation: "query".to_owned(),
                input_hash: [0x48; 32],
                event_id: "existing-grant".to_owned(),
                consumed: false,
            })
            .unwrap();
        let backend = new_backend(
            store.clone(),
            Arc::new(HttpProvider::new("http://127.0.0.1:1", "unused").unwrap()),
            MasterKey::from_hex(&"a4".repeat(32)).unwrap(),
            deployments.clone(),
        );
        let source = SuiEventSource::new(deployments[1].event_filter()).unwrap();

        assert_eq!(backend.poll_chain_once(&source).await.unwrap(), 2);
        assert_eq!(
            store
                .event_rejection_reason("grant-conflict-page:0")
                .unwrap(),
            Some("authorization nonce already has a grant".to_owned())
        );
        assert!(store.has_chain_event("grant-conflict-page:1").unwrap());
        let binding = store.binding("0x42").unwrap().unwrap();
        assert_eq!(
            (binding.settled_usage, binding.recognized_coin_units),
            (2, 1)
        );
        assert_eq!(
            store.event_cursor(source.source_name()).unwrap(),
            Some(serde_json::json!({
                "txDigest": "grant-conflict-page",
                "eventSeq": "1"
            }))
        );

        server.abort();
        let _ = server.await;
        drop(backend);
        drop(store);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn worker_continues_other_stream_and_refunds_before_returning_stream_error() {
        let store_path = db_path("worker-partial-stream-failure");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let rpc_url = format!("http://{address}");
        let deployments = vec![
            deployment(&rpc_url, "0xabc", "0x2::sui::SUI", "0x2", "0xc1"),
            deployment(&rpc_url, "0xdef", "0x2::sui::SUI", "0x7", "0xc2"),
        ];
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "data": [rpc_event(
                    "worker-healthy-stream",
                    0,
                    "0xdef::accounting::UsageSettledEvent<0x2::sui::SUI>".to_owned(),
                    serde_json::json!({
                        "cashier_id": "0x7",
                        "binding_id": "0x42",
                        "cumulative_usage": 2,
                        "coin_units_recognized": 1
                    }),
                )],
                "nextCursor": {"txDigest": "worker-healthy-stream", "eventSeq": "0"},
                "hasNextPage": false
            }
        });
        let route = warp::post()
            .and(warp::body::json())
            .map(move |request: Value| {
                let package = request
                    .pointer("/params/0/MoveEventModule/package")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if package == "0xabc" {
                    warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({"error": "test stream failure"})),
                        warp::http::StatusCode::SERVICE_UNAVAILABLE,
                    )
                } else {
                    warp::reply::with_status(
                        warp::reply::json(&response),
                        warp::http::StatusCode::OK,
                    )
                }
            });
        let server = tokio::spawn(warp::serve(route).run(address));
        wait_for_server(address).await;

        let store = Store::open(&store_path).unwrap();
        for (deployment, binding_id, wallet_id) in [
            (&deployments[0], "0x41", "0xa1"),
            (&deployments[1], "0x42", "0xa2"),
        ] {
            store
                .insert_registration(&test_binding_for(deployment, binding_id, wallet_id))
                .unwrap();
            store
                .set_provider_key_box(binding_id, b"synthetic-event-key")
                .unwrap();
            store
                .record_charge(&format!("charge-{binding_id}"), binding_id, 100, 200)
                .unwrap();
        }
        store.revoke_binding("0x41").unwrap();
        store.update_provider_usage("0x41", 0).unwrap();
        store.queue_settlement("0x41", 0, 100).unwrap();
        let backend = new_backend(
            store.clone(),
            Arc::new(HttpProvider::new("http://127.0.0.1:1", "unused").unwrap()),
            MasterKey::from_hex(&"a5".repeat(32)).unwrap(),
            deployments.clone(),
        );
        let executor = BuildingSettlementExecutor::default();
        let routes = deployments
            .iter()
            .map(|deployment| SettlementRoute::new(deployment.event_filter(), executor.clone()))
            .collect();
        let worker = SettlementWorker::new(backend, routes);

        let error = worker.run_once().await.unwrap_err();
        assert!(matches!(
            error,
            crate::worker::WorkerError::Backend(BackendError::EventStreams(_))
        ));
        assert!(store
            .event_cursor("0xdef:accounting:0x2::sui::SUI:0x7")
            .unwrap()
            .is_some());
        let active = store.binding("0x42").unwrap().unwrap();
        assert_eq!((active.settled_usage, active.recognized_coin_units), (2, 1));
        let pending = store.pending_settlements().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].binding.binding_id, "0x41");
        assert_eq!(pending[0].state, "refund_submitted");
        assert_eq!(
            executor
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| (call.operation, call.binding_id.as_str()))
                .collect::<Vec<_>>(),
            vec![("settle", "0x41"), ("refund", "0x41")]
        );

        server.abort();
        let _ = server.await;
        drop(worker);
        drop(store);
        let _ = std::fs::remove_file(store_path);
    }

    async fn wait_for_server(address: std::net::SocketAddr) {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("mock provider failed to listen");
    }

    #[derive(Clone)]
    struct LoseAcknowledgementProvider {
        inner: HttpProvider,
        lose_credit_ack: Arc<AtomicBool>,
        lose_query_ack: Arc<AtomicBool>,
        lose_disable_ack: Arc<AtomicBool>,
    }

    impl LoseAcknowledgementProvider {
        fn new(inner: HttpProvider) -> Self {
            Self {
                inner,
                lose_credit_ack: Arc::new(AtomicBool::new(true)),
                lose_query_ack: Arc::new(AtomicBool::new(true)),
                lose_disable_ack: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    #[async_trait]
    impl crate::provider::Provider for LoseAcknowledgementProvider {
        async fn verify_capabilities(&self) -> Result<(), ProviderError> {
            self.inner.verify_capabilities().await
        }

        async fn create_key(&self, binding_id: &str) -> Result<String, ProviderError> {
            self.inner.create_key(binding_id).await
        }

        async fn credit(
            &self,
            key: &str,
            credit_units: u64,
            idempotency_key: &str,
        ) -> Result<(), ProviderError> {
            self.inner
                .credit(key, credit_units, idempotency_key)
                .await?;
            if self.lose_credit_ack.swap(false, Ordering::SeqCst) {
                Err(ProviderError::Unavailable)
            } else {
                Ok(())
            }
        }

        async fn query(
            &self,
            key: &str,
            payload: &Value,
            idempotency_key: &str,
        ) -> Result<ProviderQueryResult, ProviderError> {
            let result = self.inner.query(key, payload, idempotency_key).await?;
            if self.lose_query_ack.swap(false, Ordering::SeqCst) {
                Err(ProviderError::Unavailable)
            } else {
                Ok(result)
            }
        }

        async fn disable(&self, key: &str) -> Result<(), ProviderError> {
            self.inner.disable(key).await?;
            if self.lose_disable_ack.swap(false, Ordering::SeqCst) {
                Err(ProviderError::Unavailable)
            } else {
                Ok(())
            }
        }

        async fn final_usage(&self, key: &str) -> Result<u64, ProviderError> {
            self.inner.final_usage(key).await
        }
    }

    #[derive(Clone)]
    struct RevokeRaceProvider {
        inner: HttpProvider,
        query_calls: Arc<AtomicUsize>,
        first_committed: Arc<Barrier>,
        release_first: Arc<Barrier>,
        second_ready: Arc<Barrier>,
        release_second: Arc<Barrier>,
    }

    impl RevokeRaceProvider {
        fn new(inner: HttpProvider) -> Self {
            Self {
                inner,
                query_calls: Arc::new(AtomicUsize::new(0)),
                first_committed: Arc::new(Barrier::new(2)),
                release_first: Arc::new(Barrier::new(2)),
                second_ready: Arc::new(Barrier::new(2)),
                release_second: Arc::new(Barrier::new(2)),
            }
        }
    }

    #[async_trait]
    impl crate::provider::Provider for RevokeRaceProvider {
        async fn verify_capabilities(&self) -> Result<(), ProviderError> {
            self.inner.verify_capabilities().await
        }

        async fn create_key(&self, binding_id: &str) -> Result<String, ProviderError> {
            self.inner.create_key(binding_id).await
        }

        async fn credit(
            &self,
            key: &str,
            credit_units: u64,
            idempotency_key: &str,
        ) -> Result<(), ProviderError> {
            self.inner.credit(key, credit_units, idempotency_key).await
        }

        async fn query(
            &self,
            key: &str,
            payload: &Value,
            idempotency_key: &str,
        ) -> Result<ProviderQueryResult, ProviderError> {
            match self.query_calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    let result = self.inner.query(key, payload, idempotency_key).await?;
                    self.first_committed.wait().await;
                    self.release_first.wait().await;
                    Ok(result)
                }
                1 => {
                    self.second_ready.wait().await;
                    self.release_second.wait().await;
                    self.inner.query(key, payload, idempotency_key).await
                }
                _ => self.inner.query(key, payload, idempotency_key).await,
            }
        }

        async fn disable(&self, key: &str) -> Result<(), ProviderError> {
            self.inner.disable(key).await
        }

        async fn final_usage(&self, key: &str) -> Result<u64, ProviderError> {
            self.inner.final_usage(key).await
        }
    }

    #[derive(Clone)]
    struct ReorderedReplyProvider {
        inner: HttpProvider,
        query_calls: Arc<AtomicUsize>,
        first_committed: Arc<Barrier>,
        release_first: Arc<Barrier>,
    }

    impl ReorderedReplyProvider {
        fn new(inner: HttpProvider) -> Self {
            Self {
                inner,
                query_calls: Arc::new(AtomicUsize::new(0)),
                first_committed: Arc::new(Barrier::new(2)),
                release_first: Arc::new(Barrier::new(2)),
            }
        }
    }

    #[async_trait]
    impl crate::provider::Provider for ReorderedReplyProvider {
        async fn verify_capabilities(&self) -> Result<(), ProviderError> {
            self.inner.verify_capabilities().await
        }

        async fn create_key(&self, binding_id: &str) -> Result<String, ProviderError> {
            self.inner.create_key(binding_id).await
        }

        async fn credit(
            &self,
            key: &str,
            credit_units: u64,
            idempotency_key: &str,
        ) -> Result<(), ProviderError> {
            self.inner.credit(key, credit_units, idempotency_key).await
        }

        async fn query(
            &self,
            key: &str,
            payload: &Value,
            idempotency_key: &str,
        ) -> Result<ProviderQueryResult, ProviderError> {
            let call = self.query_calls.fetch_add(1, Ordering::SeqCst);
            let result = self.inner.query(key, payload, idempotency_key).await?;
            if call == 0 {
                self.first_committed.wait().await;
                self.release_first.wait().await;
            }
            Ok(result)
        }

        async fn disable(&self, key: &str) -> Result<(), ProviderError> {
            self.inner.disable(key).await
        }

        async fn final_usage(&self, key: &str) -> Result<u64, ProviderError> {
            self.inner.final_usage(key).await
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SettlementObservation {
        operation: &'static str,
        final_usage: u64,
        recognized_coin_units: u64,
        refundable_coin_units: u64,
    }

    #[derive(Default)]
    struct RecordingSettlementExecutor {
        calls: Mutex<Vec<SettlementObservation>>,
    }

    #[async_trait]
    impl SettlementExecutor for RecordingSettlementExecutor {
        async fn settle(
            &self,
            _filter: &EventFilter,
            binding: &BindingRecord,
            final_usage: u64,
        ) -> Result<(), SettlementError> {
            self.calls.lock().unwrap().push(SettlementObservation {
                operation: "settle",
                final_usage,
                recognized_coin_units: binding.recognized_coin_units,
                refundable_coin_units: 0,
            });
            Ok(())
        }

        async fn refund(
            &self,
            _filter: &EventFilter,
            binding: &BindingRecord,
            final_usage: u64,
        ) -> Result<(), SettlementError> {
            self.calls.lock().unwrap().push(SettlementObservation {
                operation: "refund",
                final_usage,
                recognized_coin_units: binding.recognized_coin_units,
                refundable_coin_units: binding.charged_coin_units - binding.recognized_coin_units,
            });
            Ok(())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RoutedSettlementObservation {
        operation: &'static str,
        package_id: String,
        coin_type: String,
        ptb_coin_type: String,
        cashier_id: String,
        binding_id: String,
    }

    #[derive(Clone, Default)]
    struct BuildingSettlementExecutor {
        calls: Arc<Mutex<Vec<RoutedSettlementObservation>>>,
    }

    impl BuildingSettlementExecutor {
        fn record(
            &self,
            operation: &'static str,
            filter: &EventFilter,
            binding: &BindingRecord,
            final_usage: u64,
        ) -> Result<(), SettlementError> {
            let plan = SettlementPlan {
                package_id: filter.package_id.clone(),
                coin_type: filter.coin_type.clone(),
                settlement_cap: OwnedObjectRef {
                    object_id: "0xc1".to_owned(),
                    version: 1,
                    digest: "11111111111111111111111111111111".to_owned(),
                    cashier_id: binding.cashier_id.clone(),
                },
                cashier: SharedObjectRef {
                    object_id: binding.cashier_id.clone(),
                    initial_shared_version: 1,
                },
                wallet: SharedObjectRef {
                    object_id: binding.wallet_id.clone(),
                    initial_shared_version: 1,
                },
                binding_id: binding.binding_id.clone(),
                charged_coin_units: binding.charged_coin_units,
                credit_units: binding.credit_units,
                rate: binding.rate,
                previous_usage: binding.settled_usage,
                previous_coin_liability: binding.recognized_coin_units,
                final_usage,
            };
            let built = build_settlement(&plan)?;
            let transaction = if operation == "settle" {
                built.settle
            } else {
                built.refund
            };
            let ptb_coin_type = match transaction.commands.first() {
                Some(nexus_sdk::sui::types::Command::MoveCall(call)) => {
                    call.type_arguments[0].to_string()
                }
                _ => return Err(SettlementError::InvalidConfiguration),
            };
            self.calls
                .lock()
                .unwrap()
                .push(RoutedSettlementObservation {
                    operation,
                    package_id: filter.package_id.clone(),
                    coin_type: filter.coin_type.clone(),
                    ptb_coin_type,
                    cashier_id: binding.cashier_id.clone(),
                    binding_id: binding.binding_id.clone(),
                });
            Ok(())
        }
    }

    #[async_trait]
    impl SettlementExecutor for BuildingSettlementExecutor {
        async fn settle(
            &self,
            filter: &EventFilter,
            binding: &BindingRecord,
            final_usage: u64,
        ) -> Result<(), SettlementError> {
            self.record("settle", filter, binding, final_usage)
        }

        async fn refund(
            &self,
            filter: &EventFilter,
            binding: &BindingRecord,
            final_usage: u64,
        ) -> Result<(), SettlementError> {
            self.record("refund", filter, binding, final_usage)
        }
    }

    async fn prepare_binding(backend: &Backend) {
        backend
            .process_event(DecodedEvent {
                event_id: "repair-register".to_owned(),
                type_tag: "0xabc::accounting::RegistrationEvent<0x2::sui::SUI>".to_owned(),
                event: AgentApiEvent::Registered {
                    cashier_id: "0x2".to_owned(),
                    wallet_id: "0x3".to_owned(),
                    binding_id: "0x4".to_owned(),
                    owner: "0x5".to_owned(),
                    expected_agent_uid: "0x6".to_owned(),
                    rate: 2,
                    export_enabled: false,
                    owner_public_key: vec![],
                },
            })
            .await
            .unwrap();
        backend
            .process_event(DecodedEvent {
                event_id: "repair-charge".to_owned(),
                type_tag: "0xabc::accounting::ChargeEvent<0x2::sui::SUI>".to_owned(),
                event: AgentApiEvent::Charged {
                    cashier_id: "0x2".to_owned(),
                    wallet_id: "0x3".to_owned(),
                    binding_id: "0x4".to_owned(),
                    coin_units: 100,
                    credit_units: 200,
                    rate: 2,
                },
            })
            .await
            .unwrap();
    }

    async fn authorize_query(
        backend: &Backend,
        event_id: &str,
        nonce: [u8; 32],
        input_hash: [u8; 32],
    ) -> AuthContext {
        backend
            .process_event(DecodedEvent {
                event_id: event_id.to_owned(),
                type_tag: "0xabc::accounting::AuthorizationEvent<0x2::sui::SUI>".to_owned(),
                event: AgentApiEvent::Authorized {
                    cashier_id: "0x2".to_owned(),
                    binding_id: "0x4".to_owned(),
                    target_nonce: nonce,
                    operation: "query".to_owned(),
                    input_hash,
                },
            })
            .await
            .unwrap();
        AuthContext {
            leader_id: event_id.to_owned(),
            leader_key_id: 0,
            input_hash,
            leader_signature: [0; 64],
            nonce,
        }
    }

    fn provider_recorded_credit_units(path: &PathBuf, binding_id: &str) -> u64 {
        let connection = rusqlite::Connection::open(path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT provider_credits.credit_units
                 FROM provider_credits
                 JOIN provider_keys USING (key_hash)
                 WHERE provider_keys.binding_id = ?1",
            )
            .unwrap();
        statement
            .query_map([binding_id], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| row.unwrap().parse::<u64>().unwrap())
            .sum()
    }

    fn provider_query_state(path: &PathBuf, binding_id: &str) -> (u64, bool, u64) {
        let connection = rusqlite::Connection::open(path).unwrap();
        let (usage, active, query_count): (String, bool, i64) = connection
            .query_row(
                "SELECT provider_keys.cumulative_usage, provider_keys.active, COUNT(provider_queries.idempotency_key)
                 FROM provider_keys LEFT JOIN provider_queries USING (key_hash)
                 WHERE provider_keys.binding_id = ?1 GROUP BY provider_keys.key_hash",
                [binding_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        (usage.parse().unwrap(), active, query_count as u64)
    }

    #[tokio::test]
    async fn query_effect_lost_ack_is_recovered_after_backend_and_provider_restart() {
        let provider_path = db_path("query-recovery-provider");
        let store_path = db_path("query-recovery-store");
        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;

        let provider_http = HttpProvider::new(format!("http://{address}"), "operator").unwrap();
        let provider = Arc::new(LoseAcknowledgementProvider::new(provider_http.clone()));
        provider.lose_credit_ack.store(false, Ordering::SeqCst);
        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"55".repeat(32)).unwrap();
        let backend = new_backend(
            store.clone(),
            provider.clone(),
            master_key.clone(),
            test_deployments(),
        );
        prepare_binding(&backend).await;
        let input_hash = [0x91; 32];
        let nonce = [0x92; 32];
        let context = authorize_query(&backend, "query-recovery-auth", nonce, input_hash).await;
        let payload = serde_json::json!({"prompt": "durable query result"});
        let provider_key = provider.inner.create_key("0x4").await.unwrap();

        assert!(matches!(
            backend.query(&context, input_hash, "0x4", &payload).await,
            Err(BackendError::Provider(ProviderError::Unavailable))
        ));
        assert_eq!(provider.inner.final_usage(&provider_key).await.unwrap(), 1);
        assert_eq!(provider_query_state(&provider_path, "0x4"), (1, true, 1));

        server.abort();
        let _ = server.await;
        drop(backend);
        drop(provider);
        drop(provider_http);
        drop(store);

        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let restarted_address = listener.local_addr().unwrap();
        drop(listener);
        let restarted_server = tokio::spawn(warp::serve(routes).run(restarted_address));
        wait_for_server(restarted_address).await;
        let restarted_http =
            HttpProvider::new(format!("http://{restarted_address}"), "operator").unwrap();
        let restarted_provider = Arc::new(LoseAcknowledgementProvider::new(restarted_http.clone()));
        restarted_provider
            .lose_credit_ack
            .store(false, Ordering::SeqCst);
        restarted_provider
            .lose_query_ack
            .store(false, Ordering::SeqCst);
        restarted_provider
            .lose_disable_ack
            .store(false, Ordering::SeqCst);
        let reopened_store = Store::open(&store_path).unwrap();
        let reopened_backend = new_backend(
            reopened_store.clone(),
            restarted_provider,
            master_key,
            test_deployments(),
        );

        let expected = ProviderQueryResult {
            result: serde_json::json!({"accepted": true, "echo": payload}),
            cumulative_usage: 1,
        };
        assert_eq!(
            reopened_backend
                .query(&context, input_hash, "0x4", &payload)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            reopened_backend
                .query(&context, input_hash, "0x4", &payload)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(provider_query_state(&provider_path, "0x4"), (1, true, 1));
        assert_eq!(
            reopened_store
                .binding("0x4")
                .unwrap()
                .unwrap()
                .latest_provider_usage,
            1
        );
        assert_eq!(
            reopened_store
                .claim_request(&nonce, "query", &input_hash, "0x4")
                .unwrap(),
            RequestClaim::Cached(serde_json::to_value(&expected).unwrap())
        );

        restarted_server.abort();
        let _ = restarted_server.await;
        drop(reopened_backend);
        drop(reopened_store);
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn out_of_order_query_replies_persist_both_results_across_restart() {
        let provider_path = db_path("reordered-provider");
        let store_path = db_path("reordered-store");
        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;

        let provider_http = HttpProvider::new(format!("http://{address}"), "operator").unwrap();
        let provider = Arc::new(ReorderedReplyProvider::new(provider_http.clone()));
        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"71".repeat(32)).unwrap();
        let backend = new_backend(
            store.clone(),
            provider.clone(),
            master_key.clone(),
            test_deployments(),
        );
        prepare_binding(&backend).await;
        let first_hash = [0xc1; 32];
        let second_hash = [0xc2; 32];
        let first_context =
            authorize_query(&backend, "reordered-auth-first", [0xc3; 32], first_hash).await;
        let second_context =
            authorize_query(&backend, "reordered-auth-second", [0xc4; 32], second_hash).await;
        let first_payload = serde_json::json!({"prompt": "first"});
        let second_payload = serde_json::json!({"prompt": "second"});

        let first_backend = backend.clone();
        let first_request = tokio::spawn(async move {
            first_backend
                .query(&first_context, first_hash, "0x4", &first_payload)
                .await
        });
        provider.first_committed.wait().await;

        let second_backend = backend.clone();
        let second_request = tokio::spawn(async move {
            second_backend
                .query(&second_context, second_hash, "0x4", &second_payload)
                .await
        });
        let second_result = second_request.await.unwrap().unwrap();
        assert_eq!(
            second_result,
            ProviderQueryResult {
                result: serde_json::json!({
                    "accepted": true,
                    "echo": {"prompt": "second"}
                }),
                cumulative_usage: 2,
            }
        );
        provider.release_first.wait().await;
        let first_result = first_request.await.unwrap().unwrap();
        assert_eq!(
            first_result,
            ProviderQueryResult {
                result: serde_json::json!({
                    "accepted": true,
                    "echo": {"prompt": "first"}
                }),
                cumulative_usage: 1,
            }
        );
        assert_eq!(provider_query_state(&provider_path, "0x4"), (2, true, 2));
        assert_eq!(
            store.binding("0x4").unwrap().unwrap().latest_provider_usage,
            2
        );
        assert!(matches!(
            store.update_provider_usage("0x4", 1),
            Err(StorageError::UsageRegression)
        ));

        drop(backend);
        drop(store);
        let reopened_store = Store::open(&store_path).unwrap();
        let reopened_backend = new_backend(
            reopened_store.clone(),
            provider.clone(),
            master_key,
            test_deployments(),
        );
        assert_eq!(
            reopened_backend
                .query(
                    &AuthContext {
                        leader_id: "reordered-auth-first".to_owned(),
                        leader_key_id: 0,
                        input_hash: first_hash,
                        leader_signature: [0; 64],
                        nonce: [0xc3; 32],
                    },
                    first_hash,
                    "0x4",
                    &serde_json::json!({"prompt": "first"})
                )
                .await
                .unwrap(),
            first_result
        );
        assert_eq!(
            reopened_backend
                .query(
                    &AuthContext {
                        leader_id: "reordered-auth-second".to_owned(),
                        leader_key_id: 0,
                        input_hash: second_hash,
                        leader_signature: [0; 64],
                        nonce: [0xc4; 32],
                    },
                    second_hash,
                    "0x4",
                    &serde_json::json!({"prompt": "second"})
                )
                .await
                .unwrap(),
            second_result
        );
        assert_eq!(provider.query_calls.load(Ordering::SeqCst), 2);
        assert_eq!(provider_query_state(&provider_path, "0x4"), (2, true, 2));
        assert_eq!(
            reopened_store
                .binding("0x4")
                .unwrap()
                .unwrap()
                .latest_provider_usage,
            2
        );

        server.abort();
        let _ = server.await;
        drop(reopened_backend);
        drop(reopened_store);
        drop(provider);
        drop(provider_http);
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn completed_query_replays_after_full_retirement_without_deployment_configuration() {
        let provider_path = db_path("revoked-replay-provider");
        let store_path = db_path("revoked-replay-store");
        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;

        let provider = HttpProvider::new(format!("http://{address}"), "operator").unwrap();
        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"72".repeat(32)).unwrap();
        let backend = new_backend(
            store.clone(),
            Arc::new(provider.clone()),
            master_key.clone(),
            test_deployments(),
        );
        prepare_binding(&backend).await;
        let input_hash = [0xd1; 32];
        let nonce = [0xd2; 32];
        let context = authorize_query(&backend, "revoked-replay-auth", nonce, input_hash).await;
        let payload = serde_json::json!({"prompt": "replay after revoke"});
        let expected = backend
            .query(&context, input_hash, "0x4", &payload)
            .await
            .unwrap();
        assert_eq!(provider_query_state(&provider_path, "0x4"), (1, true, 1));

        backend
            .process_event(DecodedEvent {
                event_id: "revoked-replay-event".to_owned(),
                type_tag: "0xabc::accounting::RevokedEvent<0x2::sui::SUI>".to_owned(),
                event: AgentApiEvent::Revoked {
                    cashier_id: "0x2".to_owned(),
                    wallet_id: "0x3".to_owned(),
                    binding_id: "0x4".to_owned(),
                },
            })
            .await
            .unwrap();
        assert!(backend.check_grant(&context, "query").is_err());
        server.abort();
        let _ = server.await;

        let unavailable_provider =
            Arc::new(HttpProvider::new(format!("http://{address}"), "operator").unwrap());
        let replay_backend = new_backend(
            store.clone(),
            unavailable_provider.clone(),
            master_key.clone(),
            test_deployments(),
        );
        assert_eq!(
            replay_backend
                .query(&context, input_hash, "0x4", &payload)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(provider_query_state(&provider_path, "0x4"), (1, false, 1));
        assert_eq!(
            store.binding("0x4").unwrap().unwrap().latest_provider_usage,
            1
        );

        let deployment = test_deployments().remove(0);
        let settlement = RecordingSettlementExecutor::default();
        assert_eq!(
            process_pending_settlements(
                &store,
                &[SettlementRoute::new(deployment.event_filter(), settlement)],
            )
            .await
            .unwrap(),
            1
        );
        let refundable_coin_units = store.pending_settlements().unwrap()[0].refundable_coin_units;
        store
            .record_refund_without_event_for_test("0x4", "0x3", refundable_coin_units)
            .unwrap();
        assert_eq!(store.binding("0x4").unwrap().unwrap().status, "refunded");
        assert!(store.pending_settlements().unwrap().is_empty());

        drop(replay_backend);
        drop(backend);
        drop(store);
        let retired_store = Store::open(&store_path).unwrap();
        assert!(retired_store
            .bindings_requiring_tracking()
            .unwrap()
            .is_empty());
        let retired_backend = Backend::new(
            retired_store.clone(),
            unavailable_provider,
            master_key,
            Vec::new(),
        )
        .unwrap();
        assert!(!retired_backend.health().await);
        assert_eq!(
            retired_backend
                .query(&context, input_hash, "0x4", &payload)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            retired_store
                .binding("0x4")
                .unwrap()
                .unwrap()
                .latest_provider_usage,
            1
        );
        drop(retired_backend);
        drop(retired_store);
        drop(provider);
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn revoke_waits_for_committed_usage_and_rejects_provider_call_after_disable() {
        let provider_path = db_path("revoke-race-provider");
        let store_path = db_path("revoke-race-store");
        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;

        let provider_http = HttpProvider::new(format!("http://{address}"), "operator").unwrap();
        let provider = Arc::new(RevokeRaceProvider::new(provider_http.clone()));
        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"66".repeat(32)).unwrap();
        let backend = new_backend(
            store.clone(),
            provider.clone(),
            master_key,
            test_deployments(),
        );
        prepare_binding(&backend).await;
        let first_hash = [0xa1; 32];
        let second_hash = [0xb1; 32];
        let first_context =
            authorize_query(&backend, "revoke-race-auth-1", [0xa2; 32], first_hash).await;
        let second_context =
            authorize_query(&backend, "revoke-race-auth-2", [0xb2; 32], second_hash).await;
        let first_payload = serde_json::json!({"prompt": "committed before disable"});
        let second_payload = serde_json::json!({"prompt": "provider call after disable"});
        let provider_key = provider.inner.create_key("0x4").await.unwrap();

        let first_backend = backend.clone();
        let first_request = tokio::spawn(async move {
            first_backend
                .query(&first_context, first_hash, "0x4", &first_payload)
                .await
        });
        provider.first_committed.wait().await;

        let second_backend = backend.clone();
        let second_request = tokio::spawn(async move {
            second_backend
                .query(&second_context, second_hash, "0x4", &second_payload)
                .await
        });
        provider.second_ready.wait().await;

        backend
            .process_event(DecodedEvent {
                event_id: "revoke-race-event".to_owned(),
                type_tag: "0xabc::accounting::RevokedEvent<0x2::sui::SUI>".to_owned(),
                event: AgentApiEvent::Revoked {
                    cashier_id: "0x2".to_owned(),
                    wallet_id: "0x3".to_owned(),
                    binding_id: "0x4".to_owned(),
                },
            })
            .await
            .unwrap();
        let pending = store.pending_settlements().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (pending[0].final_usage, pending[0].refundable_coin_units),
            (1, 99)
        );
        assert_eq!(provider.inner.final_usage(&provider_key).await.unwrap(), 1);

        provider.release_second.wait().await;
        assert!(matches!(
            second_request.await.unwrap(),
            Err(BackendError::Provider(ProviderError::Rejected))
        ));
        assert_eq!(provider_query_state(&provider_path, "0x4"), (1, false, 1));

        provider.release_first.wait().await;
        assert_eq!(
            first_request.await.unwrap().unwrap(),
            ProviderQueryResult {
                result: serde_json::json!({
                    "accepted": true,
                    "echo": {"prompt": "committed before disable"}
                }),
                cumulative_usage: 1,
            }
        );
        assert_eq!(
            store.binding("0x4").unwrap().unwrap().latest_provider_usage,
            1
        );
        assert!(backend
            .check_grant(
                &AuthContext {
                    leader_id: "revoked-grant".to_owned(),
                    leader_key_id: 0,
                    input_hash: first_hash,
                    leader_signature: [0; 64],
                    nonce: [0xa2; 32],
                },
                "query"
            )
            .is_err());

        let filter = EventFilter {
            rpc_url: "http://127.0.0.1:9000".to_owned(),
            package_id: "0xabc".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x2::sui::SUI".to_owned(),
            cashier_id: "0x2".to_owned(),
        };
        let executor = Arc::new(RecordingSettlementExecutor::default());
        let routes = vec![SettlementRoute::new(filter.clone(), Arc::clone(&executor))];
        assert_eq!(
            process_pending_settlements(&store, &routes).await.unwrap(),
            1
        );
        assert_eq!(
            *executor.calls.lock().unwrap(),
            vec![
                SettlementObservation {
                    operation: "settle",
                    final_usage: 1,
                    recognized_coin_units: 0,
                    refundable_coin_units: 0,
                },
                SettlementObservation {
                    operation: "refund",
                    final_usage: 1,
                    recognized_coin_units: 1,
                    refundable_coin_units: 99,
                },
            ]
        );
        assert_eq!(
            store.binding("0x4").unwrap().unwrap().recognized_coin_units,
            1
        );
        assert_eq!(
            process_pending_settlements(&store, &routes).await.unwrap(),
            1
        );
        assert_eq!(executor.calls.lock().unwrap().len(), 2);
        backend
            .process_event(DecodedEvent {
                event_id: "revoke-race-refund-confirmed".to_owned(),
                type_tag: "0xabc::accounting::RefundedEvent<0x2::sui::SUI>".to_owned(),
                event: AgentApiEvent::Refunded {
                    cashier_id: "0x2".to_owned(),
                    wallet_id: "0x3".to_owned(),
                    binding_id: "0x4".to_owned(),
                    coin_units: 99,
                },
            })
            .await
            .unwrap();
        let refunded = store.binding("0x4").unwrap().unwrap();
        assert_eq!(refunded.status, "refunded");
        assert!(refunded.refunded);
        assert!(store.pending_settlements().unwrap().is_empty());
        assert_eq!(provider_query_state(&provider_path, "0x4"), (1, false, 1));

        server.abort();
        let _ = server.await;
        drop(backend);
        drop(provider);
        drop(provider_http);
        drop(store);
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn refunded_event_recovers_the_unjournaled_crash_state_after_restart() {
        let provider_path = db_path("refund-crash-provider");
        let store_path = db_path("refund-crash-store");
        let provider_routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let provider_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        drop(provider_listener);
        let provider_server = tokio::spawn(warp::serve(provider_routes).run(provider_address));
        wait_for_server(provider_address).await;
        let provider = HttpProvider::new(format!("http://{provider_address}"), "operator").unwrap();

        let mut store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"61".repeat(32)).unwrap();
        let original_refund = BindingRecord {
            binding_id: "0x41".to_owned(),
            cashier_id: "0x22".to_owned(),
            wallet_id: "0x31".to_owned(),
            owner: "0x51".to_owned(),
            expected_agent_uid: "0x61".to_owned(),
            origin: Some(test_origin("0x22")),
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
        };
        store.insert_registration(&original_refund).unwrap();
        store
            .set_provider_key_box(
                "0x41",
                &seal_at_rest(&master_key, "0x41", b"synthetic-refund-key").unwrap(),
            )
            .unwrap();
        store
            .record_charge("prior-charge-a", "0x41", 100, 200)
            .unwrap();
        store.revoke_binding("0x41").unwrap();
        store.update_provider_usage("0x41", 40).unwrap();
        store.record_usage_settled("0x41", 40, 20).unwrap();
        store
            .record_refund_without_event_for_test("0x41", "0x31", 80)
            .unwrap();
        assert!(!store.has_chain_event("crash-runtime:0").unwrap());

        let following = BindingRecord {
            binding_id: "0x42".to_owned(),
            cashier_id: "0x22".to_owned(),
            wallet_id: "0x32".to_owned(),
            owner: "0x52".to_owned(),
            expected_agent_uid: "0x62".to_owned(),
            origin: Some(test_origin("0x22")),
            rate: 1,
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
        };
        store.insert_registration(&following).unwrap();
        let following_key = provider.create_key("0x42").await.unwrap();
        store
            .set_provider_key_box(
                "0x42",
                &seal_at_rest(&master_key, "0x42", following_key.as_bytes()).unwrap(),
            )
            .unwrap();

        let pending = BindingRecord {
            binding_id: "0x43".to_owned(),
            cashier_id: "0x22".to_owned(),
            wallet_id: "0x33".to_owned(),
            owner: "0x53".to_owned(),
            expected_agent_uid: "0x63".to_owned(),
            origin: Some(test_origin("0x22")),
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
        };
        store.insert_registration(&pending).unwrap();
        store
            .set_provider_key_box(
                "0x43",
                &seal_at_rest(&master_key, "0x43", b"synthetic-pending-key").unwrap(),
            )
            .unwrap();
        store
            .record_charge("prior-charge-c", "0x43", 100, 200)
            .unwrap();
        store.revoke_binding("0x43").unwrap();
        store.update_provider_usage("0x43", 40).unwrap();
        store.queue_settlement("0x43", 40, 80).unwrap();

        let package = "0xabc";
        let coin = "0x2::sui::SUI";
        let event_refund = rpc_event(
            "crash-runtime",
            0,
            format!("{package}::accounting::RefundedEvent<{coin}>"),
            serde_json::json!({
                "cashier_id": "0x22", "wallet_id": "0x31", "binding_id": "0x41", "coin_units": 80
            }),
        );
        let event_charge = rpc_event(
            "crash-runtime",
            1,
            format!("{package}::accounting::ChargeEvent<{coin}>"),
            serde_json::json!({
                "cashier_id": "0x22", "wallet_id": "0x32", "binding_id": "0x42",
                "coin_units": 50, "credit_units": 50, "rate": 1
            }),
        );
        let event_response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "data": [event_refund, event_charge],
                "nextCursor": {"txDigest": "crash-runtime", "eventSeq": "1"},
                "hasNextPage": false
            }
        });
        let event_route = warp::post()
            .and(warp::body::json())
            .map(move |_request: Value| warp::reply::json(&event_response));
        let event_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let event_address = event_listener.local_addr().unwrap();
        drop(event_listener);
        let event_server = tokio::spawn(warp::serve(event_route).run(event_address));
        wait_for_server(event_address).await;
        let deployment = deployment(
            &format!("http://{event_address}"),
            package,
            coin,
            "0x22",
            "0xc1",
        );

        drop(store);
        store = Store::open(&store_path).unwrap();
        let backend = new_backend(
            store.clone(),
            Arc::new(provider.clone()),
            master_key,
            vec![deployment.clone()],
        );
        let settlement = Arc::new(RecordingSettlementExecutor::default());
        let worker = SettlementWorker::new(
            backend.clone(),
            vec![SettlementRoute::new(
                deployment.event_filter(),
                Arc::clone(&settlement),
            )],
        );
        assert_eq!(worker.run_once().await.unwrap(), (2, 1));

        let recovered = store.binding("0x41").unwrap().unwrap();
        assert_eq!(recovered.status, "refunded");
        assert!(recovered.refunded);
        assert_eq!(recovered.recognized_coin_units, 20);
        assert!(store
            .chain_event_matches(
                "crash-runtime:0",
                "0xabc::accounting::RefundedEvent<0x2::sui::SUI>",
                &serde_json::to_value(AgentApiEvent::Refunded {
                    cashier_id: "0x22".to_owned(),
                    wallet_id: "0x31".to_owned(),
                    binding_id: "0x41".to_owned(),
                    coin_units: 80,
                })
                .unwrap()
            )
            .unwrap());
        assert_eq!(store.binding("0x42").unwrap().unwrap().credit_units, 50);
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x42"), 50);
        assert!(store.has_chain_event("crash-runtime:1").unwrap());
        let source = crate::events::SuiEventSource::new(deployment.event_filter()).unwrap();
        assert_eq!(
            store.event_cursor(source.source_name()).unwrap(),
            Some(serde_json::json!({"txDigest":"crash-runtime", "eventSeq":"1"}))
        );
        assert_eq!(
            store
                .binding("0x43")
                .unwrap()
                .unwrap()
                .recognized_coin_units,
            20
        );
        assert_eq!(settlement.calls.lock().unwrap().len(), 2);
        assert_eq!(
            store.pending_settlements().unwrap()[0].state,
            "refund_submitted"
        );

        let conflicting_amount = DecodedEvent {
            event_id: "crash-runtime:0".to_owned(),
            type_tag: "0xabc::accounting::RefundedEvent<0x2::sui::SUI>".to_owned(),
            event: AgentApiEvent::Refunded {
                cashier_id: "0x22".to_owned(),
                wallet_id: "0x31".to_owned(),
                binding_id: "0x41".to_owned(),
                coin_units: 79,
            },
        };
        assert!(backend.process_event(conflicting_amount).await.is_err());
        let conflicting_wallet = DecodedEvent {
            event_id: "crash-runtime:5".to_owned(),
            type_tag: "0xabc::accounting::RefundedEvent<0x2::sui::SUI>".to_owned(),
            event: AgentApiEvent::Refunded {
                cashier_id: "0x22".to_owned(),
                wallet_id: "0x34".to_owned(),
                binding_id: "0x41".to_owned(),
                coin_units: 80,
            },
        };
        assert!(backend.process_event(conflicting_wallet).await.is_err());
        assert!(!store.has_chain_event("crash-runtime:5").unwrap());
        assert_eq!(settlement.calls.lock().unwrap().len(), 2);

        event_server.abort();
        provider_server.abort();
        let _ = event_server.await;
        let _ = provider_server.await;
        drop(worker);
        drop(backend);
        drop(store);
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn one_runtime_polls_and_routes_two_coin_deployments_across_restart() {
        let provider_path = db_path("multi-deployment-provider");
        let store_path = db_path("multi-deployment-store");
        let provider_routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let provider_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        drop(provider_listener);
        let provider_server = tokio::spawn(warp::serve(provider_routes).run(provider_address));
        wait_for_server(provider_address).await;
        let provider = HttpProvider::new(format!("http://{provider_address}"), "operator").unwrap();

        let event_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let event_address = event_listener.local_addr().unwrap();
        drop(event_listener);
        let rpc_url = format!("http://{event_address}");
        let deployments = vec![
            deployment(&rpc_url, "0xa1", "0x2::sui::SUI", "0x22", "0xc1"),
            deployment(
                &rpc_url,
                "0xb1",
                "0x123::test_coin::TEST_COIN",
                "0x23",
                "0xc2",
            ),
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
                let config = event_deployments
                    .iter()
                    .find(|config| config.package_id == package);
                let (events, digest, cursor_seq) = if let Some(config) = config {
                    let (binding_id, wallet_id, owner, agent_uid, nonce) =
                        if config.cashier_id == "0x22" {
                            ("0x41", "0x31", "0x51", "0x61", 0x71_u8)
                        } else {
                            ("0x42", "0x32", "0x52", "0x62", 0x72_u8)
                        };
                    let digest = format!("multi-{binding_id}");
                    let prefix = format!("{}::{}::", config.package_id, config.module);
                    let coin = &config.coin_type;
                    match cursor {
                        None => (
                            vec![
                                rpc_event(
                                    &digest,
                                    0,
                                    format!("{prefix}RegistrationEvent<{coin}>"),
                                    serde_json::json!({
                                        "cashier_id": config.cashier_id,
                                        "wallet_id": wallet_id,
                                        "binding_id": binding_id,
                                        "owner": owner,
                                        "expected_agent_uid": agent_uid,
                                        "rate": 2,
                                        "export_enabled": false,
                                        "owner_public_key": []
                                    }),
                                ),
                                rpc_event(
                                    &digest,
                                    1,
                                    format!("{prefix}ChargeEvent<{coin}>"),
                                    serde_json::json!({
                                        "cashier_id": config.cashier_id,
                                        "wallet_id": wallet_id,
                                        "binding_id": binding_id,
                                        "coin_units": 100,
                                        "credit_units": 200,
                                        "rate": 2
                                    }),
                                ),
                                rpc_event(
                                    &digest,
                                    2,
                                    format!("{prefix}AuthorizationEvent<{coin}>"),
                                    serde_json::json!({
                                        "cashier_id": config.cashier_id,
                                        "binding_id": binding_id,
                                        "target_nonce": vec![nonce; 32],
                                        "input_hash": vec![nonce.wrapping_add(1); 32],
                                        "operation": b"query"
                                    }),
                                ),
                            ],
                            digest,
                            "2",
                        ),
                        Some("2") => (
                            vec![rpc_event(
                                &digest,
                                3,
                                format!("{prefix}RevokedEvent<{coin}>"),
                                serde_json::json!({
                                    "cashier_id": config.cashier_id,
                                    "wallet_id": wallet_id,
                                    "binding_id": binding_id
                                }),
                            )],
                            digest,
                            "3",
                        ),
                        Some("3") => (
                            vec![rpc_event(
                                &digest,
                                4,
                                format!("{prefix}RefundedEvent<{coin}>"),
                                serde_json::json!({
                                    "cashier_id": config.cashier_id,
                                    "wallet_id": wallet_id,
                                    "binding_id": binding_id,
                                    "coin_units": 100
                                }),
                            )],
                            digest,
                            "4",
                        ),
                        _ => (Vec::new(), digest, "4"),
                    }
                } else {
                    (Vec::new(), "unconfigured".to_owned(), "0")
                };
                warp::reply::json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "data": events,
                        "nextCursor": {"txDigest": digest, "eventSeq": cursor_seq},
                        "hasNextPage": false
                    }
                }))
            });
        let event_server = tokio::spawn(warp::serve(event_route).run(event_address));
        wait_for_server(event_address).await;

        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"62".repeat(32)).unwrap();
        let backend = new_backend(
            store.clone(),
            Arc::new(provider.clone()),
            master_key.clone(),
            deployments.clone(),
        );
        let settlement = BuildingSettlementExecutor::default();
        let routes = deployments
            .iter()
            .map(|deployment| SettlementRoute::new(deployment.event_filter(), settlement.clone()))
            .collect();
        let worker = SettlementWorker::new(backend, routes);
        assert_eq!(worker.run_once().await.unwrap(), (6, 0));
        assert_eq!(store.binding("0x41").unwrap().unwrap().credit_units, 200);
        assert_eq!(store.binding("0x42").unwrap().unwrap().credit_units, 200);
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x41"), 200);
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x42"), 200);
        for deployment in &deployments {
            let source = crate::events::SuiEventSource::new(deployment.event_filter()).unwrap();
            assert_eq!(
                store.event_cursor(source.source_name()).unwrap(),
                Some(serde_json::json!({
                    "txDigest": format!("multi-{}", if deployment.cashier_id == "0x22" { "0x41" } else { "0x42" }),
                    "eventSeq": "2"
                }))
            );
        }

        drop(worker);
        let reopened = Store::open(&store_path).unwrap();
        let restarted_backend = new_backend(
            reopened.clone(),
            Arc::new(provider.clone()),
            master_key,
            deployments.clone(),
        );
        let restarted_routes = deployments
            .iter()
            .map(|deployment| SettlementRoute::new(deployment.event_filter(), settlement.clone()))
            .collect();
        let restarted_worker = SettlementWorker::new(restarted_backend.clone(), restarted_routes);
        assert_eq!(restarted_worker.run_once().await.unwrap(), (2, 2));
        assert_eq!(provider_query_state(&provider_path, "0x41"), (0, false, 0));
        assert_eq!(provider_query_state(&provider_path, "0x42"), (0, false, 0));
        assert_eq!(reopened.binding("0x41").unwrap().unwrap().status, "revoked");
        assert_eq!(reopened.binding("0x42").unwrap().unwrap().status, "revoked");

        let observations = settlement.calls.lock().unwrap().clone();
        assert_eq!(observations.len(), 4);
        for (binding_id, cashier_id, package_id, coin_type) in [
            ("0x41", "0x22", "0xa1", "0x2::sui::SUI"),
            ("0x42", "0x23", "0xb1", "0x123::test_coin::TEST_COIN"),
        ] {
            let routed = observations
                .iter()
                .filter(|call| call.binding_id == binding_id)
                .collect::<Vec<_>>();
            assert_eq!(routed.len(), 2);
            assert!(routed.iter().all(|call| {
                call.cashier_id == cashier_id
                    && call.package_id == package_id
                    && call.coin_type == coin_type
                    && call.ptb_coin_type == TypeTag::from_str(coin_type).unwrap().to_string()
            }));
        }

        let third_stage_backend = restarted_backend.clone();
        let third_stage_worker = SettlementWorker::new(
            third_stage_backend.clone(),
            deployments
                .iter()
                .map(|deployment| {
                    SettlementRoute::new(deployment.event_filter(), settlement.clone())
                })
                .collect(),
        );
        assert_eq!(third_stage_worker.run_once().await.unwrap(), (2, 0));
        assert_eq!(
            reopened.binding("0x41").unwrap().unwrap().status,
            "refunded"
        );
        assert_eq!(
            reopened.binding("0x42").unwrap().unwrap().status,
            "refunded"
        );
        assert_eq!(reopened.pending_settlements().unwrap().len(), 0);
        assert_eq!(settlement.calls.lock().unwrap().len(), 4);

        let cross_binding = BindingRecord {
            binding_id: "0x49".to_owned(),
            cashier_id: "0x22".to_owned(),
            wallet_id: "0x39".to_owned(),
            owner: "0x59".to_owned(),
            expected_agent_uid: "0x69".to_owned(),
            origin: Some(deployments[0].origin().unwrap()),
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
        };
        reopened.insert_registration(&cross_binding).unwrap();
        reopened
            .set_provider_key_box("0x49", b"synthetic-cross-deployment-key")
            .unwrap();
        reopened
            .record_charge("cross-charge", "0x49", 100, 200)
            .unwrap();
        reopened.revoke_binding("0x49").unwrap();
        reopened.update_provider_usage("0x49", 0).unwrap();
        reopened.queue_settlement("0x49", 0, 100).unwrap();
        let wrong_route = [SettlementRoute::new(
            deployments[1].event_filter(),
            settlement.clone(),
        )];
        assert!(process_pending_settlements(&reopened, &wrong_route)
            .await
            .is_err());
        assert_eq!(settlement.calls.lock().unwrap().len(), 4);

        event_server.abort();
        provider_server.abort();
        let _ = event_server.await;
        let _ = provider_server.await;
        drop(third_stage_worker);
        drop(restarted_worker);
        drop(restarted_backend);
        drop(reopened);
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn event_replay_is_idempotent_and_revoke_freezes_grants_and_provider_key() {
        let provider_path = db_path("provider");
        let store_path = db_path("store");
        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;
        let provider_http = HttpProvider::new(format!("http://{address}"), "operator").unwrap();
        let provider = Arc::new(LoseAcknowledgementProvider::new(provider_http.clone()));
        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"55".repeat(32)).unwrap();
        let backend = new_backend(
            store.clone(),
            provider.clone(),
            master_key.clone(),
            test_deployments(),
        );
        let registration = DecodedEvent {
            event_id: "register-1".to_owned(),
            type_tag: "0xabc::accounting::RegistrationEvent<0x2::sui::SUI>".to_owned(),
            event: AgentApiEvent::Registered {
                cashier_id: "0x2".to_owned(),
                wallet_id: "0x3".to_owned(),
                binding_id: "0x4".to_owned(),
                owner: "0x5".to_owned(),
                expected_agent_uid: "0x6".to_owned(),
                rate: 2,
                export_enabled: false,
                owner_public_key: vec![],
            },
        };
        backend.process_event(registration.clone()).await.unwrap();
        backend.process_event(registration).await.unwrap();
        let charge = DecodedEvent {
            event_id: "charge-1".to_owned(),
            type_tag: "0xabc::accounting::ChargeEvent<0x2::sui::SUI>".to_owned(),
            event: AgentApiEvent::Charged {
                cashier_id: "0x2".to_owned(),
                wallet_id: "0x3".to_owned(),
                binding_id: "0x4".to_owned(),
                coin_units: 100,
                credit_units: 200,
                rate: 2,
            },
        };
        assert!(backend.process_event(charge.clone()).await.is_err());
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x4"), 200);
        assert_eq!(store.binding("0x4").unwrap().unwrap().credit_units, 0);
        assert!(!store.has_chain_event("charge-1").unwrap());

        drop(backend);
        drop(provider);
        drop(store);
        let store = Store::open(&store_path).unwrap();
        let backend = new_backend(
            store.clone(),
            Arc::new(provider_http.clone()),
            master_key.clone(),
            test_deployments(),
        );
        backend.process_event(charge.clone()).await.unwrap();
        backend.process_event(charge.clone()).await.unwrap();
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x4"), 200);
        assert_eq!(store.binding("0x4").unwrap().unwrap().credit_units, 200);

        let authorized = DecodedEvent {
            event_id: "authorization-1".to_owned(),
            type_tag: "0xabc::accounting::AuthorizationEvent<0x2::sui::SUI>".to_owned(),
            event: AgentApiEvent::Authorized {
                cashier_id: "0x2".to_owned(),
                binding_id: "0x4".to_owned(),
                target_nonce: [0x37; 32],
                operation: "query".to_owned(),
                input_hash: [0x48; 32],
            },
        };
        backend.process_event(authorized.clone()).await.unwrap();
        let context = AuthContext {
            leader_id: "test-leader".to_owned(),
            leader_key_id: 0,
            input_hash: [0x48; 32],
            leader_signature: [0; 64],
            nonce: [0x37; 32],
        };
        assert_eq!(
            backend.check_grant(&context, "query").unwrap().binding_id,
            "0x4"
        );
        assert_eq!(
            store
                .claim_request(&context.nonce, "query", &context.input_hash, "0x4")
                .unwrap(),
            RequestClaim::Execute
        );
        store
            .finish_query_request(
                &context.nonce,
                &context.input_hash,
                "0x4",
                0,
                &serde_json::json!({"cumulative_usage": 0}),
            )
            .unwrap();
        backend.process_event(authorized).await.unwrap();

        drop(backend);
        drop(store);
        let store = Store::open(&store_path).unwrap();
        let backend = new_backend(
            store.clone(),
            Arc::new(LoseAcknowledgementProvider::new(provider_http.clone())),
            master_key,
            test_deployments(),
        );
        assert_eq!(
            backend.check_grant(&context, "query").unwrap().binding_id,
            "0x4"
        );
        assert_eq!(
            store
                .claim_request(&context.nonce, "query", &context.input_hash, "0x4")
                .unwrap(),
            RequestClaim::Cached(serde_json::json!({"cumulative_usage": 0}))
        );
        let revoke = DecodedEvent {
            event_id: "revoke-1".to_owned(),
            type_tag: "0xabc::accounting::RevokedEvent<0x2::sui::SUI>".to_owned(),
            event: AgentApiEvent::Revoked {
                cashier_id: "0x2".to_owned(),
                wallet_id: "0x3".to_owned(),
                binding_id: "0x4".to_owned(),
            },
        };
        assert!(backend.process_event(revoke.clone()).await.is_err());
        let binding = store.binding("0x4").unwrap().unwrap();
        assert_eq!(binding.status, "revoked");
        assert_eq!(binding.latest_provider_usage, 0);
        assert!(backend.check_grant(&context, "query").is_err());
        backend.process_event(revoke).await.unwrap();
        assert!(store.has_chain_event("revoke-1").unwrap());
        let pending = store.pending_settlements().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (pending[0].final_usage, pending[0].refundable_coin_units),
            (0, 100)
        );
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x4"), 200);
        assert_eq!(
            provider_http
                .final_usage(&provider_http.create_key("0x4").await.unwrap())
                .await
                .unwrap(),
            0
        );
        backend.process_event(charge.clone()).await.unwrap();
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x4"), 200);
        let late_charge = DecodedEvent {
            event_id: "late-charge".to_owned(),
            ..charge
        };
        assert!(backend.process_event(late_charge).await.is_err());
        assert_eq!(provider_recorded_credit_units(&provider_path, "0x4"), 200);
        drop(backend);
        drop(store);
        server.abort();
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn conflicting_grants_across_bindings_and_cashiers_are_rejected_without_blocking_refunds()
    {
        let provider_path = db_path("grant-conflict-provider");
        let store_path = db_path("grant-conflict-store");
        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;

        let provider =
            Arc::new(HttpProvider::new(format!("http://{address}"), "operator").unwrap());
        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"93".repeat(32)).unwrap();
        let mut deployments = test_deployments();
        deployments.push(deployment(
            "http://127.0.0.1:9001",
            "0xdef",
            "0x2::sui::SUI",
            "0x7",
            "0xc2",
        ));
        let backend = new_backend(
            store.clone(),
            provider.clone(),
            master_key,
            deployments.clone(),
        );
        prepare_binding(&backend).await;
        let nonce = [0x37; 32];
        let input_hash = [0x48; 32];
        let context = authorize_query(&backend, "authorized-original", nonce, input_hash).await;

        for (binding_id, cashier_id, package_id, event_prefix) in [
            ("0x5", "0x2", "0xabc", "same-cashier"),
            ("0x6", "0x7", "0xdef", "other-cashier"),
        ] {
            backend
                .process_event(DecodedEvent {
                    event_id: format!("{event_prefix}-register"),
                    type_tag: format!("{package_id}::accounting::RegistrationEvent<0x2::sui::SUI>"),
                    event: AgentApiEvent::Registered {
                        cashier_id: cashier_id.to_owned(),
                        wallet_id: format!("{cashier_id}1"),
                        binding_id: binding_id.to_owned(),
                        owner: "0x5".to_owned(),
                        expected_agent_uid: "0x6".to_owned(),
                        rate: 2,
                        export_enabled: false,
                        owner_public_key: Vec::new(),
                    },
                })
                .await
                .unwrap();
            backend
                .process_event(DecodedEvent {
                    event_id: format!("{event_prefix}-charge"),
                    type_tag: format!("{package_id}::accounting::ChargeEvent<0x2::sui::SUI>"),
                    event: AgentApiEvent::Charged {
                        cashier_id: cashier_id.to_owned(),
                        wallet_id: format!("{cashier_id}1"),
                        binding_id: binding_id.to_owned(),
                        coin_units: 100,
                        credit_units: 200,
                        rate: 2,
                    },
                })
                .await
                .unwrap();
        }

        for (event_id, binding_id, cashier_id, package_id, operation, hash) in [
            (
                "same-cashier-conflict",
                "0x5",
                "0x2",
                "0xabc",
                "query",
                [0x59; 32],
            ),
            (
                "other-cashier-conflict",
                "0x6",
                "0x7",
                "0xdef",
                "retrieve-key",
                [0x69; 32],
            ),
        ] {
            let conflicting = DecodedEvent {
                event_id: event_id.to_owned(),
                type_tag: format!("{package_id}::accounting::AuthorizationEvent<0x2::sui::SUI>"),
                event: AgentApiEvent::Authorized {
                    cashier_id: cashier_id.to_owned(),
                    binding_id: binding_id.to_owned(),
                    target_nonce: nonce,
                    operation: operation.to_owned(),
                    input_hash: hash,
                },
            };
            backend.process_event(conflicting.clone()).await.unwrap();
            backend.process_event(conflicting).await.unwrap();
            assert_eq!(
                store.event_rejection_reason(event_id).unwrap(),
                Some("authorization nonce already has a grant".to_owned())
            );
        }
        assert_eq!(
            backend.check_grant(&context, "query").unwrap().binding_id,
            "0x4"
        );
        assert!(backend
            .query(
                &context,
                input_hash,
                "0x5",
                &serde_json::json!({"prompt": "must not query"}),
            )
            .await
            .is_err());
        backend
            .query(
                &context,
                input_hash,
                "0x4",
                &serde_json::json!({"prompt": "original grant"}),
            )
            .await
            .unwrap();
        assert_eq!(provider_query_state(&provider_path, "0x4"), (1, true, 1));
        assert_eq!(provider_query_state(&provider_path, "0x5"), (0, true, 0));
        assert_eq!(provider_query_state(&provider_path, "0x6"), (0, true, 0));

        for (binding_id, cashier_id, wallet_id, package_id, event_id) in [
            ("0x5", "0x2", "0x21", "0xabc", "same-cashier-revoke"),
            ("0x6", "0x7", "0x71", "0xdef", "other-cashier-revoke"),
        ] {
            backend
                .process_event(DecodedEvent {
                    event_id: event_id.to_owned(),
                    type_tag: format!("{package_id}::accounting::RevokedEvent<0x2::sui::SUI>"),
                    event: AgentApiEvent::Revoked {
                        cashier_id: cashier_id.to_owned(),
                        wallet_id: wallet_id.to_owned(),
                        binding_id: binding_id.to_owned(),
                    },
                })
                .await
                .unwrap();
        }
        let pending = store.pending_settlements().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending
            .iter()
            .all(|record| record.final_usage == 0 && record.refundable_coin_units == 100));
        let settlement = Arc::new(RecordingSettlementExecutor::default());
        let settlement_routes = deployments
            .iter()
            .map(|deployment| SettlementRoute::new(deployment.event_filter(), settlement.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            process_pending_settlements(&store, &settlement_routes)
                .await
                .unwrap(),
            2
        );
        for (binding_id, cashier_id, wallet_id, package_id, event_id) in [
            ("0x5", "0x2", "0x21", "0xabc", "same-cashier-refund"),
            ("0x6", "0x7", "0x71", "0xdef", "other-cashier-refund"),
        ] {
            backend
                .process_event(DecodedEvent {
                    event_id: event_id.to_owned(),
                    type_tag: format!("{package_id}::accounting::RefundedEvent<0x2::sui::SUI>"),
                    event: AgentApiEvent::Refunded {
                        cashier_id: cashier_id.to_owned(),
                        wallet_id: wallet_id.to_owned(),
                        binding_id: binding_id.to_owned(),
                        coin_units: 100,
                    },
                })
                .await
                .unwrap();
            assert!(store.binding(binding_id).unwrap().unwrap().refunded);
        }
        assert!(store.pending_settlements().unwrap().is_empty());

        server.abort();
        let _ = server.await;
        drop(backend);
        drop(provider);
        drop(store);
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test]
    async fn owner_direct_and_nexus_requests_race_for_the_last_provider_credit() {
        let provider_path = db_path("direct-race-provider");
        let store_path = db_path("direct-race-store");
        let routes = mock_routes(&provider_path, "operator".to_owned()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(warp::serve(routes).run(address));
        wait_for_server(address).await;

        let provider =
            Arc::new(HttpProvider::new(format!("http://{address}"), "operator").unwrap());
        let provider_key = provider.create_key("binding-a").await.unwrap();
        provider.credit(&provider_key, 1, "charge-1").await.unwrap();
        let master_key = MasterKey::from_hex(&"66".repeat(32)).unwrap();
        let store = Store::open(&store_path).unwrap();
        store
            .insert_registration(&BindingRecord {
                binding_id: "binding-a".to_owned(),
                cashier_id: "0x2".to_owned(),
                wallet_id: "wallet-a".to_owned(),
                owner: "owner-a".to_owned(),
                expected_agent_uid: "agent-a".to_owned(),
                origin: Some(test_origin("0x2")),
                rate: 1,
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
            })
            .unwrap();
        store
            .set_provider_key_box(
                "binding-a",
                &seal_at_rest(&master_key, "binding-a", provider_key.as_bytes()).unwrap(),
            )
            .unwrap();
        store.record_charge("charge-1", "binding-a", 1, 1).unwrap();
        let input_hash = [0x72; 32];
        let nonce = [0x73; 32];
        store
            .insert_grant(&GrantRecord {
                nonce,
                binding_id: "binding-a".to_owned(),
                operation: "query".to_owned(),
                input_hash,
                event_id: "authorization-1".to_owned(),
                consumed: false,
            })
            .unwrap();
        let backend = new_backend(
            store.clone(),
            provider.clone(),
            master_key,
            test_deployments(),
        );
        let context = AuthContext {
            leader_id: "test-leader".to_owned(),
            leader_key_id: 0,
            input_hash,
            leader_signature: [0; 64],
            nonce,
        };
        let payload = serde_json::json!({"text": "one credit"});
        let (nexus_request, owner_direct_request) = tokio::join!(
            backend.query(&context, input_hash, "binding-a", &payload),
            provider.query(&provider_key, &payload, "owner-direct-1"),
        );
        assert_ne!(nexus_request.is_ok(), owner_direct_request.is_ok());
        assert_eq!(provider.final_usage(&provider_key).await.unwrap(), 1);
        let provider_balance = rusqlite::Connection::open(&provider_path).unwrap();
        let (credited, used): (String, String) = provider_balance
            .query_row(
                "SELECT credit_units, cumulative_usage FROM provider_keys WHERE binding_id = ?1",
                ["binding-a"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((credited.as_str(), used.as_str()), ("1", "1"));

        drop(backend);
        drop(store);
        server.abort();
        let _ = std::fs::remove_file(provider_path);
        let _ = std::fs::remove_file(store_path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_key_export_keeps_one_envelope_after_revoke_and_restart() {
        use x25519_dalek::{PublicKey, StaticSecret};

        let provider = Arc::new(HttpProvider::new("http://127.0.0.1:1", "unused").unwrap());
        let store_path = db_path("key-export-replay");
        let store = Store::open(&store_path).unwrap();
        let master_key = MasterKey::from_hex(&"77".repeat(32)).unwrap();
        let owner_secret = StaticSecret::from([0x31; 32]);
        let owner_public = PublicKey::from(&owner_secret).to_bytes().to_vec();
        let enabled = BindingRecord {
            binding_id: "export-enabled".to_owned(),
            cashier_id: "0x2".to_owned(),
            wallet_id: "wallet-a".to_owned(),
            owner: "owner-a".to_owned(),
            expected_agent_uid: "agent-a".to_owned(),
            origin: Some(test_origin("0x2")),
            rate: 1,
            export_enabled: true,
            owner_public_key: owner_public,
            status: "active".to_owned(),
            provider_key_box: None,
            charged_coin_units: 0,
            credit_units: 0,
            settled_usage: 0,
            recognized_coin_units: 0,
            latest_provider_usage: 0,
            refunded: false,
        };
        let disabled = BindingRecord {
            binding_id: "export-disabled".to_owned(),
            export_enabled: false,
            owner_public_key: Vec::new(),
            ..enabled.clone()
        };
        for binding in [&enabled, &disabled] {
            store.insert_registration(binding).unwrap();
            store
                .set_provider_key_box(
                    &binding.binding_id,
                    &seal_at_rest(&master_key, &binding.binding_id, b"mock-provider-key").unwrap(),
                )
                .unwrap();
            store
                .record_charge(
                    &format!("charge-{}", binding.binding_id),
                    &binding.binding_id,
                    1,
                    1,
                )
                .unwrap();
        }
        let input_hash = [0x81; 32];
        let enabled_nonce = [0x82; 32];
        let disabled_nonce = [0x83; 32];
        for (nonce, binding_id) in [
            (enabled_nonce, "export-enabled"),
            (disabled_nonce, "export-disabled"),
        ] {
            store
                .insert_grant(&GrantRecord {
                    nonce,
                    binding_id: binding_id.to_owned(),
                    operation: "retrieve-key".to_owned(),
                    input_hash,
                    event_id: format!("authorization-{binding_id}"),
                    consumed: false,
                })
                .unwrap();
        }
        let backend = Arc::new(new_backend(
            store.clone(),
            provider.clone(),
            master_key.clone(),
            test_deployments(),
        ));
        let context_for = |nonce| AuthContext {
            leader_id: "test-leader".to_owned(),
            leader_key_id: 0,
            input_hash,
            leader_signature: [0; 64],
            nonce,
        };
        let enabled_context = context_for(enabled_nonce);
        let start = Arc::new(Barrier::new(3));
        let first_backend = Arc::clone(&backend);
        let first_start = Arc::clone(&start);
        let first_context = context_for(enabled_nonce);
        let first = tokio::spawn(async move {
            first_start.wait().await;
            first_backend
                .retrieve_key(&first_context, input_hash, "export-enabled")
                .await
        });
        let second_backend = Arc::clone(&backend);
        let second_start = Arc::clone(&start);
        let second_context = context_for(enabled_nonce);
        let second = tokio::spawn(async move {
            second_start.wait().await;
            second_backend
                .retrieve_key(&second_context, input_hash, "export-enabled")
                .await
        });
        start.wait().await;
        let envelope = first.await.unwrap().unwrap();
        let concurrent_envelope = second.await.unwrap().unwrap();
        assert_eq!(envelope, concurrent_envelope);
        assert_eq!(
            crate::crypto::decrypt_export(&[0x31; 32], &envelope).unwrap(),
            b"mock-provider-key"
        );
        assert!(crate::crypto::decrypt_export(&[0x32; 32], &envelope).is_err());

        store.revoke_binding("export-enabled").unwrap();
        drop(backend);
        drop(store);
        let reopened_store = Store::open(&store_path).unwrap();
        let replay_backend = new_backend(
            reopened_store.clone(),
            provider.clone(),
            master_key,
            test_deployments(),
        );
        assert_eq!(
            replay_backend
                .retrieve_key(&enabled_context, input_hash, "export-enabled")
                .await
                .unwrap(),
            envelope
        );
        let disabled_context = context_for(disabled_nonce);
        assert!(replay_backend
            .retrieve_key(&disabled_context, input_hash, "export-disabled")
            .await
            .is_err());
        drop(replay_backend);
        drop(reopened_store);
        drop(provider);
        let _ = std::fs::remove_file(&store_path);
        let _ = std::fs::remove_file(store_path.with_extension("sqlite-shm"));
        let _ = std::fs::remove_file(store_path.with_extension("sqlite-wal"));
    }
}
