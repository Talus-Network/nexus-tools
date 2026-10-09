module agent_api_sui::register;

use nexus_interface::authorization::{Self as interface_authorization, AgentVertexAuthorization};
use nexus_interface::onchain_tool_result::{Self as result_api, OnchainToolResult};
use nexus_primitives::authorization::ProvenValue;
use nexus_primitives::data;
use nexus_primitives::proof_of_uid::UIDRequirements;
use nexus_primitives::tagged_output;
use std::ascii::String as AsciiString;
use sui::object::{Self, ID};
use sui::tx_context::TxContext;
use agent_api_sui::accounting::{Self, AgentWallet, Cashier};

const E_INVALID_OWNER_PUBLIC_KEY: u64 = 1;

public enum Output {
    Registered { binding_id: AsciiString },
}

public fun fqn(): AsciiString {
    b"xyz.taluslabs.agent_api.sui.register@2".to_ascii_string()
}

fun hex_nibble(byte: u8): u8 {
    if (byte >= 48 && byte <= 57) {
        byte - 48
    } else if (byte >= 65 && byte <= 70) {
        byte - 65 + 10
    } else if (byte >= 97 && byte <= 102) {
        byte - 97 + 10
    } else {
        abort E_INVALID_OWNER_PUBLIC_KEY
    }
}

fun decode_owner_public_key(
    export_enabled: bool,
    owner_public_key_hex: AsciiString,
): vector<u8> {
    let encoded = owner_public_key_hex.into_bytes();
    if (export_enabled) {
        assert!(encoded.length() == 64, E_INVALID_OWNER_PUBLIC_KEY);
        let mut decoded = vector[];
        let mut index = 0;
        while (index < 64) {
            let high = hex_nibble(encoded[index]);
            let low = hex_nibble(encoded[index + 1]);
            decoded.push_back(high * 16 + low);
            index = index + 2;
        };
        decoded
    } else {
        assert!(encoded.is_empty(), E_INVALID_OWNER_PUBLIC_KEY);
        encoded
    }
}

public fun tool_witness_id(cashier: &Cashier<0x2::sui::SUI>): ID {
    object::uid_to_inner(accounting::tool_witness(cashier, b"register"))
}

public(package) fun registered_output(binding_id: ID): tagged_output::TaggedOutput {
    let mut binding_id_json = b"\"0x".to_ascii_string();
    binding_id_json.append(object::id_to_address(&binding_id).to_ascii_string());
    binding_id_json.append(b"\"".to_ascii_string());
    tagged_output::new(b"registered").with_named_payload(
        b"binding_id",
        data::inline_data_value(binding_id_json.into_bytes()),
    )
}

public fun execute(
    authorization: ProvenValue<AgentVertexAuthorization>,
    requirements: UIDRequirements,
    result: OnchainToolResult,
    wallet: &mut AgentWallet<0x2::sui::SUI>,
    cashier: &mut Cashier<0x2::sui::SUI>,
    export_enabled: bool,
    owner_public_key_hex: AsciiString,
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
    let owner_public_key = decode_owner_public_key(export_enabled, owner_public_key_hex);
    let binding_id = accounting::register(
        wallet,
        cashier,
        export_enabled,
        owner_public_key,
        ctx,
    );
    let mut requirements = requirements;
    accounting::satisfy_witness(&mut requirements, cashier, b"register");
    result_api::finalize_and_share(
        result,
        requirements,
        registered_output(binding_id),
        ctx,
    );
}

#[test]
fun disabled_registration_decodes_empty_scalar() {
    assert!(
        decode_owner_public_key(false, b"".to_ascii_string()).is_empty(),
        2,
    );
}

#[test]
fun enabled_registration_decodes_exact_public_key() {
    let decoded = decode_owner_public_key(
        true,
        b"000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F".to_ascii_string(),
    );
    assert!(
        decoded == vector[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
        ],
        3,
    );
}

#[test]
fun enabled_registration_accepts_lowercase_hex() {
    let decoded = decode_owner_public_key(
        true,
        b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_ascii_string(),
    );
    assert!(decoded.length() == 32, 4);
}

#[test, expected_failure(abort_code = E_INVALID_OWNER_PUBLIC_KEY)]
fun disabled_registration_rejects_nonempty_scalar() {
    let _ = decode_owner_public_key(false, b"00".to_ascii_string());
}

#[test, expected_failure(abort_code = E_INVALID_OWNER_PUBLIC_KEY)]
fun enabled_registration_rejects_empty_scalar() {
    let _ = decode_owner_public_key(true, b"".to_ascii_string());
}

#[test, expected_failure(abort_code = E_INVALID_OWNER_PUBLIC_KEY)]
fun enabled_registration_rejects_wrong_length() {
    let _ = decode_owner_public_key(true, b"00".to_ascii_string());
}

#[test, expected_failure(abort_code = E_INVALID_OWNER_PUBLIC_KEY)]
fun enabled_registration_rejects_non_hex_scalar() {
    let mut invalid = b"".to_ascii_string();
    let mut index = 0;
    while (index < 63) {
        invalid.append(b"0".to_ascii_string());
        index = index + 1;
    };
    invalid.append(b"g".to_ascii_string());
    let _ = decode_owner_public_key(true, invalid);
}
