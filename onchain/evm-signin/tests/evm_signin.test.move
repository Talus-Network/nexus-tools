#[test_only]
module evm_signin::evm_signin_test;

use evm_signin::evm_signin::{Self, EvmSigninState, EVM_SIGNIN};
use nexus_primitives::proof_of_uid;
use sui::test_scenario;
use sui::test_utils;

const USER: address = @0xA11CE;

// Deterministic vector generated with foundry `cast` using Anvil account #0:
//   key  : 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
//   addr : 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
//   msg  : "Login to Talus Nexus"  (signed with personal_sign / EIP-191)
//
// `MESSAGE` is the EIP-191 preimage: 0x19 || "Ethereum Signed Message:\n20" || msg.
const MESSAGE: vector<u8> =
    x"19457468657265756d205369676e6564204d6573736167653a0a32304c6f67696e20746f2054616c7573204e65787573";
const SIGNATURE: vector<u8> =
    x"a32c85b1f7bd8572112fa50788c209581ec16c4c6279dd8f2233f0c1f6492a26345a0c4147bbfc5c542c57d74407ded4d2a3046f8150491cda66ed4a66bbb6fb1c";
// Same signature with `v` in raw recovery-id form (0x01) instead of Ethereum's
// 0x1c (28). The tool must normalize both to the same recovered address.
const SIGNATURE_RAW_V: vector<u8> =
    x"a32c85b1f7bd8572112fa50788c209581ec16c4c6279dd8f2233f0c1f6492a26345a0c4147bbfc5c542c57d74407ded4d2a3046f8150491cda66ed4a66bbb6fb01";
const SIGNER: vector<u8> = x"f39fd6e51aad88f6f4ce6ab8827279cfffb92266";
const OTHER_ADDR: vector<u8> = x"0000000000000000000000000000000000000000";

#[test]
fun recovers_signer_address() {
    let recovered = evm_signin::recover_address(MESSAGE, SIGNATURE);
    assert!(recovered == SIGNER, 0);
}

#[test]
fun recovers_with_raw_recovery_id() {
    // 0x1c (28) and 0x01 (raw id 1) must recover the same address.
    let recovered = evm_signin::recover_address(MESSAGE, SIGNATURE_RAW_V);
    assert!(recovered == SIGNER, 0);
}

#[test]
fun execute_stamps_and_returns_ok() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    let mut state: EvmSigninState = scenario.take_shared();
    let witness_id = state.witness_id();

    let from = object::new(scenario.ctx());
    let mut worksheet = proof_of_uid::new(&from);

    let out = evm_signin::execute(
        &mut worksheet,
        &mut state,
        MESSAGE,
        SIGNATURE,
        SIGNER,
        scenario.ctx(),
    );

    // The tool must stamp the worksheet with its witness id.
    assert!(worksheet.has_stamp(witness_id), 0);

    let (tag, _payload) = out.into_parts();
    assert!(tag == b"ok", 1);

    let _ = worksheet.consume(&from);
    object::delete(from);
    test_scenario::return_shared(state);
    scenario.end();
}

#[test]
fun execute_rejects_bad_signature_length() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    let mut state: EvmSigninState = scenario.take_shared();

    let from = object::new(scenario.ctx());
    let mut worksheet = proof_of_uid::new(&from);

    // Truncated signature -> err variant (no abort).
    let out = evm_signin::execute(
        &mut worksheet,
        &mut state,
        MESSAGE,
        x"deadbeef",
        SIGNER,
        scenario.ctx(),
    );

    let (tag, _payload) = out.into_parts();
    assert!(tag == b"err", 0);

    let _ = worksheet.consume(&from);
    object::delete(from);
    test_scenario::return_shared(state);
    scenario.end();
}

#[test]
fun execute_ok_with_empty_expected_skips_check() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    let mut state: EvmSigninState = scenario.take_shared();

    let from = object::new(scenario.ctx());
    let mut worksheet = proof_of_uid::new(&from);

    // Empty expected_address -> recovery still succeeds, check is skipped.
    let out = evm_signin::execute(
        &mut worksheet,
        &mut state,
        MESSAGE,
        SIGNATURE,
        x"",
        scenario.ctx(),
    );

    let (tag, _payload) = out.into_parts();
    assert!(tag == b"ok", 0);

    let _ = worksheet.consume(&from);
    object::delete(from);
    test_scenario::return_shared(state);
    scenario.end();
}

#[test]
fun recovered_address_differs_from_other() {
    let recovered = evm_signin::recover_address(MESSAGE, SIGNATURE);
    assert!(recovered != OTHER_ADDR, 0);
}
