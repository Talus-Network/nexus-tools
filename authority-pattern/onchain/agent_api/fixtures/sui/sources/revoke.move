module agent_api_sui::revoke;

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
    Revoked {},
}

public fun fqn(): AsciiString {
    b"xyz.taluslabs.agent_api.sui.revoke@1".to_ascii_string()
}

public fun tool_witness_id(cashier: &Cashier<0x2::sui::SUI>): ID {
    object::uid_to_inner(accounting::tool_witness(cashier, b"revoke"))
}

public fun execute(
    authorization: ProvenValue<AgentVertexAuthorization>,
    requirements: UIDRequirements,
    result: OnchainToolResult,
    wallet: &mut AgentWallet<0x2::sui::SUI>,
    cashier: &mut Cashier<0x2::sui::SUI>,
    binding_id: ID,
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
    accounting::revoke(wallet, cashier, binding_id);
    let mut requirements = requirements;
    accounting::satisfy_witness(&mut requirements, cashier, b"revoke");
    result_api::finalize_and_share(
        result,
        requirements,
        tagged_output::new(b"revoked"),
        ctx,
    );
}
