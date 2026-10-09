use {
    crate::{backend::Backend, crypto::KeyEnvelope, tools::canonical_input_hash},
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::{AnyResult, AuthContext, NexusTool, StatusCode},
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    std::time::Duration,
    tokio::sync::Mutex,
};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetrieveKeyInput {
    pub binding_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetrieveKeyOutput {
    Ok { envelope: KeyEnvelope },
    Err { reason: String },
}

pub struct RetrieveKeyTool {
    backend: Option<Backend>,
    context: Mutex<Option<AuthContext>>,
}

impl NexusTool for RetrieveKeyTool {
    type Input = RetrieveKeyInput;
    type Output = RetrieveKeyOutput;

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.agent_api.retrieve-key@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/agent-api/retrieve-key"
    }

    fn description() -> &'static str {
        "Returns an authenticated ciphertext envelope for the provider key and registration-approved owner public key."
    }

    fn timeout() -> Duration {
        Duration::from_secs(20)
    }

    async fn new() -> Self {
        Self {
            backend: Backend::from_env().ok(),
            context: Mutex::new(None),
        }
    }

    async fn authorize(&self, context: AuthContext) -> AnyResult<()> {
        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Agent API service is not configured"))?;
        backend
            .wait_for_invocation_grant(&context, "retrieve-key")
            .await?;
        *self.context.lock().await = Some(context);
        Ok(())
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        let Some(context) = self.context.lock().await.take() else {
            return RetrieveKeyOutput::Err {
                reason: "signed workflow authorization is required".to_owned(),
            };
        };
        let hash = match canonical_input_hash::<RetrieveKeyInput, RetrieveKeyOutput>(&input) {
            Ok(hash) => hash,
            Err(_) => {
                return RetrieveKeyOutput::Err {
                    reason: "request authorization could not be verified".to_owned(),
                }
            }
        };
        let Some(backend) = self.backend.as_ref() else {
            return RetrieveKeyOutput::Err {
                reason: "Agent API service is not configured".to_owned(),
            };
        };
        match backend
            .retrieve_key(&context, hash, &input.binding_id)
            .await
        {
            Ok(envelope) => RetrieveKeyOutput::Ok { envelope },
            Err(_) => RetrieveKeyOutput::Err {
                reason: "key export is disabled or the request is unauthorized".to_owned(),
            },
        }
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        let Some(backend) = self.backend.as_ref() else {
            return Ok(StatusCode::SERVICE_UNAVAILABLE);
        };
        Ok(if backend.health().await {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            backend::DeploymentConfig,
            crypto::MasterKey,
            provider::HttpProvider,
            storage::{BindingRecord, GrantRecord, Store},
        },
        nexus_toolkit::NexusTool,
        std::sync::Arc,
    };

    fn retrieve_key_test_store() -> (Backend, Store) {
        let store = Store::open_in_memory().unwrap();
        let deployment = DeploymentConfig {
            rpc_url: "http://127.0.0.1:9000".to_owned(),
            package_id: "0xabc".to_owned(),
            module: "accounting".to_owned(),
            coin_type: "0x2::sui::SUI".to_owned(),
            cashier_id: "0x2".to_owned(),
            settlement_cap_id: "0xc1".to_owned(),
        };
        store
            .insert_registration(&BindingRecord {
                binding_id: "0x4".to_owned(),
                cashier_id: "0x2".to_owned(),
                wallet_id: "0x3".to_owned(),
                owner: "0x5".to_owned(),
                expected_agent_uid: "0x6".to_owned(),
                origin: Some(deployment.origin().unwrap()),
                rate: 2,
                export_enabled: true,
                owner_public_key: vec![0x21; 32],
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
            .set_provider_key_box("0x4", b"retrieve-wait-sealed-key")
            .unwrap();
        store
            .record_charge("retrieve-wait-charge", "0x4", 100, 200)
            .unwrap();
        let provider =
            HttpProvider::new("http://127.0.0.1:1".to_owned(), "test".to_owned()).unwrap();
        let backend = Backend::new(
            store.clone(),
            Arc::new(provider),
            MasterKey::from_hex(&"a8".repeat(32)).unwrap(),
            vec![deployment],
        )
        .unwrap();
        (backend, store)
    }

    #[tokio::test]
    async fn authorize_waits_for_delayed_retrieve_key_grant_ingestion() {
        let (backend, store) = retrieve_key_test_store();
        let tool = RetrieveKeyTool {
            backend: Some(backend),
            context: Mutex::new(None),
        };
        let nonce = [0x51; 32];
        let input_hash = [0x61; 32];
        let grant = GrantRecord {
            nonce,
            binding_id: "0x4".to_owned(),
            operation: "retrieve-key".to_owned(),
            input_hash,
            event_id: "retrieve-wait-authorization".to_owned(),
            consumed: false,
        };
        let grant_store = store.clone();
        let insertion = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            grant_store.insert_grant(&grant).unwrap()
        });

        let context = AuthContext {
            leader_id: "retrieve-wait-leader".to_owned(),
            leader_key_id: 0,
            input_hash,
            leader_signature: [0; 64],
            nonce,
        };
        assert!(<RetrieveKeyTool as NexusTool>::authorize(&tool, context)
            .await
            .is_ok());
        assert!(insertion.await.unwrap());
        assert_eq!(
            tool.context
                .lock()
                .await
                .as_ref()
                .map(|context| context.nonce),
            Some(nonce)
        );
    }

    #[test]
    fn export_schema_does_not_accept_a_replacement_recipient_key() {
        let input = serde_json::to_string(&schemars::schema_for!(RetrieveKeyInput)).unwrap();
        let output = serde_json::to_string(&schemars::schema_for!(RetrieveKeyOutput)).unwrap();
        assert!(input.contains("binding_id"));
        assert!(!input.contains("public_key"));
        assert!(output.contains("ciphertext"));
        assert!(!output.contains("provider-secret"));
    }
}
