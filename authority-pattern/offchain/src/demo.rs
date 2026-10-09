use {
    agent_api::{
        accounting::{coin_units_for_usage, refundable_coin_units},
        backend::{Backend, DeploymentConfig},
        crypto::{decrypt_export, MasterKey},
        events::{decode_event, ChainEvent, EventFilter},
        provider::{mock_routes, HttpProvider, Provider, ProviderError},
        settlement::{
            build_settlement,
            OwnedObjectRef,
            SettlementError,
            SettlementPlan,
            SharedObjectRef,
        },
        storage::{BindingRecord, Store},
        tools::{
            canonical_input_hash,
            query::{QueryInput, QueryOutput},
            retrieve_key::{RetrieveKeyInput, RetrieveKeyOutput},
        },
        worker::{process_pending_settlements, SettlementExecutor, SettlementRoute},
    },
    anyhow::{bail, ensure, Context, Result},
    async_trait::async_trait,
    nexus_toolkit::AuthContext,
    serde_json::json,
    std::{
        path::Path,
        sync::{Arc, Mutex},
    },
    x25519_dalek::{PublicKey, StaticSecret},
};

const INITIAL_WALLET_COIN_UNITS: u64 = 1_000;
const DEMO_WALLET_ID: &str = "0xc1";
const DEMO_CASHIER_ID: &str = "0xc2";
const DEMO_BINDING_ID: &str = "0xc4";
const DEMO_COIN_UNITS: u64 = 100;
const DEMO_RATE: u64 = 2;
const DEMO_CREDIT_UNITS: u64 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DemoOutcome {
    initial_wallet_coin_units: u64,
    wallet_after_charge_coin_units: u64,
    reserve_after_charge_coin_units: u64,
    provider_credited_units: u64,
    provider_usage_units: u64,
    provider_remaining_units_before_revoke: u64,
    provider_remaining_units: u64,
    earned_revenue_coin_units: u64,
    refund_coin_units: u64,
    wallet_after_refund_coin_units: u64,
    reserve_after_refund_coin_units: u64,
    provider_key_disabled: bool,
    binding_status: String,
    binding_refunded: bool,
    refund_event_confirmed: bool,
    pending_settlements: usize,
}

impl DemoOutcome {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.initial_wallet_coin_units == 1_000
                && self.wallet_after_charge_coin_units == 900
                && self.reserve_after_charge_coin_units == 100,
            "simulated charge did not debit 100 coin units into the refund reserve"
        );
        ensure!(
            self.provider_credited_units == 200
                && self.provider_usage_units == 40
                && self.provider_remaining_units_before_revoke == 160
                && self.provider_remaining_units == 0,
            "provider credit did not reconcile to 160 before revoke and zero after disable"
        );
        ensure!(
            self.earned_revenue_coin_units == 20
                && self.refund_coin_units == 80
                && self.wallet_after_refund_coin_units == 980
                && self.reserve_after_refund_coin_units == 0,
            "simulated revenue and refund did not reconcile to 20 + 80 coin units"
        );
        ensure!(
            self.provider_key_disabled
                && self.binding_status == "refunded"
                && self.binding_refunded
                && self.refund_event_confirmed
                && self.pending_settlements == 0,
            "revocation and refund obligation did not reach the completed state"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct LedgerSnapshot {
    wallet_coin_units: u64,
    reserve_coin_units: u64,
    earned_revenue_coin_units: u64,
    credited_provider_units: u64,
    provider_usage_units: u64,
    provider_key_disabled: bool,
    refund_coin_units: u64,
    refund_event_confirmed: bool,
}

#[derive(Debug)]
struct LocalLedger {
    wallet_id: String,
    wallet_coin_units: u64,
    reserve_coin_units: u64,
    earned_revenue_coin_units: u64,
    credited_provider_units: u64,
    provider_usage_units: u64,
    provider_key_disabled: bool,
    settled_binding_id: Option<String>,
    recognized_coin_units: u64,
    refunded_binding_id: Option<String>,
    refund_coin_units: u64,
    refund_event_confirmed: bool,
}

impl LocalLedger {
    fn new(wallet_id: &str, wallet_coin_units: u64) -> Self {
        Self {
            wallet_id: wallet_id.to_owned(),
            wallet_coin_units,
            reserve_coin_units: 0,
            earned_revenue_coin_units: 0,
            credited_provider_units: 0,
            provider_usage_units: 0,
            provider_key_disabled: false,
            settled_binding_id: None,
            recognized_coin_units: 0,
            refunded_binding_id: None,
            refund_coin_units: 0,
            refund_event_confirmed: false,
        }
    }

    fn apply_charge(
        &mut self,
        wallet_id: &str,
        coin_units: u64,
        credit_units: u64,
        rate: u64,
    ) -> Result<()> {
        ensure!(
            wallet_id == self.wallet_id,
            "charge used an unexpected wallet"
        );
        ensure!(
            coin_units.checked_mul(rate) == Some(credit_units),
            "charge credit does not match the configured rate"
        );
        self.wallet_coin_units = self
            .wallet_coin_units
            .checked_sub(coin_units)
            .context("simulated wallet cannot cover the charge")?;
        self.reserve_coin_units = self
            .reserve_coin_units
            .checked_add(coin_units)
            .context("simulated refund reserve overflowed")?;
        self.credited_provider_units = self
            .credited_provider_units
            .checked_add(credit_units)
            .context("simulated provider credit overflowed")?;
        self.ensure_conserved()?;
        Ok(())
    }

    fn observe_provider_usage(&mut self, usage: u64) -> Result<()> {
        ensure!(
            usage >= self.provider_usage_units && usage <= self.credited_provider_units,
            "mock provider usage regressed or exceeded its credit"
        );
        self.provider_usage_units = usage;
        Ok(())
    }

    fn mark_provider_key_disabled(&mut self) {
        self.provider_key_disabled = true;
    }

    fn apply_settlement(
        &mut self,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> std::result::Result<(), SettlementError> {
        if binding.status != "revoked"
            || binding.wallet_id != self.wallet_id
            || binding.credit_units != self.credited_provider_units
            || final_usage != binding.latest_provider_usage
            || final_usage != self.provider_usage_units
        {
            return Err(SettlementError::InvalidConfiguration);
        }
        let recognized = coin_units_for_usage(final_usage, binding.rate)?;
        if let Some(settled_binding_id) = &self.settled_binding_id {
            return if settled_binding_id == &binding.binding_id
                && self.recognized_coin_units == recognized
            {
                Ok(())
            } else {
                Err(SettlementError::InvalidConfiguration)
            };
        }
        self.reserve_coin_units = self
            .reserve_coin_units
            .checked_sub(recognized)
            .ok_or(SettlementError::InvalidConfiguration)?;
        self.earned_revenue_coin_units = self
            .earned_revenue_coin_units
            .checked_add(recognized)
            .ok_or(SettlementError::InvalidConfiguration)?;
        self.recognized_coin_units = recognized;
        self.settled_binding_id = Some(binding.binding_id.clone());
        self.ensure_conserved()
            .map_err(|_| SettlementError::InvalidConfiguration)
    }

    fn apply_refund(
        &mut self,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> std::result::Result<(), SettlementError> {
        if binding.status != "revoked"
            || binding.wallet_id != self.wallet_id
            || binding.latest_provider_usage != final_usage
            || final_usage != self.provider_usage_units
        {
            return Err(SettlementError::InvalidConfiguration);
        }
        let refund = refundable_coin_units(binding.charged_coin_units, final_usage, binding.rate)?;
        if binding.recognized_coin_units != self.recognized_coin_units
            || self.settled_binding_id.as_deref() != Some(binding.binding_id.as_str())
        {
            return Err(SettlementError::InvalidConfiguration);
        }
        if let Some(refunded_binding_id) = &self.refunded_binding_id {
            return if refunded_binding_id == &binding.binding_id && self.refund_coin_units == refund
            {
                Ok(())
            } else {
                Err(SettlementError::InvalidConfiguration)
            };
        }
        self.reserve_coin_units = self
            .reserve_coin_units
            .checked_sub(refund)
            .ok_or(SettlementError::InvalidConfiguration)?;
        self.wallet_coin_units = self
            .wallet_coin_units
            .checked_add(refund)
            .ok_or(SettlementError::InvalidConfiguration)?;
        self.refund_coin_units = refund;
        self.refunded_binding_id = Some(binding.binding_id.clone());
        self.ensure_conserved()
            .map_err(|_| SettlementError::InvalidConfiguration)
    }

    fn confirm_refund_event(&mut self, binding_id: &str, coin_units: u64) -> Result<()> {
        ensure!(
            self.refunded_binding_id.as_deref() == Some(binding_id)
                && self.refund_coin_units == coin_units,
            "simulated refund event does not match the submitted refund"
        );
        self.refund_event_confirmed = true;
        Ok(())
    }

    fn ensure_conserved(&self) -> Result<()> {
        let total = self
            .wallet_coin_units
            .checked_add(self.reserve_coin_units)
            .and_then(|value| value.checked_add(self.earned_revenue_coin_units))
            .context("simulated accounting total overflowed")?;
        ensure!(
            total == INITIAL_WALLET_COIN_UNITS,
            "simulated wallet, reserve, and earned revenue do not conserve deposited funds"
        );
        Ok(())
    }

    fn snapshot(&self) -> LedgerSnapshot {
        LedgerSnapshot {
            wallet_coin_units: self.wallet_coin_units,
            reserve_coin_units: self.reserve_coin_units,
            earned_revenue_coin_units: self.earned_revenue_coin_units,
            credited_provider_units: self.credited_provider_units,
            provider_usage_units: self.provider_usage_units,
            provider_key_disabled: self.provider_key_disabled,
            refund_coin_units: self.refund_coin_units,
            refund_event_confirmed: self.refund_event_confirmed,
        }
    }
}

struct DemoSettlementExecutor {
    ledger: Arc<Mutex<LocalLedger>>,
}

#[async_trait]
impl SettlementExecutor for DemoSettlementExecutor {
    async fn settle(
        &self,
        _filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> std::result::Result<(), SettlementError> {
        self.ledger
            .lock()
            .map_err(|_| SettlementError::InvalidConfiguration)?
            .apply_settlement(binding, final_usage)
    }

    async fn refund(
        &self,
        _filter: &EventFilter,
        binding: &BindingRecord,
        final_usage: u64,
    ) -> std::result::Result<(), SettlementError> {
        self.ledger
            .lock()
            .map_err(|_| SettlementError::InvalidConfiguration)?
            .apply_refund(binding, final_usage)
    }
}

fn demo_event(
    filter: &EventFilter,
    event_id: &str,
    event_name: &str,
    fields: serde_json::Value,
) -> Result<agent_api::events::DecodedEvent> {
    let event = ChainEvent {
        event_id: event_id.to_owned(),
        type_tag: format!(
            "{}::{}::{}<{}>",
            filter.package_id, filter.module, event_name, filter.coin_type
        ),
        fields,
    };
    decode_event(filter, event).context("simulated chain event did not pass the real event decoder")
}

pub async fn run() -> Result<()> {
    let outcome = run_demo_flow().await?;
    outcome.validate()?;
    println!(
        "simulated accounting reconciled: wallet {} -> {} -> {}, reserve {} -> {}, provider credit {}, usage {}, spendable before revoke {}, after revoke {}, earned {}, refunded {}",
        outcome.initial_wallet_coin_units,
        outcome.wallet_after_charge_coin_units,
        outcome.wallet_after_refund_coin_units,
        outcome.reserve_after_charge_coin_units,
        outcome.reserve_after_refund_coin_units,
        outcome.provider_credited_units,
        outcome.provider_usage_units,
        outcome.provider_remaining_units_before_revoke,
        outcome.provider_remaining_units,
        outcome.earned_revenue_coin_units,
        outcome.refund_coin_units,
    );
    println!("simulated refund event completed the backend obligation; no network transaction was submitted");
    Ok(())
}

async fn run_demo_flow() -> Result<DemoOutcome> {
    let unique = uuid::Uuid::new_v4();
    let provider_path =
        std::env::temp_dir().join(format!("agent-api-demo-provider-{unique}.sqlite"));
    let store_path = std::env::temp_dir().join(format!("agent-api-demo-store-{unique}.sqlite"));
    let operator_key = "synthetic-demo-operator-secret".to_owned();
    let routes = mock_routes(provider_path.clone(), operator_key.clone())?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(warp::serve(routes).incoming(listener).run());

    let result = run_demo_with_provider(address, operator_key, store_path.clone()).await;
    server.abort();
    for path in [&provider_path, &store_path] {
        remove_demo_database(path);
    }
    result
}

fn remove_demo_database(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
}

async fn run_demo_with_provider(
    address: std::net::SocketAddr,
    operator_key: String,
    store_path: impl AsRef<Path>,
) -> Result<DemoOutcome> {
    let filter = EventFilter {
        rpc_url: "http://127.0.0.1:9000".to_owned(),
        package_id: "0xabc".to_owned(),
        module: "accounting".to_owned(),
        coin_type: "0x2::sui::SUI".to_owned(),
        cashier_id: DEMO_CASHIER_ID.to_owned(),
    };
    let provider = Arc::new(HttpProvider::new(
        format!("http://{address}"),
        operator_key,
    )?);
    provider.verify_capabilities().await?;
    let backend = Backend::new(
        Store::open(store_path)?,
        provider.clone(),
        MasterKey::from_hex(&"4d".repeat(32))?,
        vec![DeploymentConfig {
            rpc_url: filter.rpc_url.clone(),
            package_id: filter.package_id.clone(),
            module: filter.module.clone(),
            coin_type: filter.coin_type.clone(),
            cashier_id: filter.cashier_id.clone(),
            settlement_cap_id: "0xc6".to_owned(),
        }],
    )?;
    let ledger = Arc::new(Mutex::new(LocalLedger::new(
        DEMO_WALLET_ID,
        INITIAL_WALLET_COIN_UNITS,
    )));

    let owner_private = [0x44_u8; 32];
    let owner_public = PublicKey::from(&StaticSecret::from(owner_private));
    backend
        .process_event(demo_event(
            &filter,
            "register-1",
            "RegistrationEvent",
            json!({
                "cashier_id":DEMO_CASHIER_ID, "wallet_id":DEMO_WALLET_ID, "binding_id":DEMO_BINDING_ID, "owner":"0xc5",
                "expected_agent_uid":"0xc6", "rate":DEMO_RATE, "export_enabled":true,
                "owner_public_key":owner_public.as_bytes().to_vec()
            }),
        )?)
        .await?;
    backend
        .process_event(demo_event(
            &filter,
            "charge-1",
            "ChargeEvent",
            json!({
                "cashier_id":DEMO_CASHIER_ID, "wallet_id":DEMO_WALLET_ID, "binding_id":DEMO_BINDING_ID, "coin_units":DEMO_COIN_UNITS,
                "credit_units":DEMO_CREDIT_UNITS, "rate":DEMO_RATE
            }),
        )?)
        .await?;
    ledger
        .lock()
        .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
        .apply_charge(
            DEMO_WALLET_ID,
            DEMO_COIN_UNITS,
            DEMO_CREDIT_UNITS,
            DEMO_RATE,
        )?;
    let after_charge = ledger
        .lock()
        .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
        .snapshot();

    let query = QueryInput {
        binding_id: DEMO_BINDING_ID.to_owned(),
        payload: json!({"prompt":"hello from the local demo"}),
    };
    let query_hash = canonical_input_hash::<QueryInput, QueryOutput>(&query)?;
    let query_nonce = [1_u8; 32];
    backend
        .process_event(demo_event(
            &filter,
            "authorize-query-1",
            "AuthorizationEvent",
            json!({
                "cashier_id":DEMO_CASHIER_ID, "binding_id":DEMO_BINDING_ID, "execution_id":"0xc7", "walk_index":0,
                "target_vertex":b"xyz.taluslabs.agent_api.query@1".to_vec(), "iteration":0,
                "target_nonce":query_nonce.to_vec(), "operation":b"query".to_vec(), "input_hash":query_hash.to_vec()
            }),
        )?)
        .await?;
    let query_context = AuthContext {
        leader_id: "local-demo".to_owned(),
        leader_key_id: 1,
        input_hash: query_hash,
        leader_signature: [0; 64],
        nonce: query_nonce,
    };
    backend.check_grant(&query_context, "query")?;
    let query_result = backend
        .query(
            &query_context,
            query_hash,
            &query.binding_id,
            &query.payload,
        )
        .await?;
    ledger
        .lock()
        .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
        .observe_provider_usage(query_result.cumulative_usage)?;
    println!(
        "provider query through one-use simulated grant: {}",
        query_result.result
    );

    let export_input = RetrieveKeyInput {
        binding_id: DEMO_BINDING_ID.to_owned(),
    };
    let export_hash = canonical_input_hash::<RetrieveKeyInput, RetrieveKeyOutput>(&export_input)?;
    let export_nonce = [2_u8; 32];
    backend
        .process_event(demo_event(
            &filter,
            "authorize-export-1",
            "AuthorizationEvent",
            json!({
                "cashier_id":DEMO_CASHIER_ID, "binding_id":DEMO_BINDING_ID, "execution_id":"0xc8", "walk_index":0,
                "target_vertex":b"xyz.taluslabs.agent_api.retrieve-key@1".to_vec(), "iteration":0,
                "target_nonce":export_nonce.to_vec(), "operation":b"retrieve-key".to_vec(), "input_hash":export_hash.to_vec()
            }),
        )?)
        .await?;
    let export_context = AuthContext {
        leader_id: "local-demo".to_owned(),
        leader_key_id: 1,
        input_hash: export_hash,
        leader_signature: [0; 64],
        nonce: export_nonce,
    };
    backend.check_grant(&export_context, "retrieve-key")?;
    let envelope = backend
        .retrieve_key(&export_context, export_hash, &export_input.binding_id)
        .await?;
    let key = decrypt_export(&owner_private, &envelope)?;
    let key = std::str::from_utf8(&key)?;
    println!("owner decrypted the approved ciphertext envelope locally");

    for index in 0..39 {
        let payload = json!({"owner_direct_request":index});
        let result = provider
            .query(key, &payload, &format!("owner-direct-{index}"))
            .await?;
        ledger
            .lock()
            .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
            .observe_provider_usage(result.cumulative_usage)?;
    }
    println!("owner made 39 direct provider calls with the exported key and no Nexus authorization headers");
    let after_usage = ledger
        .lock()
        .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
        .snapshot();
    ensure!(
        after_usage.provider_usage_units == 40
            && after_usage.credited_provider_units - after_usage.provider_usage_units == 160,
        "mock provider usage did not reach 40 of 200 credited units"
    );

    backend
        .process_event(demo_event(
            &filter,
            "revoke-1",
            "RevokedEvent",
            json!({
                "cashier_id":DEMO_CASHIER_ID, "wallet_id":DEMO_WALLET_ID, "binding_id":DEMO_BINDING_ID
            }),
        )?)
        .await?;
    match provider
        .query(
            key,
            &json!({"after_revoke":true}),
            "owner-direct-after-revoke",
        )
        .await
    {
        Err(ProviderError::Rejected) => ledger
            .lock()
            .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
            .mark_provider_key_disabled(),
        Err(error) => return Err(error.into()),
        Ok(_) => bail!("revoked provider key unexpectedly accepted another query"),
    }
    let provider_usage_after_revoke = provider.final_usage(key).await?;
    let provider_remaining_after_revoke = provider.mock_remaining_credit_units(key).await?;
    ensure!(
        provider_usage_after_revoke == 40 && provider_remaining_after_revoke == 0,
        "mock provider did not preserve usage and retire remaining credit after revoke"
    );

    let binding = backend
        .store
        .binding(DEMO_BINDING_ID)?
        .context("demo binding disappeared")?;
    let plan = SettlementPlan {
        package_id: "0xabc".to_owned(),
        coin_type: "0x2::sui::SUI".to_owned(),
        settlement_cap: OwnedObjectRef {
            object_id: "0xc3".to_owned(),
            version: 1,
            digest: "11111111111111111111111111111111".to_owned(),
            cashier_id: DEMO_CASHIER_ID.to_owned(),
        },
        cashier: SharedObjectRef {
            object_id: DEMO_CASHIER_ID.to_owned(),
            initial_shared_version: 1,
        },
        wallet: SharedObjectRef {
            object_id: DEMO_WALLET_ID.to_owned(),
            initial_shared_version: 1,
        },
        binding_id: DEMO_BINDING_ID.to_owned(),
        charged_coin_units: binding.charged_coin_units,
        credit_units: binding.credit_units,
        rate: binding.rate,
        previous_usage: binding.settled_usage,
        previous_coin_liability: binding.recognized_coin_units,
        final_usage: binding.latest_provider_usage,
    };
    let built = build_settlement(&plan)?;
    println!(
        "simulated settlement for package {}: earned {}, refund {}; SDK PTBs contain {} ordered Move calls",
        plan.package_id,
        built.earned_coin_units,
        built.refundable_coin_units,
        built.settle.commands.len() + built.refund.commands.len(),
    );

    let settlement_executor = DemoSettlementExecutor {
        ledger: Arc::clone(&ledger),
    };
    let settlement_routes = [SettlementRoute::new(filter.clone(), settlement_executor)];
    ensure!(
        process_pending_settlements(&backend.store, &settlement_routes).await? == 1,
        "settlement worker did not complete the pending simulated refund submission"
    );
    let settled_binding = backend
        .store
        .binding(DEMO_BINDING_ID)?
        .context("demo binding disappeared after settlement")?;
    ensure!(
        settled_binding.recognized_coin_units == 20 && settled_binding.latest_provider_usage == 40,
        "settlement worker did not record the final usage and earned revenue"
    );
    let refund_coin_units = ledger
        .lock()
        .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
        .snapshot()
        .refund_coin_units;
    ensure!(
        refund_coin_units == 80,
        "simulated executor produced the wrong refund amount"
    );
    backend
        .process_event(demo_event(
            &filter,
            "refund-1",
            "RefundedEvent",
            json!({
                "cashier_id":DEMO_CASHIER_ID, "wallet_id":DEMO_WALLET_ID,
                "binding_id":DEMO_BINDING_ID, "coin_units":refund_coin_units
            }),
        )?)
        .await?;
    ledger
        .lock()
        .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
        .confirm_refund_event(DEMO_BINDING_ID, refund_coin_units)?;

    let completed_binding = backend
        .store
        .binding(DEMO_BINDING_ID)?
        .context("demo binding disappeared after the refund event")?;
    let pending_settlements = backend.store.pending_settlements()?;
    let final_ledger = ledger
        .lock()
        .map_err(|_| anyhow::anyhow!("demo accounting ledger mutex was poisoned"))?
        .snapshot();
    let outcome = DemoOutcome {
        initial_wallet_coin_units: INITIAL_WALLET_COIN_UNITS,
        wallet_after_charge_coin_units: after_charge.wallet_coin_units,
        reserve_after_charge_coin_units: after_charge.reserve_coin_units,
        provider_credited_units: after_usage.credited_provider_units,
        provider_usage_units: after_usage.provider_usage_units,
        provider_remaining_units_before_revoke: after_usage
            .credited_provider_units
            .checked_sub(after_usage.provider_usage_units)
            .context("provider usage exceeded its credit")?,
        provider_remaining_units: provider_remaining_after_revoke,
        earned_revenue_coin_units: final_ledger.earned_revenue_coin_units,
        refund_coin_units: final_ledger.refund_coin_units,
        wallet_after_refund_coin_units: final_ledger.wallet_coin_units,
        reserve_after_refund_coin_units: final_ledger.reserve_coin_units,
        provider_key_disabled: final_ledger.provider_key_disabled,
        binding_status: completed_binding.status,
        binding_refunded: completed_binding.refunded,
        refund_event_confirmed: final_ledger.refund_event_confirmed,
        pending_settlements: pending_settlements.len(),
    };
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_demo_reconciles_provider_usage_revenue_refund_and_wallet() {
        let outcome = run_demo_flow().await.expect("local demo completes");
        outcome.validate().expect("simulated lifecycle reconciles");
        assert_eq!(outcome.wallet_after_charge_coin_units, 900);
        assert_eq!(outcome.provider_remaining_units_before_revoke, 160);
        assert_eq!(outcome.provider_remaining_units, 0);
        assert_eq!(outcome.earned_revenue_coin_units, 20);
        assert_eq!(outcome.refund_coin_units, 80);
        assert_eq!(outcome.wallet_after_refund_coin_units, 980);
        assert_eq!(outcome.reserve_after_refund_coin_units, 0);
        assert_eq!(outcome.pending_settlements, 0);
    }
}
