module agent_api_sui::accounting;

use sui::balance::{Self, Balance};
use sui::bag::{Self, Bag};
use sui::coin::{Self, Coin};
use sui::event;
use sui::object::{Self, ID, UID};
use sui::table::{Self, Table};
use sui::transfer;
use sui::tx_context::{Self, TxContext};
use nexus_primitives::proof_of_uid::UIDRequirements;
use std::vector;

#[error]
const EInvalidConfiguration: vector<u8> = b"invalid agent API configuration";
#[error]
const EUnauthorized: vector<u8> = b"agent API authorization failed";
#[error]
const EBindingMismatch: vector<u8> = b"binding does not match wallet or cashier";
#[error]
const EBindingInactive: vector<u8> = b"binding is inactive";
#[error]
const EInsufficientReserve: vector<u8> = b"binding reserve is insufficient";
#[error]
const EAlreadyRefunded: vector<u8> = b"binding refund was already completed";
#[error]
const EUsageRegression: vector<u8> = b"provider usage regressed";
#[error]
const EGrantReuse: vector<u8> = b"authorization nonce was already issued";
#[error]
const ENotRevoked: vector<u8> = b"binding must be revoked before refund";

/// A wallet shared by one owner and one Nexus Agent identity.
public struct AgentWallet<phantom T> has key {
    id: UID,
    owner: address,
    expected_agent_uid: ID,
    balance: Balance<T>,
}

/// Provider-key binding and its independently refundable coin reserve.
public struct Binding<phantom T> has store {
    wallet_id: ID,
    owner: address,
    expected_agent_uid: ID,
    rate: u64,
    export_enabled: bool,
    owner_public_key: vector<u8>,
    active: bool,
    refunded: bool,
    charged_coin_units: u64,
    credit_units: u64,
    settled_usage: u64,
    recognized_coin_units: u64,
    reserve: Balance<T>,
    issued_nonces: Table<vector<u8>, bool>,
}

/// One cashier per supported payment coin. Revenue is unavailable to refunds.
public struct Cashier<phantom T> has key {
    id: UID,
    operator: address,
    credit_rate: u64,
    earned_revenue: Balance<T>,
    bindings: Table<ID, Binding<T>>,
    witnesses: Bag,
}

/// A Tool registration witness nested in the cashier.
public struct ToolWitness has key, store {
    id: UID,
}

/// May withdraw only earned cashier revenue.
public struct OperatorCap<phantom T> has key, store {
    id: UID,
    cashier_id: ID,
}

/// May settle usage and return reserves to their original wallets.
public struct SettlementCap<phantom T> has key, store {
    id: UID,
    cashier_id: ID,
}

public struct RegistrationEvent<phantom T> has copy, drop {
    cashier_id: ID,
    wallet_id: ID,
    binding_id: ID,
    owner: address,
    expected_agent_uid: ID,
    rate: u64,
    export_enabled: bool,
    owner_public_key: vector<u8>,
}

public struct ChargeEvent<phantom T> has copy, drop {
    cashier_id: ID,
    wallet_id: ID,
    binding_id: ID,
    coin_units: u64,
    credit_units: u64,
    rate: u64,
}

public struct AuthorizationEvent<phantom T> has copy, drop {
    cashier_id: ID,
    binding_id: ID,
    execution_id: ID,
    walk_index: u64,
    target_vertex: vector<u8>,
    iteration: u64,
    target_nonce: vector<u8>,
    operation: vector<u8>,
    input_hash: vector<u8>,
}

public struct RevokedEvent<phantom T> has copy, drop {
    cashier_id: ID,
    wallet_id: ID,
    binding_id: ID,
}

public struct UsageSettledEvent<phantom T> has copy, drop {
    cashier_id: ID,
    binding_id: ID,
    cumulative_usage: u64,
    coin_units_recognized: u64,
}

public struct RefundedEvent<phantom T> has copy, drop {
    cashier_id: ID,
    wallet_id: ID,
    binding_id: ID,
    coin_units: u64,
}

public struct ACCOUNTING has drop {}

/// Creates one owner-bound shared wallet for this payment coin.
public fun create_wallet<T>(expected_agent_uid: ID, ctx: &mut TxContext) {
    let wallet: AgentWallet<T> = new_wallet(tx_context::sender(ctx), expected_agent_uid, ctx);
    transfer::share_object(wallet);
}

fun new_wallet<T>(owner: address, expected_agent_uid: ID, ctx: &mut TxContext): AgentWallet<T> {
    AgentWallet<T> {
        id: object::new(ctx),
        owner,
        expected_agent_uid,
        balance: balance::zero<T>(),
    }
}

#[test_only]
public(package) fun new_wallet_for_testing<T>(
    owner: address,
    expected_agent_uid: ID,
    ctx: &mut TxContext,
): AgentWallet<T> {
    new_wallet(owner, expected_agent_uid, ctx)
}

#[test_only]
public(package) fun new_cashier_for_testing<T>(
    operator: address,
    ctx: &mut TxContext,
): (Cashier<T>, OperatorCap<T>, SettlementCap<T>) {
    let mut witnesses = bag::new(ctx);
    witnesses.add(b"register", ToolWitness { id: object::new(ctx) });
    witnesses.add(b"charge", ToolWitness { id: object::new(ctx) });
    witnesses.add(b"authorize", ToolWitness { id: object::new(ctx) });
    witnesses.add(b"revoke", ToolWitness { id: object::new(ctx) });
    let cashier = Cashier<T> {
        id: object::new(ctx),
        operator,
        credit_rate: 0,
        earned_revenue: balance::zero<T>(),
        bindings: table::new(ctx),
        witnesses,
    };
    let cashier_id = object::id(&cashier);
    let operator_cap = OperatorCap<T> { id: object::new(ctx), cashier_id };
    let settlement_cap = SettlementCap<T> { id: object::new(ctx), cashier_id };
    (cashier, operator_cap, settlement_cap)
}

#[test_only]
public(package) fun wallet_balance_for_testing<T>(wallet: &AgentWallet<T>): u64 {
    balance::value(&wallet.balance)
}

#[test_only]
public(package) fun cashier_revenue_for_testing<T>(cashier: &Cashier<T>): u64 {
    balance::value(&cashier.earned_revenue)
}

#[test_only]
public(package) fun binding_state_for_testing<T>(
    cashier: &Cashier<T>,
    binding_id: ID,
): (bool, bool, u64, u64, u64, u64, u64) {
    let binding = table::borrow(&cashier.bindings, binding_id);
    (
        binding.active,
        binding.refunded,
        binding.rate,
        binding.charged_coin_units,
        binding.credit_units,
        binding.settled_usage,
        balance::value(&binding.reserve),
    )
}

#[test_only]
public(package) fun registration_binding_id_for_testing<T>(event: &RegistrationEvent<T>): ID {
    event.binding_id
}

#[test_only]
public(package) fun binding_owner_public_key_for_testing<T>(
    cashier: &Cashier<T>,
    binding_id: ID,
): vector<u8> {
    table::borrow(&cashier.bindings, binding_id).owner_public_key
}

#[test_only]
public(package) fun registration_owner_public_key_for_testing<T>(
    event: &RegistrationEvent<T>,
): vector<u8> {
    event.owner_public_key
}

/// Adds payment coins to the shared wallet.
public fun deposit<T>(wallet: &mut AgentWallet<T>, coin: Coin<T>, ctx: &mut TxContext) {
    assert!(tx_context::sender(ctx) == wallet.owner, EUnauthorized);
    balance::join(&mut wallet.balance, coin::into_balance(coin));
}

/// Withdraws only the wallet owner's uncommitted balance.
public fun withdraw<T>(wallet: &mut AgentWallet<T>, coin_units: u64, ctx: &mut TxContext) {
    assert!(tx_context::sender(ctx) == wallet.owner, EUnauthorized);
    let coin = coin::from_balance(balance::split(&mut wallet.balance, coin_units), ctx);
    transfer::public_transfer(coin, wallet.owner);
}

/// Returns the shared wallet UID for exact authorization recipient checks.
public(package) fun wallet_uid<T>(wallet: &AgentWallet<T>): &UID { &wallet.id }

/// Returns the cashier UID for source and witness checks.
public(package) fun cashier_uid<T>(cashier: &Cashier<T>): &UID { &cashier.id }

/// Confirms the proven Agent identity before the authorization is consumed.
public(package) fun assert_agent<T, A>(
    proven: &nexus_primitives::authorization::ProvenValue<A>,
    wallet: &AgentWallet<T>,
) {
    assert_expected_agent(nexus_primitives::authorization::by(proven), wallet);
}

fun assert_expected_agent<T>(proven_agent_uid: ID, wallet: &AgentWallet<T>) {
    assert!(
        proven_agent_uid == wallet.expected_agent_uid,
        EUnauthorized,
    );
}

/// Returns a distinct nested Tool witness for the selected operation.
public(package) fun tool_witness<T>(cashier: &Cashier<T>, operation: vector<u8>): &UID {
    &bag::borrow<vector<u8>, ToolWitness>(&cashier.witnesses, operation).id
}

public(package) fun satisfy_witness<T>(
    requirements: &mut UIDRequirements,
    cashier: &Cashier<T>,
    operation: vector<u8>,
) {
    requirements.satisfy(tool_witness(cashier, operation));
}

/// Registers a binding without taking payment.
public(package) fun register<T>(
    wallet: &AgentWallet<T>,
    cashier: &mut Cashier<T>,
    export_enabled: bool,
    owner_public_key: vector<u8>,
    ctx: &mut TxContext,
): ID {
    assert!(cashier.credit_rate > 0, EInvalidConfiguration);
    assert!(
        (export_enabled && owner_public_key.length() == 32) ||
            (!export_enabled && owner_public_key.is_empty()),
        EInvalidConfiguration,
    );
    let binding_uid = object::new(ctx);
    let binding_id = object::uid_to_inner(&binding_uid);
    binding_uid.delete();
    assert!(!table::contains(&cashier.bindings, binding_id), EInvalidConfiguration);
    let wallet_id = object::id(wallet);
    let expected_agent_uid = wallet.expected_agent_uid;
    let owner = wallet.owner;
    let cashier_id = object::id(cashier);
    let rate = cashier.credit_rate;
    table::add(
        &mut cashier.bindings,
        binding_id,
        Binding<T> {
            wallet_id,
            owner,
            expected_agent_uid,
            rate,
            export_enabled,
            owner_public_key,
            active: true,
            refunded: false,
            charged_coin_units: 0,
            credit_units: 0,
            settled_usage: 0,
            recognized_coin_units: 0,
            reserve: balance::zero<T>(),
            issued_nonces: table::new(ctx),
        },
    );
    event::emit(RegistrationEvent<T> {
        cashier_id,
        wallet_id,
        binding_id,
        owner,
        expected_agent_uid,
        rate,
        export_enabled,
        owner_public_key,
    });
    binding_id
}

/// Sets the positive provider-credit rate for future registrations.
public fun set_credit_rate<T>(
    cap: &OperatorCap<T>,
    cashier: &mut Cashier<T>,
    credit_rate: u64,
) {
    assert!(cap.cashier_id == object::id(cashier), EUnauthorized);
    assert!(credit_rate > 0, EInvalidConfiguration);
    cashier.credit_rate = credit_rate;
}

/// Moves wallet funds to one binding's refundable reserve and emits its provider credit.
public(package) fun charge<T>(
    wallet: &mut AgentWallet<T>,
    cashier: &mut Cashier<T>,
    binding_id: ID,
    coin_units: u64,
) {
    assert!(coin_units > 0, EInvalidConfiguration);
    let wallet_id = object::id(wallet);
    let cashier_id = object::id(cashier);
    let binding = table::borrow_mut(&mut cashier.bindings, binding_id);
    assert!(
        binding.wallet_id == wallet_id && binding.expected_agent_uid == wallet.expected_agent_uid,
        EBindingMismatch,
    );
    assert!(binding.active && !binding.refunded, EBindingInactive);
    let credit_units = coin_units * binding.rate;
    let reserve = balance::split(&mut wallet.balance, coin_units);
    balance::join(&mut binding.reserve, reserve);
    binding.charged_coin_units = binding.charged_coin_units + coin_units;
    binding.credit_units = binding.credit_units + credit_units;
    event::emit(ChargeEvent<T> {
        cashier_id,
        wallet_id,
        binding_id,
        coin_units,
        credit_units,
        rate: binding.rate,
    });
}

/// Converts cumulative provider usage to its once-rounded coin liability.
public(package) fun coin_units_for_usage(credit_units: u64, rate: u64): u64 {
    assert!(rate > 0, EInvalidConfiguration);
    let whole = credit_units / rate;
    if (credit_units % rate == 0) whole else whole + 1
}

/// Issues one grant for one downstream Tool call.
public(package) fun authorize<T>(
    wallet: &AgentWallet<T>,
    cashier: &mut Cashier<T>,
    binding_id: ID,
    execution_id: ID,
    walk_index: u64,
    target_vertex: vector<u8>,
    iteration: u64,
    target_nonce: vector<u8>,
    operation: vector<u8>,
    input_hash: vector<u8>,
) {
    assert!(input_hash.length() == 32 && target_nonce.length() == 32, EInvalidConfiguration);
    assert!(operation == b"query" || operation == b"retrieve-key", EInvalidConfiguration);
    let cashier_id = object::id(cashier);
    let binding = table::borrow_mut(&mut cashier.bindings, binding_id);
    assert!(
        binding.wallet_id == object::id(wallet) &&
            binding.expected_agent_uid == wallet.expected_agent_uid,
        EBindingMismatch,
    );
    assert!(binding.active && !binding.refunded, EBindingInactive);
    assert!(binding.credit_units > binding.settled_usage, EInsufficientReserve);
    assert!(!table::contains(&binding.issued_nonces, target_nonce), EGrantReuse);
    table::add(&mut binding.issued_nonces, target_nonce, true);
    event::emit(AuthorizationEvent<T> {
        cashier_id,
        binding_id,
        execution_id,
        walk_index,
        target_vertex,
        iteration,
        target_nonce,
        operation,
        input_hash,
    });
}

/// Disables an active provider binding. Funding remains refundable.
public(package) fun revoke<T>(wallet: &AgentWallet<T>, cashier: &mut Cashier<T>, binding_id: ID) {
    let cashier_id = object::id(cashier);
    let binding = table::borrow_mut(&mut cashier.bindings, binding_id);
    assert!(
        binding.wallet_id == object::id(wallet) &&
            binding.expected_agent_uid == wallet.expected_agent_uid,
        EBindingMismatch,
    );
    if (binding.active) {
        binding.active = false;
        event::emit(RevokedEvent<T> {
            cashier_id,
            wallet_id: binding.wallet_id,
            binding_id,
        });
    };
}

/// Applies monotonic final provider usage to earned revenue.
public fun settle<T>(
    cap: &SettlementCap<T>,
    cashier: &mut Cashier<T>,
    binding_id: ID,
    cumulative_usage: u64,
) {
    assert!(cap.cashier_id == object::id(cashier), EUnauthorized);
    let binding = table::borrow_mut(&mut cashier.bindings, binding_id);
    assert!(!binding.refunded, EAlreadyRefunded);
    assert!(cumulative_usage >= binding.settled_usage, EUsageRegression);
    assert!(cumulative_usage <= binding.credit_units, EInsufficientReserve);
    if (cumulative_usage == binding.settled_usage) return;
    let new_liability = coin_units_for_usage(cumulative_usage, binding.rate);
    assert!(new_liability >= binding.recognized_coin_units, EUsageRegression);
    let moved = new_liability - binding.recognized_coin_units;
    assert!(moved <= balance::value(&binding.reserve), EInsufficientReserve);
    if (moved > 0) {
        let earned = balance::split(&mut binding.reserve, moved);
        balance::join(&mut cashier.earned_revenue, earned);
    };
    binding.settled_usage = cumulative_usage;
    binding.recognized_coin_units = new_liability;
    event::emit(UsageSettledEvent<T> {
        cashier_id: object::id(cashier),
        binding_id,
        cumulative_usage,
        coin_units_recognized: new_liability,
    });
}

/// Returns a revoked binding's complete remaining reserve to its original wallet exactly once.
public fun refund<T>(
    cap: &SettlementCap<T>,
    cashier: &mut Cashier<T>,
    wallet: &mut AgentWallet<T>,
    binding_id: ID,
) {
    assert!(cap.cashier_id == object::id(cashier), EUnauthorized);
    let binding = table::borrow_mut(&mut cashier.bindings, binding_id);
    assert!(binding.wallet_id == object::id(wallet), EBindingMismatch);
    assert!(binding.expected_agent_uid == wallet.expected_agent_uid, EBindingMismatch);
    if (!binding.refunded) {
        assert!(!binding.active, ENotRevoked);
        let coin_units = balance::value(&binding.reserve);
        let remaining = balance::split(&mut binding.reserve, coin_units);
        balance::join(&mut wallet.balance, remaining);
        binding.refunded = true;
        event::emit(RefundedEvent<T> {
            cashier_id: object::id(cashier),
            wallet_id: object::id(wallet),
            binding_id,
            coin_units,
        });
    };
}

/// Withdraws earned revenue only; refundable binding reserves are inaccessible here.
#[allow(lint(self_transfer))]
public fun withdraw_revenue<T>(
    cap: &OperatorCap<T>,
    cashier: &mut Cashier<T>,
    coin_units: u64,
    ctx: &mut TxContext,
) {
    assert!(cap.cashier_id == object::id(cashier), EUnauthorized);
    let coin = coin::from_balance(balance::split(&mut cashier.earned_revenue, coin_units), ctx);
    transfer::public_transfer(coin, tx_context::sender(ctx));
}

fun init(_otw: ACCOUNTING, ctx: &mut TxContext) {
    let mut witnesses = bag::new(ctx);
    witnesses.add(b"register", ToolWitness { id: object::new(ctx) });
    witnesses.add(b"charge", ToolWitness { id: object::new(ctx) });
    witnesses.add(b"authorize", ToolWitness { id: object::new(ctx) });
    witnesses.add(b"revoke", ToolWitness { id: object::new(ctx) });
    let cashier = Cashier<0x2::sui::SUI> {
        id: object::new(ctx),
        operator: tx_context::sender(ctx),
        credit_rate: 0,
        earned_revenue: balance::zero<0x2::sui::SUI>(),
        bindings: table::new(ctx),
        witnesses,
    };
    let cashier_id = object::id(&cashier);
    transfer::share_object(cashier);
    transfer::public_transfer(
        OperatorCap<0x2::sui::SUI> { id: object::new(ctx), cashier_id },
        tx_context::sender(ctx),
    );
    transfer::public_transfer(
        SettlementCap<0x2::sui::SUI> { id: object::new(ctx), cashier_id },
        tx_context::sender(ctx),
    );
}
