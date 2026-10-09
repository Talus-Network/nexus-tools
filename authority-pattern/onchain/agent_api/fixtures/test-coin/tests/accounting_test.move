module agent_api_test_coin::accounting_test;

use agent_api_test_coin::accounting::{Self, AgentWallet, Cashier};
use nexus_interface::authorization as interface_authorization;
use nexus_interface::version;
use nexus_primitives::authorization as primitive_authorization;
use nexus_primitives::proof_of_uid;
use sui::coin;
use sui::object::{Self, ID};
use sui::test_scenario;
use sui::vec_set;
use std::unit_test::destroy;

const OWNER: address = @0xA;

fun new_state(
    ctx: &mut sui::tx_context::TxContext,
    payment: u64,
    expected_agent_uid: ID,
    rate: u64,
): (AgentWallet<agent_api_test_coin::test_coin::TEST_COIN>, Cashier<agent_api_test_coin::test_coin::TEST_COIN>, accounting::OperatorCap<agent_api_test_coin::test_coin::TEST_COIN>, accounting::SettlementCap<agent_api_test_coin::test_coin::TEST_COIN>, ID) {
    let mut wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(
        OWNER,
        expected_agent_uid,
        ctx,
    );
    accounting::deposit(
        &mut wallet,
        coin::mint_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(payment, ctx),
        ctx,
    );
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, rate);
    let binding_id = accounting::register(
        &wallet,
        &mut cashier,
        false,
        vector[],
        ctx,
    );
    (wallet, cashier, operator_cap, settlement_cap, binding_id)
}

/// Registration is free; production charge, cumulative settlement, revocation,
/// and refund calls conserve the wallet, reserve, and earned revenue balances.
#[test]
fun lifecycle_conserves_payment_coin_and_is_idempotent() {
    let mut scenario = test_scenario::begin(OWNER);
    let ctx = scenario.ctx();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let (mut wallet, mut cashier, operator_cap, settlement_cap, binding_id) =
        new_state(ctx, 1000, expected_agent_uid, 2);

    assert!(accounting::wallet_balance_for_testing(&wallet) == 1000, 0);
    let (active, refunded, rate, charged, credits, usage, reserve) =
        accounting::binding_state_for_testing(&cashier, binding_id);
    assert!(active && !refunded && rate == 2 && charged == 0 && credits == 0 && usage == 0 && reserve == 0, 1);

    accounting::charge(&mut wallet, &mut cashier, binding_id, 100);
    assert!(accounting::wallet_balance_for_testing(&wallet) == 900, 2);
    let (active, refunded, rate, charged, credits, usage, reserve) =
        accounting::binding_state_for_testing(&cashier, binding_id);
    assert!(active && !refunded && rate == 2 && charged == 100 && credits == 200 && usage == 0 && reserve == 100, 3);

    accounting::authorize(
        &wallet,
        &mut cashier,
        binding_id,
        object::id_from_address(@0xE1),
        0,
        b"agent-api",
        0,
        vector::tabulate!(32, |_| 3),
        b"query",
        vector::tabulate!(32, |_| 4),
    );
    assert!(sui::event::events_by_type<accounting::AuthorizationEvent<agent_api_test_coin::test_coin::TEST_COIN>>().length() == 1, 12);

    accounting::settle(&settlement_cap, &mut cashier, binding_id, 40);
    accounting::settle(&settlement_cap, &mut cashier, binding_id, 40);
    assert!(accounting::cashier_revenue_for_testing(&cashier) == 20, 4);
    let (active, refunded, rate, charged, credits, usage, reserve) =
        accounting::binding_state_for_testing(&cashier, binding_id);
    assert!(active && !refunded && rate == 2 && charged == 100 && credits == 200 && usage == 40 && reserve == 80, 5);

    accounting::withdraw_revenue(&operator_cap, &mut cashier, 20, ctx);
    assert!(accounting::cashier_revenue_for_testing(&cashier) == 0, 6);
    accounting::revoke(&wallet, &mut cashier, binding_id);
    accounting::revoke(&wallet, &mut cashier, binding_id);
    assert!(sui::event::events_by_type<accounting::RevokedEvent<agent_api_test_coin::test_coin::TEST_COIN>>().length() == 1, 7);
    accounting::refund(&settlement_cap, &mut cashier, &mut wallet, binding_id);
    accounting::refund(&settlement_cap, &mut cashier, &mut wallet, binding_id);
    assert!(accounting::wallet_balance_for_testing(&wallet) == 980, 8);
    assert!(accounting::cashier_revenue_for_testing(&cashier) == 0, 9);
    let (active, refunded, rate, charged, credits, usage, reserve) =
        accounting::binding_state_for_testing(&cashier, binding_id);
    assert!(!active && refunded && rate == 2 && charged == 100 && credits == 200 && usage == 40 && reserve == 0, 10);
    assert!(sui::event::events_by_type<accounting::RefundedEvent<agent_api_test_coin::test_coin::TEST_COIN>>().length() == 1, 11);

    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    scenario.end();
}

#[test]
fun registrations_allocate_unique_ids_across_cashiers() {
    let mut ctx = sui::tx_context::dummy();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    let (mut first, first_operator, first_settlement) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    let (mut second, second_operator, second_settlement) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&first_operator, &mut first, 2);
    accounting::set_credit_rate(&second_operator, &mut second, 2);
    let first_id = accounting::register(&wallet, &mut first, false, vector[], &mut ctx);
    let second_id = accounting::register(&wallet, &mut second, false, vector[], &mut ctx);
    assert!(first_id != second_id, 20);
    let events = sui::event::events_by_type<accounting::RegistrationEvent<agent_api_test_coin::test_coin::TEST_COIN>>();
    assert!(events.length() == 2, 21);
    assert!(accounting::registration_binding_id_for_testing(&events[0]) == first_id, 22);
    assert!(accounting::registration_binding_id_for_testing(&events[1]) == second_id, 23);
    let (_, _, _, first_charged, _, _, _) = accounting::binding_state_for_testing(&first, first_id);
    let (_, _, _, second_charged, _, _, _) = accounting::binding_state_for_testing(&second, second_id);
    assert!(first_charged == 0 && second_charged == 0, 24);
    destroy(wallet);
    destroy(first);
    destroy(first_operator);
    destroy(first_settlement);
    destroy(second);
    destroy(second_operator);
    destroy(second_settlement);
}

#[test, expected_failure]
fun registration_requires_configured_cashier_rate() {
    let mut ctx = sui::tx_context::dummy();
    let wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(
        OWNER,
        object::id_from_address(@0xA11CE),
        &mut ctx,
    );
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::register(&wallet, &mut cashier, false, vector[], &mut ctx);
    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
}

#[test, expected_failure]
fun zero_credit_rate_cannot_be_configured() {
    let mut ctx = sui::tx_context::dummy();
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, 0);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
}

#[test, expected_failure]
fun operator_cap_cannot_configure_another_cashier() {
    let mut ctx = sui::tx_context::dummy();
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    let (mut other_cashier, other_operator_cap, other_settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&operator_cap, &mut other_cashier, 2);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    destroy(other_cashier);
    destroy(other_operator_cap);
    destroy(other_settlement_cap);
}

/// The shared wallet owner can withdraw uncommitted funds after a charge.
#[test]
fun owner_can_withdraw_only_uncommitted_wallet_funds() {
    let mut scenario = test_scenario::begin(OWNER);
    let ctx = scenario.ctx();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let (mut wallet, mut cashier, operator_cap, settlement_cap, binding_id) =
        new_state(ctx, 100, expected_agent_uid, 2);
    accounting::charge(&mut wallet, &mut cashier, binding_id, 40);
    accounting::withdraw(&mut wallet, 10, ctx);
    assert!(accounting::wallet_balance_for_testing(&wallet) == 50, 0);
    let (_, _, _, charged, credits, _, reserve) =
        accounting::binding_state_for_testing(&cashier, binding_id);
    assert!(charged == 40 && credits == 80 && reserve == 40, 1);

    scenario.next_tx(OWNER);
    let withdrawn: sui::coin::Coin<agent_api_test_coin::test_coin::TEST_COIN> = scenario.take_from_sender();
    assert!(coin::value(&withdrawn) == 10, 2);
    destroy(withdrawn);
    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    scenario.end();
}

/// An underfunded charge aborts; successful charges are checked in the lifecycle conservation test.
#[test, expected_failure]
fun charge_more_than_wallet_balance_aborts() {
    let mut ctx = sui::tx_context::dummy();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let (mut wallet, mut cashier, operator_cap, settlement_cap, binding_id) =
        new_state(&mut ctx, 1, expected_agent_uid, 2);

    accounting::charge(&mut wallet, &mut cashier, binding_id, 2);

    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
}

/// A zero-use revoke returns all charged coin units exactly once.
#[test]
fun zero_usage_revoke_refunds_full_charge_once() {
    let mut scenario = test_scenario::begin(OWNER);
    let ctx = scenario.ctx();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let (mut wallet, mut cashier, operator_cap, settlement_cap, binding_id) =
        new_state(ctx, 1000, expected_agent_uid, 2);

    accounting::charge(&mut wallet, &mut cashier, binding_id, 100);
    accounting::revoke(&wallet, &mut cashier, binding_id);
    accounting::settle(&settlement_cap, &mut cashier, binding_id, 0);
    accounting::settle(&settlement_cap, &mut cashier, binding_id, 0);
    accounting::refund(&settlement_cap, &mut cashier, &mut wallet, binding_id);
    accounting::refund(&settlement_cap, &mut cashier, &mut wallet, binding_id);

    assert!(accounting::wallet_balance_for_testing(&wallet) == 1000, 0);
    assert!(accounting::cashier_revenue_for_testing(&cashier) == 0, 1);
    let (active, refunded, rate, charged, credits, usage, reserve) =
        accounting::binding_state_for_testing(&cashier, binding_id);
    assert!(!active && refunded && rate == 2 && charged == 100, 2);
    assert!(credits == 200 && usage == 0 && reserve == 0, 3);
    assert!(
        sui::event::events_by_type<accounting::RefundedEvent<agent_api_test_coin::test_coin::TEST_COIN>>().length() == 1,
        4,
    );

    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    scenario.end();
}

/// Each binding snapshots its own rate and cumulative ceilings preserve dust.
#[test]
fun distinct_bindings_keep_rate_snapshots_and_refundable_rounding_dust() {
    let mut scenario = test_scenario::begin(OWNER);
    let ctx = scenario.ctx();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let mut wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(
        OWNER,
        expected_agent_uid,
        ctx,
    );
    accounting::deposit(
        &mut wallet,
        coin::mint_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(20, ctx),
        ctx,
    );
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, 2);
    let rate_two = accounting::register(&wallet, &mut cashier, false, vector[], ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, 3);
    let rate_three = accounting::register(&wallet, &mut cashier, false, vector[], ctx);
    accounting::charge(&mut wallet, &mut cashier, rate_two, 10);
    accounting::charge(&mut wallet, &mut cashier, rate_three, 10);
    accounting::settle(&settlement_cap, &mut cashier, rate_two, 5);
    accounting::settle(&settlement_cap, &mut cashier, rate_three, 4);
    assert!(accounting::cashier_revenue_for_testing(&cashier) == 5, 0);
    accounting::revoke(&wallet, &mut cashier, rate_two);
    accounting::refund(&settlement_cap, &mut cashier, &mut wallet, rate_two);
    accounting::revoke(&wallet, &mut cashier, rate_three);
    accounting::refund(&settlement_cap, &mut cashier, &mut wallet, rate_three);
    assert!(accounting::wallet_balance_for_testing(&wallet) == 15, 1);
    let (_, old_refunded, rate, charged, credits, usage, reserve) =
        accounting::binding_state_for_testing(&cashier, rate_two);
    assert!(old_refunded && rate == 2 && charged == 10 && credits == 20 && usage == 5 && reserve == 0, 4);
    let (_, new_refunded, rate, charged, credits, usage, reserve) =
        accounting::binding_state_for_testing(&cashier, rate_three);
    assert!(new_refunded && rate == 3 && charged == 10 && credits == 30 && usage == 4 && reserve == 0, 5);
    assert!(accounting::cashier_revenue_for_testing(&cashier) == 5, 6);
    assert!(
        accounting::wallet_balance_for_testing(&wallet)
            + accounting::cashier_revenue_for_testing(&cashier)
            == 20,
        7,
    );

    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    scenario.end();
}

/// Agent identity provenance is checked before a binding can be used.
#[test]
fun issuer_provenance_must_match_the_wallet_agent_uid() {
    let mut scenario = test_scenario::begin(OWNER);
    let ctx = scenario.ctx();
    let agent_uid = object::new(ctx);
    let agent_id = object::uid_to_inner(&agent_uid);
    let wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, agent_id, ctx);
    let proven = primitive_authorization::wrap(&agent_uid, agent_id);
    accounting::assert_agent(&proven, &wallet);
    primitive_authorization::drop(proven);
    agent_uid.delete();
    destroy(wallet);
    scenario.end();
}

/// UIDRequirements are completed only by this cashier's registered Tool witness.
#[test]
fun registered_witness_satisfies_framework_requirements() {
    let mut scenario = test_scenario::begin(OWNER);
    let ctx = scenario.ctx();
    let (cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, ctx);
    let execution_uid = object::new(ctx);
    let witness_id = object::uid_to_inner(accounting::tool_witness(&cashier, b"register"));
    let worksheet = proof_of_uid::new(&execution_uid);
    let remaining = vec_set::singleton(witness_id);
    let mut requirements = worksheet.into_requirements(&execution_uid, remaining);
    accounting::satisfy_witness(&mut requirements, &cashier, b"register");
    let stamps = requirements.complete();
    assert!(stamps.contains(&witness_id), 0);

    execution_uid.delete();
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    scenario.end();
}

/// Framework-issued values retain their recipient restriction when unwrapped.
#[test]
fun recipient_bound_proof_is_only_unwrapped_by_its_recipient() {
    let mut ctx = sui::tx_context::dummy();
    let issuer = object::new(&mut ctx);
    let recipient = object::new(&mut ctx);
    let recipient_id = object::uid_to_inner(&recipient);
    let proven = primitive_authorization::wrap_for_recipient(&issuer, 7u64, recipient_id);
    assert!(primitive_authorization::unwrap_as_recipient(proven, &recipient) == 7, 0);
    issuer.delete();
    recipient.delete();
}

#[test, expected_failure]
fun issuer_from_another_agent_cannot_use_the_wallet() {
    let mut ctx = sui::tx_context::dummy();
    let agent_uid = object::new(&mut ctx);
    let wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(
        OWNER,
        object::id_from_address(@0xBEEF),
        &mut ctx,
    );
    let proven = primitive_authorization::wrap(&agent_uid, object::uid_to_inner(&agent_uid));
    accounting::assert_agent(&proven, &wallet);
    primitive_authorization::drop(proven);
    agent_uid.delete();
    destroy(wallet);
}

#[test, expected_failure]
fun wrong_wallet_cannot_charge_an_existing_binding() {
    let mut ctx = sui::tx_context::dummy();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let first = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    let mut second = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, 2);
    let binding_id = accounting::register(&first, &mut cashier, false, vector[], &mut ctx);
    accounting::charge(&mut second, &mut cashier, binding_id, 1);
    destroy(first);
    destroy(second);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
}

#[test, expected_failure]
fun wrong_cashier_cannot_use_another_cashiers_binding() {
    let mut ctx = sui::tx_context::dummy();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let mut wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    let (mut first, first_operator, first_settlement) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    let (mut second, second_operator, second_settlement) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&first_operator, &mut first, 2);
    let binding_id = accounting::register(&wallet, &mut first, false, vector[], &mut ctx);
    accounting::charge(&mut wallet, &mut second, binding_id, 1);
    destroy(wallet);
    destroy(first);
    destroy(first_operator);
    destroy(first_settlement);
    destroy(second);
    destroy(second_operator);
    destroy(second_settlement);
}

#[test, expected_failure]
fun refund_cannot_return_another_bindings_reserve() {
    let mut ctx = sui::tx_context::dummy();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let mut wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    let mut wrong_wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, 2);
    let binding_id = accounting::register(&wallet, &mut cashier, false, vector[], &mut ctx);
    accounting::charge(&mut wallet, &mut cashier, binding_id, 1);
    accounting::revoke(&wallet, &mut cashier, binding_id);
    accounting::refund(&settlement_cap, &mut cashier, &mut wrong_wallet, binding_id);
    destroy(wallet);
    destroy(wrong_wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
}

#[test, expected_failure]
fun operator_cannot_withdraw_refundable_reserve_as_revenue() {
    let mut ctx = sui::tx_context::dummy();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let mut wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, 2);
    let binding_id = accounting::register(&wallet, &mut cashier, false, vector[], &mut ctx);
    accounting::charge(&mut wallet, &mut cashier, binding_id, 1);
    accounting::withdraw_revenue(&operator_cap, &mut cashier, 1, &mut ctx);
    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
}

#[test, expected_failure]
fun charge_overflow_aborts_before_reserve_movement() {
    let mut ctx = sui::tx_context::dummy();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let mut wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, expected_agent_uid, &mut ctx);
    accounting::deposit(
        &mut wallet,
        coin::mint_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(2, &mut ctx),
        &mut ctx,
    );
    let (mut cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    accounting::set_credit_rate(&operator_cap, &mut cashier, 18446744073709551615);
    let binding_id = accounting::register(&wallet, &mut cashier, false, vector[], &mut ctx);
    accounting::charge(&mut wallet, &mut cashier, binding_id, 2);
    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
}

#[test, expected_failure]
fun a_different_tool_witness_cannot_complete_uid_requirements() {
    let mut ctx = sui::tx_context::dummy();
    let (first, first_operator, first_settlement) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    let (second, second_operator, second_settlement) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    let execution_uid = object::new(&mut ctx);
    let wrong_witness = object::uid_to_inner(accounting::tool_witness(&second, b"register"));
    let worksheet = proof_of_uid::new(&execution_uid);
    let remaining = vec_set::singleton(wrong_witness);
    let mut requirements = worksheet.into_requirements(&execution_uid, remaining);
    accounting::satisfy_witness(&mut requirements, &first, b"register");
    let _ = requirements.complete();
    execution_uid.delete();
    destroy(first);
    destroy(first_operator);
    destroy(first_settlement);
    destroy(second);
    destroy(second_operator);
    destroy(second_settlement);
}

#[test, expected_failure]
fun reused_target_nonce_cannot_issue_a_second_grant() {
    let mut scenario = test_scenario::begin(OWNER);
    let ctx = scenario.ctx();
    let expected_agent_uid = object::id_from_address(@0xA11CE);
    let (mut wallet, mut cashier, operator_cap, settlement_cap, binding_id) =
        new_state(ctx, 10, expected_agent_uid, 2);
    accounting::charge(&mut wallet, &mut cashier, binding_id, 5);
    accounting::authorize(
        &wallet,
        &mut cashier,
        binding_id,
        object::id_from_address(@0xE1),
        0,
        b"agent-api",
        0,
        vector::tabulate!(32, |_| 3),
        b"query",
        vector::tabulate!(32, |_| 4),
    );
    accounting::authorize(
        &wallet,
        &mut cashier,
        binding_id,
        object::id_from_address(@0xE2),
        0,
        b"agent-api",
        0,
        vector::tabulate!(32, |_| 3),
        b"query",
        vector::tabulate!(32, |_| 4),
    );
    destroy(wallet);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    scenario.end();
}

#[test, expected_failure]
fun a_different_uid_cannot_unwrap_a_recipient_bound_value() {
    let mut ctx = sui::tx_context::dummy();
    let issuer = object::new(&mut ctx);
    let recipient = object::new(&mut ctx);
    let other = object::new(&mut ctx);
    let proven = primitive_authorization::wrap_for_recipient(
        &issuer,
        7u64,
        object::uid_to_inner(&recipient),
    );
    let _ = primitive_authorization::unwrap_as_recipient(proven, &other);
    issuer.delete();
    object::delete(recipient);
    object::delete(other);
}

#[test, expected_failure]
fun worksheet_rejects_a_different_input_commitment() {
    let mut ctx = sui::tx_context::dummy();
    let execution_uid = object::new(&mut ctx);
    let stamp = interface_authorization::agent_vertex_authorization_stamp(
        interface_authorization::agent_vertex_authorization_context(
            object::id_from_address(@0xA11CE),
            7,
            version::v(3),
            object::uid_to_inner(&execution_uid),
            b"agent-api".to_ascii_string(),
            object::id_from_address(@0xCAFE),
        ),
        vector::tabulate!(32, |_| 7),
    );
    let mut worksheet = proof_of_uid::new(&execution_uid);
    worksheet.stamp_with_data(&execution_uid, std::bcs::to_bytes(&stamp));
    let wrong_stamp = interface_authorization::agent_vertex_authorization_stamp(
        interface_authorization::agent_vertex_authorization_context(
            object::id_from_address(@0xA11CE),
            7,
            version::v(3),
            object::uid_to_inner(&execution_uid),
            b"agent-api".to_ascii_string(),
            object::id_from_address(@0xCAFE),
        ),
        vector::tabulate!(32, |_| 8),
    );
    let _ = interface_authorization::worksheet_input_commitment(&worksheet, &wrong_stamp);
    proof_of_uid::destroy_for_testing(worksheet);
    execution_uid.delete();
}
