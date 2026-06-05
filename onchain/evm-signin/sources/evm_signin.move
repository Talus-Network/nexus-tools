/// # `xyz.taluslabs.evm.verify-signature@1`
///
/// Onchain Nexus Tool that verifies an Ethereum (secp256k1) signature *inside*
/// a Sui Move contract and recovers the signer's 0x address.
module evm_signin::evm_signin;

use evm_signin::evm_access;
use evm_signin::evm_crypto;
use nexus_primitives::data;
use nexus_primitives::proof_of_uid::ProofOfUID;
use nexus_primitives::tagged_output::{Self as tagged_output, TaggedOutput};
use std::ascii::String as AsciiString;
use sui::bag::{Self, Bag};
use sui::transfer::share_object;

/// One-time witness for package initialization.
public struct EVM_SIGNIN has drop {}

/// Witness object used by the Nexus framework to identify this tool.
public struct EvmSigninWitness has key, store {
    id: UID,
}

/// Shared state object. Holds only the tool witness; signature verification is
/// pure, so the tool keeps no per-call state.
public struct EvmSigninState has key {
    id: UID,
    witness: Bag,
}

/// Tool execution output variants. Only used so the SDK can fetch the output
/// schema at registration time; runtime output is built via `TaggedOutput`.
public enum Output {
    Ok {
        recovered_address: AsciiString,
        matches: bool,
    },
    Err {
        reason: AsciiString,
    },
}

/// Package init: creates both the Nexus tool state and the identity-binding
/// registry. Sui only invokes init for the package OTW (`EVM_SIGNIN`), so both
/// shared objects must be created here.
fun init(_otw: EVM_SIGNIN, ctx: &mut TxContext) {
    let state = EvmSigninState {
        id: object::new(ctx),
        witness: {
            let mut bag = bag::new(ctx);
            bag.add(b"witness", EvmSigninWitness { id: object::new(ctx) });
            bag
        },
    };
    share_object(state);
    evm_access::share_registry(ctx);
}

/// Verify an Ethereum signature and recover the signer address.
public fun execute(
    worksheet: &mut ProofOfUID,
    state: &mut EvmSigninState,
    message: vector<u8>,
    signature: vector<u8>,
    expected_address: vector<u8>,
    _ctx: &mut TxContext,
): TaggedOutput {
    let witness = state.witness();
    worksheet.stamp_with_data(&witness.id, b"evm_signin_executed");

    if (signature.length() != 65) {
        return tagged_output::new(b"err").with_named_payload(
            b"reason",
            data::inline_one(b"Signature must be 65 bytes (r, s, v)").as_string(),
        )
    };

    let recovered = evm_crypto::recover_address(message, signature);
    let recovered_hex = evm_crypto::to_hex_string(&recovered);

    let matches = expected_address.is_empty() || expected_address == recovered;

    tagged_output::new(b"ok")
        .with_named_payload(b"recovered_address", data::inline_one(recovered_hex).as_string())
        .with_named_payload(
            b"matches",
            data::inline_one(if (matches) b"true" else b"false").as_bool(),
        )
}

fun witness(self: &EvmSigninState): &EvmSigninWitness {
    self.witness.borrow(b"witness")
}

public fun witness_id(self: &EvmSigninState): ID {
    self.witness().id.to_inner()
}

/// Re-export for callers that imported recovery from this module historically.
public fun recover_address(message: vector<u8>, signature: vector<u8>): vector<u8> {
    evm_crypto::recover_address(message, signature)
}

/// Re-export hex helper.
public fun to_hex_string(bytes: &vector<u8>): vector<u8> {
    evm_crypto::to_hex_string(bytes)
}

#[test_only]
public fun init_for_test(otw: EVM_SIGNIN, ctx: &mut TxContext) {
    init(otw, ctx);
}
