#[test_only]
module evm_signin::evm_access_test;

use evm_signin::evm_access::{Self, EvmBindingRegistry, EvmIdentityPass, EvmMemberBadge};
use evm_signin::evm_signin::{Self, EVM_SIGNIN};
use sui::test_scenario;
use sui::test_utils;

const USER: address = @0xA11CE;
const OTHER: address = @0xB0B;

// SIGNER / SIGNATURE are NOT arbitrary: they are produced from the well-known
// public Anvil/Hardhat test account #0 (a standard, non-secret test key).
// Reproduce exactly:
//   PK=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
//   cast wallet address --private-key $PK
//     -> 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266   (== SIGNER)
//   cast wallet sign --private-key $PK \
//     "Bind my Ethereum address to Sui account: 0x00000000000000000000000000000000000000000000000000000000000a11ce"
//     -> 0x073b...1c                                  (== SIGNATURE)
// The Sui account in the statement is USER (@0xA11CE) left-padded to 32 bytes.
const SIGNER: vector<u8> = x"f39fd6e51aad88f6f4ce6ab8827279cfffb92266";
const SIGNATURE: vector<u8> =
    x"073bb824168198df8af80de7cdce73d69191b975ecfb1e5beab9db7f0686acdb19cf36d8d90d1ab2bffbc3a93e8afea3fc01399d47aa23bc15a880d21d0441931c";

#[test]
fun package_init_creates_registry() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    // Publish init must create the binding registry alongside tool state.
    let _registry = scenario.take_shared<EvmBindingRegistry>();
    let _state = scenario.take_shared<evm_signin::EvmSigninState>();
    test_scenario::return_shared(_registry);
    test_scenario::return_shared(_state);
    scenario.end();
}

#[test]
fun bind_mints_pass_and_records_binding() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    let mut registry = scenario.take_shared<EvmBindingRegistry>();
    evm_access::bind(&mut registry, SIGNATURE, scenario.ctx());
    test_scenario::return_shared(registry);

    // bind must emit exactly one EvmAddressBound event.
    let effects = scenario.next_tx(USER);
    assert!(effects.num_user_events() == 1, 4);

    let pass = scenario.take_from_sender<EvmIdentityPass>();
    assert!(evm_access::eth_address(&pass) == SIGNER, 0);
    assert!(evm_access::sui_owner(&pass) == USER, 1);

    let registry = scenario.take_shared<EvmBindingRegistry>();
    assert!(evm_access::is_bound(&registry, SIGNER), 2);
    assert!(evm_access::owner_of(&registry, SIGNER) == USER, 3);
    test_scenario::return_shared(registry);

    scenario.return_to_sender(pass);
    scenario.end();
}

#[test]
fun claim_member_badge_gated_by_pass() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    // Bind to obtain the identity pass.
    let mut registry = scenario.take_shared<EvmBindingRegistry>();
    evm_access::bind(&mut registry, SIGNATURE, scenario.ctx());
    test_scenario::return_shared(registry);

    scenario.next_tx(USER);

    // Use the pass to claim a gated member badge.
    let pass = scenario.take_from_sender<EvmIdentityPass>();
    evm_access::claim_member_badge(&pass, scenario.ctx());

    let effects = scenario.next_tx(USER);
    assert!(effects.num_user_events() == 1, 0);

    let badge = scenario.take_from_sender<EvmMemberBadge>();
    assert!(evm_access::badge_eth_address(&badge) == SIGNER, 1);

    scenario.return_to_sender(badge);
    scenario.return_to_sender(pass);
    scenario.end();
}

#[test]
fun assert_controls_accepts_correct_address() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    let mut registry = scenario.take_shared<EvmBindingRegistry>();
    evm_access::bind(&mut registry, SIGNATURE, scenario.ctx());
    test_scenario::return_shared(registry);

    scenario.next_tx(USER);

    let pass = scenario.take_from_sender<EvmIdentityPass>();
    evm_access::assert_controls(&pass, SIGNER);

    scenario.return_to_sender(pass);
    scenario.end();
}

#[test]
#[expected_failure(abort_code = evm_access::ENotController)]
fun assert_controls_rejects_wrong_address() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    let mut registry = scenario.take_shared<EvmBindingRegistry>();
    evm_access::bind(&mut registry, SIGNATURE, scenario.ctx());
    test_scenario::return_shared(registry);

    scenario.next_tx(USER);

    let pass = scenario.take_from_sender<EvmIdentityPass>();
    // Wrong address must abort.
    evm_access::assert_controls(&pass, x"0000000000000000000000000000000000000000");

    scenario.return_to_sender(pass);
    scenario.end();
}

#[test]
#[expected_failure(abort_code = evm_access::EAlreadyBound)]
fun double_bind_aborts() {
    let mut scenario = test_scenario::begin(USER);
    {
        let otw = test_utils::create_one_time_witness<EVM_SIGNIN>();
        evm_signin::init_for_test(otw, scenario.ctx());
    };
    scenario.next_tx(USER);

    let mut registry = scenario.take_shared<EvmBindingRegistry>();
    evm_access::bind(&mut registry, SIGNATURE, scenario.ctx());
    evm_access::bind(&mut registry, SIGNATURE, scenario.ctx());
    test_scenario::return_shared(registry);

    scenario.end();
}

#[test]
fun binding_is_replay_safe() {
    let preimage_user = evm_access::bind_preimage_for_test(USER);
    let preimage_other = evm_access::bind_preimage_for_test(OTHER);

    let eth_user = evm_signin::recover_address(preimage_user, SIGNATURE);
    let eth_other = evm_signin::recover_address(preimage_other, SIGNATURE);

    assert!(eth_user == SIGNER, 0);
    assert!(eth_other != SIGNER, 1);
}
