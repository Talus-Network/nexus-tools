module agent_api_test_coin::authorize;

use nexus_interface::authorization::{Self as interface_authorization, AgentVertexAuthorization};
use nexus_interface::onchain_tool_result::{Self as result_api, OnchainToolResult};
use nexus_interface::verifier;
use nexus_primitives::authorization::ProvenValue;
use nexus_primitives::data;
use nexus_primitives::proof_of_uid::UIDRequirements;
use nexus_primitives::tagged_output;
use std::ascii::String as AsciiString;
use sui::object::{Self, ID};
use sui::tx_context::TxContext;
use agent_api_test_coin::accounting::{Self, AgentWallet, Cashier};

public enum Output {
    Authorized { binding_id: AsciiString },
}

public fun fqn(): AsciiString {
    b"xyz.taluslabs.agent_api.test-coin.authorize@1".to_ascii_string()
}

public fun tool_witness_id(cashier: &Cashier<agent_api_test_coin::test_coin::TEST_COIN>): ID {
    object::uid_to_inner(accounting::tool_witness(cashier, b"authorize"))
}

public(package) fun authorized_output(binding_id: ID): tagged_output::TaggedOutput {
    let mut binding_id_json = b"\"0x".to_ascii_string();
    binding_id_json.append(object::id_to_address(&binding_id).to_ascii_string());
    binding_id_json.append(b"\"".to_ascii_string());
    tagged_output::new(b"authorized").with_named_payload(
        b"binding_id",
        data::inline_data_value(binding_id_json.into_bytes()),
    )
}

public fun execute(
    authorization: ProvenValue<AgentVertexAuthorization>,
    requirements: UIDRequirements,
    result: OnchainToolResult,
    wallet: &mut AgentWallet<agent_api_test_coin::test_coin::TEST_COIN>,
    cashier: &mut Cashier<agent_api_test_coin::test_coin::TEST_COIN>,
    binding_id: ID,
    target_vertex: vector<u8>,
    walk_index: u64,
    iteration: u64,
    operation: vector<u8>,
    input_hash: vector<u8>,
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
    let execution_id = result_api::execution_id(&result);
    let target_nonce = verifier::tool_invocation_nonce(
        execution_id,
        walk_index,
        target_vertex,
        iteration,
    );
    accounting::authorize(
        wallet,
        cashier,
        binding_id,
        execution_id,
        walk_index,
        target_vertex,
        iteration,
        target_nonce,
        operation,
        input_hash,
    );
    let mut requirements = requirements;
    accounting::satisfy_witness(&mut requirements, cashier, b"authorize");
    result_api::finalize_and_share(
        result,
        requirements,
        authorized_output(binding_id),
        ctx,
    );
}
