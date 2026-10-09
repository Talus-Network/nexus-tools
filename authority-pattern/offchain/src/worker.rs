use {
    crate::{
        accounting::{coin_units_for_usage, refundable_coin_units},
        backend::{Backend, BackendError, DeploymentConfig},
        events::{EventFilter, EventOrigin},
        settlement::{
            build_settlement,
            OwnedObjectRef,
            SettlementError,
            SettlementPlan,
            SharedObjectRef,
        },
        storage::{BindingRecord, SettlementRecord, StorageError, Store},
    },
    async_trait::async_trait,
    nexus_sdk::{
        nexus::wallet::WalletClient,
        sui::{
            crypto::Ed25519PrivateKey,
            types::{Address, TypeTag},
        },
    },
    reqwest::Client,
    serde_json::{json, Value},
    std::{env, str::FromStr, sync::Arc, time::Duration},
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("event polling failed")]
    Backend(#[from] BackendError),
    #[error("durable settlement state failed")]
    Storage(#[from] StorageError),
    #[error("settlement operation failed")]
    Settlement(#[from] SettlementError),
    #[error("worker configuration is incomplete")]
    Configuration,
    #[error("worker cycle encountered event error ({event}) and settlement error ({settlement})")]
    Cycle { event: String, settlement: String },
}

#[async_trait]
pub trait SettlementExecutor: Send + Sync {
    async fn settle(
        &self,
        filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> Result<(), SettlementError>;

    async fn refund(
        &self,
        filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> Result<(), SettlementError>;
}

#[async_trait]
impl<T: SettlementExecutor + ?Sized> SettlementExecutor for Arc<T> {
    async fn settle(
        &self,
        filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> Result<(), SettlementError> {
        (**self).settle(filter, binding, final_usage).await
    }

    async fn refund(
        &self,
        filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> Result<(), SettlementError> {
        (**self).refund(filter, binding, final_usage).await
    }
}

pub struct SettlementWorker<E> {
    backend: Backend,
    routes: Vec<SettlementRoute<E>>,
}

pub struct SettlementRoute<E> {
    pub filter: EventFilter,
    pub executor: E,
}

impl<E> SettlementRoute<E> {
    pub fn new(filter: EventFilter, executor: E) -> Self {
        Self { filter, executor }
    }
}

impl<E: SettlementExecutor> SettlementWorker<E> {
    pub fn new(backend: Backend, routes: Vec<SettlementRoute<E>>) -> Self {
        Self { backend, routes }
    }

    pub async fn run_once(&self) -> Result<(usize, usize), WorkerError> {
        let events = self.backend.poll_configured_events().await;
        let settlements = process_pending_settlements(&self.backend.store, &self.routes).await;
        match (events, settlements) {
            (Ok(events), Ok(settlements)) => Ok((events, settlements)),
            (Err(event), Ok(_)) => Err(event.into()),
            (Ok(_), Err(settlement)) => Err(settlement),
            (Err(event), Err(settlement)) => Err(WorkerError::Cycle {
                event: event.to_string(),
                settlement: settlement.to_string(),
            }),
        }
    }
}

pub async fn process_pending_settlements<E: SettlementExecutor>(
    store: &Store,
    routes: &[SettlementRoute<E>],
) -> Result<usize, WorkerError> {
    let pending = store.pending_settlements()?;
    let mut finished = 0;
    let mut first_error = None;
    for record in pending {
        let matches = routes
            .iter()
            .filter(|route| route_matches_binding_origin(&route.filter, &record.binding))
            .take(2)
            .collect::<Vec<_>>();
        let route = if matches.len() == 1 {
            matches[0]
        } else {
            first_error.get_or_insert(WorkerError::Configuration);
            continue;
        };
        match process_settlement(store, route, record).await {
            Ok(()) => finished += 1,
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(finished), Err)
}

async fn process_settlement<E: SettlementExecutor>(
    store: &Store,
    route: &SettlementRoute<E>,
    record: SettlementRecord,
) -> Result<(), WorkerError> {
    let binding_id = record.binding.binding_id.as_str();
    let expected_refund = refundable_coin_units(
        record.binding.charged_coin_units,
        record.final_usage,
        record.binding.rate,
    )
    .map_err(SettlementError::Accounting)?;
    if record.binding.status != "revoked"
        || expected_refund != record.refundable_coin_units
        || record.final_usage != record.binding.latest_provider_usage
        || !route_matches_binding_origin(&route.filter, &record.binding)
    {
        return Err(SettlementError::InvalidConfiguration.into());
    }

    match record.state.as_str() {
        "refunded" | "refund_submitted" => return Ok(()),
        "refund_submitting" => {
            let binding = store
                .binding(binding_id)?
                .ok_or(SettlementError::InvalidConfiguration)?;
            route
                .executor
                .refund(&route.filter, &binding, record.final_usage)
                .await
                .map_err(|error| {
                    let _ = store.set_settlement_state(
                        binding_id,
                        "refund_submitting",
                        Some("refund transaction submission failed"),
                    );
                    WorkerError::Settlement(error)
                })?;
            store.set_settlement_state(binding_id, "refund_submitted", None)?;
            return Ok(());
        }
        "pending" | "settling" | "settled" => {}
        _ => return Err(SettlementError::InvalidConfiguration.into()),
    }

    let mut binding = store
        .binding(binding_id)?
        .ok_or(SettlementError::InvalidConfiguration)?;
    if record.state != "settled" {
        store.set_settlement_state(binding_id, "settling", None)?;
        route
            .executor
            .settle(&route.filter, &binding, record.final_usage)
            .await
            .map_err(|error| {
                let _ = store.set_settlement_state(
                    binding_id,
                    "settling",
                    Some("settlement transaction submission failed"),
                );
                WorkerError::Settlement(error)
            })?;
        let recognized = coin_units_for_usage(record.final_usage, binding.rate)
            .map_err(SettlementError::Accounting)?;
        store.record_usage_settled(binding_id, record.final_usage, recognized)?;
        binding = store
            .binding(binding_id)?
            .ok_or(SettlementError::InvalidConfiguration)?;
    }

    store.set_settlement_state(binding_id, "refund_submitting", None)?;
    route
        .executor
        .refund(&route.filter, &binding, record.final_usage)
        .await
        .map_err(|error| {
            let _ = store.set_settlement_state(
                binding_id,
                "refund_submitting",
                Some("refund transaction submission failed"),
            );
            WorkerError::Settlement(error)
        })?;
    store.set_settlement_state(binding_id, "refund_submitted", None)?;
    Ok(())
}

pub struct SuiSettlementExecutor {
    wallet: WalletClient,
    rpc_url: String,
    settlement_cap_id: String,
    gas_budget: u64,
    client: Client,
}

impl SuiSettlementExecutor {
    pub async fn from_env(deployment: &DeploymentConfig) -> Result<Self, SettlementError> {
        let grpc_url = env::var("AGENT_API_SUI_GRPC_URL")
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        let signer_key = env::var("AGENT_API_SETTLEMENT_SIGNER_KEY_B64")
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        let settlement_cap_id = deployment.settlement_cap_id.clone();
        let gas_budget = env::var("AGENT_API_SETTLEMENT_GAS_BUDGET")
            .unwrap_or_else(|_| "50000000".to_owned())
            .parse::<u64>()
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        if grpc_url.is_empty() || settlement_cap_id.is_empty() || gas_budget == 0 {
            return Err(SettlementError::InvalidConfiguration);
        }
        let signer = Ed25519PrivateKey::from_base64(&signer_key)
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        drop(signer_key);
        let wallet = WalletClient::connect(&grpc_url, signer)
            .await
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        Ok(Self {
            wallet,
            rpc_url: deployment.rpc_url.clone(),
            settlement_cap_id,
            gas_budget,
            client,
        })
    }

    async fn plan(
        &self,
        filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> Result<SettlementPlan, SettlementError> {
        if canonical_address(&binding.cashier_id) != canonical_address(&filter.cashier_id) {
            return Err(SettlementError::InvalidConfiguration);
        }
        let cap = self.read_object(&self.settlement_cap_id).await?;
        let cashier = self.read_object(&binding.cashier_id).await?;
        let wallet = self.read_object(&binding.wallet_id).await?;
        let package = Address::from_str(&filter.package_id)
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        let coin = TypeTag::from_str(&filter.coin_type)
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        let cap_type = TypeTag::from_str(&format!("{package}::accounting::SettlementCap<{coin}>"))
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        let cashier_type = TypeTag::from_str(&format!("{package}::accounting::Cashier<{coin}>"))
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        let wallet_type = TypeTag::from_str(&format!("{package}::accounting::AgentWallet<{coin}>"))
            .map_err(|_| SettlementError::InvalidConfiguration)?;
        if cap.type_tag != cap_type
            || cashier.type_tag != cashier_type
            || wallet.type_tag != wallet_type
        {
            return Err(SettlementError::ObjectMetadata);
        }
        let cap_owner = cap
            .owner
            .pointer("/AddressOwner")
            .and_then(Value::as_str)
            .ok_or(SettlementError::ObjectMetadata)?;
        if canonical_address(cap_owner) != canonical_address(&self.wallet.owner().to_string()) {
            return Err(SettlementError::SignerMismatch);
        }
        let initial_shared_version = |owner: &Value| {
            owner
                .pointer("/Shared/initial_shared_version")
                .or_else(|| owner.pointer("/ConsensusAddress/start_version"))
                .and_then(parse_u64)
                .ok_or(SettlementError::ObjectMetadata)
        };
        Ok(SettlementPlan {
            package_id: filter.package_id.clone(),
            coin_type: filter.coin_type.clone(),
            settlement_cap: OwnedObjectRef {
                object_id: cap.object_id,
                version: cap.version,
                digest: cap.digest,
                cashier_id: binding.cashier_id.clone(),
            },
            cashier: SharedObjectRef {
                object_id: cashier.object_id,
                initial_shared_version: initial_shared_version(&cashier.owner)?,
            },
            wallet: SharedObjectRef {
                object_id: wallet.object_id,
                initial_shared_version: initial_shared_version(&wallet.owner)?,
            },
            binding_id: binding.binding_id.clone(),
            charged_coin_units: binding.charged_coin_units,
            credit_units: binding.credit_units,
            rate: binding.rate,
            previous_usage: binding.settled_usage,
            previous_coin_liability: binding.recognized_coin_units,
            final_usage,
        })
    }

    async fn read_object(&self, object_id: &str) -> Result<SuiObjectMetadata, SettlementError> {
        let response = self
            .client
            .post(&self.rpc_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "sui_getObject",
                "params": [object_id, {"showOwner": true, "showType": true}]
            }))
            .send()
            .await
            .map_err(|_| SettlementError::ObjectMetadata)?;
        if !response.status().is_success() {
            return Err(SettlementError::ObjectMetadata);
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| SettlementError::ObjectMetadata)?;
        let data = body
            .pointer("/result/data")
            .filter(|value| !value.is_null())
            .ok_or(SettlementError::ObjectMetadata)?;
        let object_id = data
            .get("objectId")
            .and_then(Value::as_str)
            .ok_or(SettlementError::ObjectMetadata)?
            .to_owned();
        let type_tag = TypeTag::from_str(
            data.get("type")
                .and_then(Value::as_str)
                .ok_or(SettlementError::ObjectMetadata)?,
        )
        .map_err(|_| SettlementError::ObjectMetadata)?;
        Ok(SuiObjectMetadata {
            object_id,
            version: data
                .get("version")
                .and_then(parse_u64)
                .ok_or(SettlementError::ObjectMetadata)?,
            digest: data
                .get("digest")
                .and_then(Value::as_str)
                .ok_or(SettlementError::ObjectMetadata)?
                .to_owned(),
            type_tag,
            owner: data
                .get("owner")
                .cloned()
                .ok_or(SettlementError::ObjectMetadata)?,
        })
    }

    async fn submit(
        &self,
        transaction: nexus_sdk::sui::types::ProgrammableTransaction,
    ) -> Result<(), SettlementError> {
        let transaction = self
            .wallet
            .prepare_transaction(transaction, self.gas_budget)
            .await
            .map_err(|_| SettlementError::Submission)?;
        self.wallet
            .execute_transaction(transaction)
            .await
            .map_err(|_| SettlementError::Submission)?;
        Ok(())
    }
}

#[async_trait]
impl SettlementExecutor for SuiSettlementExecutor {
    async fn settle(
        &self,
        filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> Result<(), SettlementError> {
        let plan = self.plan(filter, binding, final_usage).await?;
        let built = build_settlement(&plan)?;
        self.submit(built.settle).await
    }

    async fn refund(
        &self,
        filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> Result<(), SettlementError> {
        let plan = self.plan(filter, binding, final_usage).await?;
        let built = build_settlement(&plan)?;
        self.submit(built.refund).await
    }
}

struct SuiObjectMetadata {
    object_id: String,
    version: u64,
    digest: String,
    type_tag: TypeTag,
    owner: Value,
}

fn parse_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn canonical_address(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    let digits = normalized
        .strip_prefix("0x")
        .unwrap_or(&normalized)
        .trim_start_matches('0');
    format!("0x{}", if digits.is_empty() { "0" } else { digits })
}

fn route_matches_binding_origin(filter: &EventFilter, binding: &BindingRecord) -> bool {
    let Some(binding_origin) = binding.origin.as_ref() else {
        return false;
    };
    EventOrigin::from_filter(filter).is_ok_and(|route_origin| route_origin.matches(binding_origin))
}

pub async fn run() -> Result<(), WorkerError> {
    run_worker().await
}

async fn run_worker() -> Result<(), WorkerError> {
    let backend = Backend::from_env()?;
    if backend.deployments().is_empty() {
        return Err(WorkerError::Configuration);
    }
    let mut routes = Vec::with_capacity(backend.deployments().len());
    for deployment in backend.deployments() {
        routes.push(SettlementRoute {
            filter: deployment.event_filter(),
            executor: SuiSettlementExecutor::from_env(deployment).await?,
        });
    }
    let worker = SettlementWorker::new(backend, routes);
    loop {
        if worker.run_once().await.is_err() {
            eprintln!("agent-api worker iteration failed; durable work will be retried");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::storage::BindingRecord,
        std::{
            collections::HashSet,
            path::PathBuf,
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
                Mutex,
            },
        },
    };

    #[derive(Default)]
    struct MockExecutor {
        calls: Mutex<Vec<SettlementCall>>,
        fail_refund: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SettlementCall {
        operation: &'static str,
        binding_id: String,
        cashier_id: String,
        wallet_id: String,
        final_usage: u64,
    }

    #[derive(Default)]
    struct RefundEffects {
        attempts: usize,
        moved_coin_units: u64,
        refund_ids: HashSet<String>,
        settle_calls: usize,
    }

    struct LostRefundAckExecutor {
        effects: Arc<Mutex<RefundEffects>>,
        lose_ack_once: AtomicBool,
    }

    #[async_trait]
    impl SettlementExecutor for LostRefundAckExecutor {
        async fn settle(
            &self,
            _filter: &EventFilter,
            _binding: &BindingRecord,
            _final_usage: u64,
        ) -> Result<(), SettlementError> {
            self.effects.lock().unwrap().settle_calls += 1;
            Ok(())
        }

        async fn refund(
            &self,
            _filter: &EventFilter,
            binding: &BindingRecord,
            _final_usage: u64,
        ) -> Result<(), SettlementError> {
            let refund_id = format!("{}:{}", binding.cashier_id, binding.binding_id);
            let mut effects = self.effects.lock().unwrap();
            effects.attempts += 1;
            if effects.refund_ids.insert(refund_id) {
                effects.moved_coin_units +=
                    binding.charged_coin_units - binding.recognized_coin_units;
            }
            drop(effects);
            if self.lose_ack_once.swap(false, Ordering::SeqCst) {
                Err(SettlementError::Submission)
            } else {
                Ok(())
            }
        }
    }

    impl MockExecutor {
        fn record(&self, operation: &'static str, binding: &BindingRecord, final_usage: u64) {
            self.calls.lock().unwrap().push(SettlementCall {
                operation,
                binding_id: binding.binding_id.clone(),
                cashier_id: binding.cashier_id.clone(),
                wallet_id: binding.wallet_id.clone(),
                final_usage,
            });
        }
    }

    #[async_trait]
    impl SettlementExecutor for MockExecutor {
        async fn settle(
            &self,
            _filter: &EventFilter,
            binding: &BindingRecord,
            final_usage: u64,
        ) -> Result<(), SettlementError> {
            self.record("settle", binding, final_usage);
            Ok(())
        }

        async fn refund(
            &self,
            _filter: &EventFilter,
            binding: &BindingRecord,
            final_usage: u64,
        ) -> Result<(), SettlementError> {
            self.record("refund", binding, final_usage);
            if self.fail_refund {
                Err(SettlementError::Submission)
            } else {
                Ok(())
            }
        }
    }

    fn binding() -> BindingRecord {
        BindingRecord {
            binding_id: "0x4".to_owned(),
            cashier_id: "0x2".to_owned(),
            wallet_id: "0x3".to_owned(),
            owner: "0x5".to_owned(),
            expected_agent_uid: "0x6".to_owned(),
            origin: Some(EventOrigin {
                package_id: "0xabc".to_owned(),
                module: "accounting".to_owned(),
                coin_type: "0x2::sui::SUI".to_owned(),
                cashier_id: "0x2".to_owned(),
            }),
            rate: 2,
            export_enabled: false,
            owner_public_key: vec![],
            status: "active".to_owned(),
            provider_key_box: None,
            charged_coin_units: 100,
            credit_units: 200,
            settled_usage: 0,
            recognized_coin_units: 0,
            latest_provider_usage: 40,
            refunded: false,
        }
    }

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.insert_registration(&binding()).unwrap();
        store.set_provider_key_box("0x4", b"sealed").unwrap();
        store.record_charge("charge-1", "0x4", 100, 200).unwrap();
        store.revoke_binding("0x4").unwrap();
        store.update_provider_usage("0x4", 40).unwrap();
        store.queue_settlement("0x4", 40, 80).unwrap();
        store
    }

    fn disk_store(path: &PathBuf) -> Store {
        let store = Store::open(path).unwrap();
        store.insert_registration(&binding()).unwrap();
        store.set_provider_key_box("0x4", b"sealed").unwrap();
        store.record_charge("charge-1", "0x4", 100, 200).unwrap();
        store.revoke_binding("0x4").unwrap();
        store.update_provider_usage("0x4", 40).unwrap();
        store.queue_settlement("0x4", 40, 80).unwrap();
        store
    }

    fn filter() -> EventFilter {
        EventFilter {
            rpc_url: "http://127.0.0.1:9000".to_owned(),
            package_id: "0xabc".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x2::sui::SUI".to_owned(),
            cashier_id: "0x2".to_owned(),
        }
    }

    #[tokio::test]
    async fn pending_settlement_submits_usage_then_refund_to_original_wallet() {
        let store = store();
        let executor = Arc::new(MockExecutor::default());
        let routes = vec![SettlementRoute::new(filter(), Arc::clone(&executor))];

        assert_eq!(
            process_pending_settlements(&store, &routes).await.unwrap(),
            1
        );
        assert_eq!(
            *executor.calls.lock().unwrap(),
            vec![
                SettlementCall {
                    operation: "settle",
                    binding_id: "0x4".to_owned(),
                    cashier_id: "0x2".to_owned(),
                    wallet_id: "0x3".to_owned(),
                    final_usage: 40,
                },
                SettlementCall {
                    operation: "refund",
                    binding_id: "0x4".to_owned(),
                    cashier_id: "0x2".to_owned(),
                    wallet_id: "0x3".to_owned(),
                    final_usage: 40,
                },
            ]
        );
        assert_eq!(
            store.binding("0x4").unwrap().unwrap().recognized_coin_units,
            20
        );
        assert_eq!(
            store.pending_settlements().unwrap()[0].state,
            "refund_submitted"
        );
    }

    #[tokio::test]
    async fn active_settlement_does_not_block_another_binding_refund_and_later_revoke() {
        let store = Store::open_in_memory().unwrap();
        let mut active = binding();
        active.binding_id = "active-binding".to_owned();
        let mut revoked = binding();
        revoked.binding_id = "revoked-binding".to_owned();
        store.insert_registration(&active).unwrap();
        store.insert_registration(&revoked).unwrap();
        store
            .set_provider_key_box("active-binding", b"sealed-active")
            .unwrap();
        store
            .set_provider_key_box("revoked-binding", b"sealed-revoked")
            .unwrap();
        store
            .record_charge("active-charge", "active-binding", 100, 200)
            .unwrap();
        store
            .record_charge("revoked-charge", "revoked-binding", 100, 200)
            .unwrap();

        store
            .record_usage_settled("active-binding", 40, 20)
            .unwrap();
        store
            .record_usage_settled("active-binding", 40, 20)
            .unwrap();
        assert!(store.pending_settlements().unwrap().is_empty());

        store.revoke_binding("revoked-binding").unwrap();
        store.update_provider_usage("revoked-binding", 50).unwrap();
        store.queue_settlement("revoked-binding", 50, 75).unwrap();
        let pending = store.pending_settlements().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].binding.binding_id, "revoked-binding");
        assert_eq!(pending[0].refundable_coin_units, 75);

        let executor = Arc::new(MockExecutor::default());
        let routes = vec![SettlementRoute::new(filter(), Arc::clone(&executor))];
        assert_eq!(
            process_pending_settlements(&store, &routes).await.unwrap(),
            1
        );
        assert_eq!(
            executor
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| (call.operation, call.binding_id.as_str(), call.final_usage))
                .collect::<Vec<_>>(),
            vec![
                ("settle", "revoked-binding", 50),
                ("refund", "revoked-binding", 50),
            ]
        );
        assert_eq!(
            store
                .binding("active-binding")
                .unwrap()
                .unwrap()
                .recognized_coin_units,
            20
        );

        store.revoke_binding("active-binding").unwrap();
        store.update_provider_usage("active-binding", 40).unwrap();
        store.queue_settlement("active-binding", 40, 80).unwrap();
        assert_eq!(
            process_pending_settlements(&store, &routes).await.unwrap(),
            2
        );
        assert_eq!(
            executor
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| (call.operation, call.binding_id.as_str(), call.final_usage))
                .collect::<Vec<_>>(),
            vec![
                ("settle", "revoked-binding", 50),
                ("refund", "revoked-binding", 50),
                ("settle", "active-binding", 40),
                ("refund", "active-binding", 40),
            ]
        );
        store
            .record_refund_without_event_for_test("revoked-binding", "0x3", 75)
            .unwrap();
        store
            .record_refund_without_event_for_test("active-binding", "0x3", 80)
            .unwrap();
        assert!(store.pending_settlements().unwrap().is_empty());
        assert_eq!(
            store
                .binding("active-binding")
                .unwrap()
                .unwrap()
                .recognized_coin_units,
            20
        );
    }

    #[tokio::test]
    async fn restart_in_refund_phase_never_repeats_settlement() {
        let store = store();
        store.record_usage_settled("0x4", 40, 20).unwrap();
        store
            .set_settlement_state("0x4", "refund_submitting", None)
            .unwrap();
        store.record_usage_settled("0x4", 40, 20).unwrap();
        assert_eq!(
            store.pending_settlements().unwrap()[0].state,
            "refund_submitting"
        );
        let executor = Arc::new(MockExecutor::default());
        let routes = vec![SettlementRoute::new(filter(), Arc::clone(&executor))];

        process_pending_settlements(&store, &routes).await.unwrap();

        assert_eq!(executor.calls.lock().unwrap()[0].operation, "refund");
        assert_eq!(
            store.pending_settlements().unwrap()[0].state,
            "refund_submitted"
        );
    }

    #[tokio::test]
    async fn failed_refund_remains_pending_and_can_be_retried() {
        let store = store();
        let failed = Arc::new(MockExecutor {
            fail_refund: true,
            ..Default::default()
        });
        let failed_routes = vec![SettlementRoute::new(filter(), Arc::clone(&failed))];
        assert!(process_pending_settlements(&store, &failed_routes)
            .await
            .is_err());
        assert_eq!(
            store.pending_settlements().unwrap()[0].state,
            "refund_submitting"
        );

        let retry = Arc::new(MockExecutor::default());
        let retry_routes = vec![SettlementRoute::new(filter(), Arc::clone(&retry))];
        process_pending_settlements(&store, &retry_routes)
            .await
            .unwrap();
        assert_eq!(retry.calls.lock().unwrap()[0].operation, "refund");
    }

    #[tokio::test]
    async fn confirmed_refund_waits_for_chain_event_without_resubmission() {
        let store = store();
        store.record_usage_settled("0x4", 40, 20).unwrap();
        store
            .set_settlement_state("0x4", "refund_submitted", None)
            .unwrap();
        let executor = Arc::new(MockExecutor::default());
        let routes = vec![SettlementRoute::new(filter(), Arc::clone(&executor))];

        process_pending_settlements(&store, &routes).await.unwrap();

        assert!(executor.calls.lock().unwrap().is_empty());
        store
            .record_refund_without_event_for_test("0x4", "0x3", 80)
            .unwrap();
        assert!(store.pending_settlements().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refund_effect_with_lost_ack_survives_database_restart_without_double_movement() {
        let path =
            std::env::temp_dir().join(format!("agent-api-worker-{}.sqlite", uuid::Uuid::new_v4()));
        let store = disk_store(&path);
        let effects = Arc::new(Mutex::new(RefundEffects::default()));
        let first = LostRefundAckExecutor {
            effects: Arc::clone(&effects),
            lose_ack_once: AtomicBool::new(true),
        };
        let first_routes = vec![SettlementRoute::new(filter(), first)];
        assert!(process_pending_settlements(&store, &first_routes)
            .await
            .is_err());
        assert_eq!(
            store.pending_settlements().unwrap()[0].state,
            "refund_submitting"
        );
        {
            let effects = effects.lock().unwrap();
            assert_eq!(effects.settle_calls, 1);
            assert_eq!(effects.attempts, 1);
            assert_eq!(effects.moved_coin_units, 80);
        }
        drop(store);

        let reopened = Store::open(&path).unwrap();
        let retry = LostRefundAckExecutor {
            effects: Arc::clone(&effects),
            lose_ack_once: AtomicBool::new(false),
        };
        assert_eq!(
            process_pending_settlements(&reopened, &[SettlementRoute::new(filter(), retry)])
                .await
                .unwrap(),
            1
        );
        let effects = effects.lock().unwrap();
        assert_eq!(effects.settle_calls, 1);
        assert_eq!(effects.attempts, 2);
        assert_eq!(effects.refund_ids.len(), 1);
        assert_eq!(effects.moved_coin_units, 80);
        assert_eq!(
            reopened.pending_settlements().unwrap()[0].state,
            "refund_submitted"
        );
        assert_eq!(reopened.binding("0x4").unwrap().unwrap().wallet_id, "0x3");
        drop(effects);
        drop(reopened);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }
}
