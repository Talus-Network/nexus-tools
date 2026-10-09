module agent_api_sui::charge;

use nexus_interface::authorization::{Self as interface_authorization, AgentVertexAuthorization};
use nexus_interface::onchain_tool_result::{Self as result_api, OnchainToolResult};
use nexus_primitives::authorization::ProvenValue;
use nexus_primitives::proof_of_uid::UIDRequirements;
use nexus_primitives::tagged_output;
use std::ascii::String as AsciiString;
use sui::object::{Self, ID};
use sui::tx_context::TxContext;
use agent_api_sui::accounting::{Self, AgentWallet, Cashier};

public enum Output {
    Charged {},
}

public fun fqn(): AsciiString {
    b"xyz.taluslabs.agent_api.sui.charge@1".to_ascii_string()
}

public fun tool_witness_id(cashier: &Cashier<0x2::sui::SUI>): ID {
    object::uid_to_inner(accounting::tool_witness(cashier, b"charge"))
}

public fun execute(
    authorization: ProvenValue<AgentVertexAuthorization>,
    requirements: UIDRequirements,
    result: OnchainToolResult,
    wallet: &mut AgentWallet<0x2::sui::SUI>,
    cashier: &mut Cashier<0x2::sui::SUI>,
    binding_id: ID,
    coin_units: u64,
    ctx: &mut TxContext,
) {
    accounting::assert_agent(&authorization, wallet);
    let commitment = result_api::input_commitment(&result);
    assert!(
        interface_authorization::consume_verified_for_worksheet_as_recipient(
            authorization,
            requirements.proof(),
            accounting::wallet_uid(wallet),
            commitment,
        ),
        1,
    );
    accounting::charge(wallet, cashier, binding_id, coin_units);
    let mut requirements = requirements;
    accounting::satisfy_witness(&mut requirements, cashier, b"charge");
    result_api::finalize_and_share(
        result,
        requirements,
        tagged_output::new(b"charged"),
        ctx,
    );
}
