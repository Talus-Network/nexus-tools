use {
    crate::accounting::{refundable_coin_units, settlement_delta, AccountingError},
    nexus_sdk::sui::types::{
        Address,
        Argument,
        Command,
        Digest,
        Identifier,
        Input,
        MoveCall,
        Mutability,
        ObjectReference,
        ProgrammableTransaction,
        SharedInput,
        TypeTag,
    },
    std::str::FromStr,
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum SettlementError {
    #[error("settlement configuration is malformed")]
    InvalidConfiguration,
    #[error("Sui object metadata could not be verified")]
    ObjectMetadata,
    #[error("settlement signer does not own the configured settlement cap")]
    SignerMismatch,
    #[error("Sui settlement transaction failed")]
    Submission,
    #[error("settlement accounting state is inconsistent")]
    Accounting(#[from] AccountingError),
    #[error("settlement transaction serialization failed")]
    Serialization(#[from] bcs::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedObjectRef {
    pub object_id: String,
    pub version: u64,
    pub digest: String,
    pub cashier_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedObjectRef {
    pub object_id: String,
    pub initial_shared_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementPlan {
    pub package_id: String,
    pub coin_type: String,
    pub settlement_cap: OwnedObjectRef,
    pub cashier: SharedObjectRef,
    pub wallet: SharedObjectRef,
    pub binding_id: String,
    pub charged_coin_units: u64,
    pub credit_units: u64,
    pub rate: u64,
    pub previous_usage: u64,
    pub previous_coin_liability: u64,
    pub final_usage: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltSettlement {
    pub settle: ProgrammableTransaction,
    pub refund: ProgrammableTransaction,
    pub earned_coin_units: u64,
    pub refundable_coin_units: u64,
}

/// Builds concrete SDK PTB commands for the configured coin package.
/// The caller submits `settle` first and waits for finality before submitting `refund`.
pub fn build_settlement(plan: &SettlementPlan) -> Result<BuiltSettlement, SettlementError> {
    if plan.package_id.is_empty()
        || plan.coin_type.is_empty()
        || plan.binding_id.is_empty()
        || plan.settlement_cap.cashier_id != plan.cashier.object_id
    {
        return Err(SettlementError::InvalidConfiguration);
    }
    let package =
        Address::from_str(&plan.package_id).map_err(|_| SettlementError::InvalidConfiguration)?;
    let coin_type =
        TypeTag::from_str(&plan.coin_type).map_err(|_| SettlementError::InvalidConfiguration)?;
    let cap = owned_input(&plan.settlement_cap)?;
    let cashier = shared_input(&plan.cashier)?;
    let wallet = shared_input(&plan.wallet)?;
    let binding =
        Address::from_str(&plan.binding_id).map_err(|_| SettlementError::InvalidConfiguration)?;
    let delta = settlement_delta(
        plan.previous_usage,
        plan.previous_coin_liability,
        plan.final_usage,
        plan.credit_units,
        plan.rate,
    )?;
    let refund_coin_units =
        refundable_coin_units(plan.charged_coin_units, plan.final_usage, plan.rate)?;

    let settle = ProgrammableTransaction {
        inputs: vec![
            cap.clone(),
            cashier.clone(),
            Input::Pure(bcs::to_bytes(&binding)?),
            Input::Pure(bcs::to_bytes(&plan.final_usage)?),
        ],
        commands: vec![Command::MoveCall(MoveCall {
            package,
            module: Identifier::from_static("accounting"),
            function: Identifier::from_static("settle"),
            type_arguments: vec![coin_type.clone()],
            arguments: vec![
                Argument::Input(0),
                Argument::Input(1),
                Argument::Input(2),
                Argument::Input(3),
            ],
        })],
    };
    let refund = ProgrammableTransaction {
        inputs: vec![cap, cashier, wallet, Input::Pure(bcs::to_bytes(&binding)?)],
        commands: vec![Command::MoveCall(MoveCall {
            package,
            module: Identifier::from_static("accounting"),
            function: Identifier::from_static("refund"),
            type_arguments: vec![coin_type],
            arguments: vec![
                Argument::Input(0),
                Argument::Input(1),
                Argument::Input(2),
                Argument::Input(3),
            ],
        })],
    };
    Ok(BuiltSettlement {
        settle,
        refund,
        earned_coin_units: delta.newly_earned_coin_units,
        refundable_coin_units: refund_coin_units,
    })
}

fn owned_input(object: &OwnedObjectRef) -> Result<Input, SettlementError> {
    let id =
        Address::from_str(&object.object_id).map_err(|_| SettlementError::InvalidConfiguration)?;
    let digest =
        Digest::from_str(&object.digest).map_err(|_| SettlementError::InvalidConfiguration)?;
    Ok(Input::ImmutableOrOwned(ObjectReference::new(
        id,
        object.version,
        digest,
    )))
}

fn shared_input(object: &SharedObjectRef) -> Result<Input, SettlementError> {
    let id =
        Address::from_str(&object.object_id).map_err(|_| SettlementError::InvalidConfiguration)?;
    Ok(Input::Shared(SharedInput::new(
        id,
        object.initial_shared_version,
        Mutability::Mutable,
    )))
}

#[cfg(test)]
mod tests {
    use {super::*, nexus_sdk::sui::types::Command};

    fn plan(coin_type: &str) -> SettlementPlan {
        SettlementPlan {
            package_id: "0xabc".to_owned(),
            coin_type: coin_type.to_owned(),
            settlement_cap: OwnedObjectRef {
                object_id: "0xc1".to_owned(),
                version: 7,
                digest: "11111111111111111111111111111111".to_owned(),
                cashier_id: "0xc2".to_owned(),
            },
            cashier: SharedObjectRef {
                object_id: "0xc2".to_owned(),
                initial_shared_version: 4,
            },
            wallet: SharedObjectRef {
                object_id: "0xc3".to_owned(),
                initial_shared_version: 5,
            },
            binding_id: "0xc4".to_owned(),
            charged_coin_units: 100,
            credit_units: 200,
            rate: 2,
            previous_usage: 0,
            previous_coin_liability: 0,
            final_usage: 40,
        }
    }

    #[test]
    fn settlement_and_refund_calls_use_the_configured_coin_type_and_ordered_objects() {
        let built = build_settlement(&plan("0x2::sui::SUI")).unwrap();
        assert_eq!(built.earned_coin_units, 20);
        assert_eq!(built.refundable_coin_units, 80);
        let Command::MoveCall(settle) = &built.settle.commands[0] else {
            panic!("settle must be a Move call")
        };
        let Command::MoveCall(refund) = &built.refund.commands[0] else {
            panic!("refund must be a Move call")
        };
        assert_eq!(settle.module.as_str(), "accounting");
        assert_eq!(settle.function.as_str(), "settle");
        assert_eq!(
            settle.type_arguments[0],
            TypeTag::from_str("0x2::sui::SUI").unwrap()
        );
        assert_eq!(refund.function.as_str(), "refund");
        assert_eq!(refund.type_arguments[0], settle.type_arguments[0]);
        assert_eq!(built.settle.inputs.len(), 4);
        assert_eq!(built.refund.inputs.len(), 4);
    }

    #[test]
    fn settlement_builder_accepts_a_non_sui_coin_and_rejects_cap_cashier_mismatch() {
        let test_coin = build_settlement(&plan("0x123::test_coin::TEST_COIN")).unwrap();
        let Command::MoveCall(call) = &test_coin.settle.commands[0] else {
            panic!("settle must be a Move call")
        };
        assert_eq!(
            call.type_arguments[0],
            TypeTag::from_str("0x123::test_coin::TEST_COIN").unwrap()
        );
        let mut bad = plan("0x2::sui::SUI");
        bad.settlement_cap.cashier_id = "0x99".to_owned();
        assert!(build_settlement(&bad).is_err());
    }
}
