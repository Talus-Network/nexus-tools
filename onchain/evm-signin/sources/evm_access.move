/// EVM identity binding and access control for Sui.
///
/// Turns a verified Ethereum signature into real, replay-safe Sui actions:
///
/// 1. `bind` — an EVM user signs a statement naming *their Sui account*; this
///    module recovers the signer, records the `0x` -> Sui binding, and mints a
///    soulbound [`EvmIdentityPass`].
/// 2. `assert_controls` — a reusable gate any dApp can call: "only the holder of
///    this Ethereum address may proceed."
/// 3. `claim_member_badge` — a concrete example of gating a Sui action behind
///    the pass, closing the MetaMask -> Sui loop end to end.
module evm_signin::evm_access;

use evm_signin::evm_crypto;
use sui::address;
use sui::event;
use sui::table::{Self, Table};
use sui::transfer;

// === Errors ===

/// The recovered binding already exists for this Ethereum address.
const EAlreadyBound: u64 = 1;
/// Signature is not 65 bytes (r, s, v).
const EInvalidSignatureLength: u64 = 2;
/// The provided pass does not control the required Ethereum address.
const ENotController: u64 = 3;

// === Constants ===

/// Fixed statement prefix the EVM user must sign, followed by their Sui address
/// as a `0x`-prefixed hex string. Keep in sync with any client that produces
/// the signature.
const STATEMENT_PREFIX: vector<u8> = b"Bind my Ethereum address to Sui account: ";

// === Objects ===

/// Soulbound proof that a Sui account controls an Ethereum address.
/// `key` only (no `store`) and no transfer function: it cannot be moved once
/// minted.
public struct EvmIdentityPass has key {
    id: UID,
    eth_address: vector<u8>,
    sui_owner: address,
}

/// Soulbound badge gated behind EVM identity. Minting requires the
/// `EvmIdentityPass`, so only an account that has proven control of an Ethereum
/// address can claim it.
public struct EvmMemberBadge has key {
    id: UID,
    eth_address: vector<u8>,
}

/// Shared registry mapping an Ethereum address to the Sui account bound to it.
public struct EvmBindingRegistry has key {
    id: UID,
    bound: Table<vector<u8>, address>,
}

// === Events ===

/// Emitted when an Ethereum address is bound to a Sui account.
public struct EvmAddressBound has copy, drop {
    eth_address: vector<u8>,
    sui_owner: address,
}

/// Emitted when a member badge is claimed.
public struct MemberBadgeClaimed has copy, drop {
    eth_address: vector<u8>,
    sui_owner: address,
}

// === Registry lifecycle ===

/// Create and share a new empty binding registry. Called from `evm_signin::init`
/// on publish so both shared objects exist after a single package init.
public fun share_registry(ctx: &mut TxContext) {
    transfer::share_object(new_registry(ctx));
}

/// Create a new empty binding registry.
public fun new_registry(ctx: &mut TxContext): EvmBindingRegistry {
    EvmBindingRegistry {
        id: object::new(ctx),
        bound: table::new(ctx),
    }
}

// === Binding ===

/// Bind the caller's Sui account to the Ethereum address that signed the
/// canonical statement for `ctx.sender()`, and mint a soulbound pass.
///
/// The caller does not pass the message: the contract reconstructs it from
/// `ctx.sender()`, so the signature is provably an authorization for *this* Sui
/// account and cannot be replayed under another account.
entry fun bind(registry: &mut EvmBindingRegistry, signature: vector<u8>, ctx: &mut TxContext) {
    assert!(signature.length() == 65, EInvalidSignatureLength);

    let sender = ctx.sender();
    let preimage = bind_preimage(sender);
    let eth_address = evm_crypto::recover_address(preimage, signature);

    assert!(!registry.bound.contains(eth_address), EAlreadyBound);
    registry.bound.add(eth_address, sender);

    event::emit(EvmAddressBound { eth_address, sui_owner: sender });

    transfer::transfer(
        EvmIdentityPass {
            id: object::new(ctx),
            eth_address,
            sui_owner: sender,
        },
        sender,
    );
}

// === Access control ===

/// Abort unless `pass` proves control of `eth_address`. Reusable gate primitive:
/// a dApp can require "only the holder of this Ethereum address may proceed."
public fun assert_controls(pass: &EvmIdentityPass, eth_address: vector<u8>) {
    assert!(pass.eth_address == eth_address, ENotController);
}

/// Example gated action: mint a soulbound member badge. Requires the caller to
/// own an `EvmIdentityPass` (a soulbound object only its owner can pass by
/// reference), so the Sui action is gated on proven EVM identity.
entry fun claim_member_badge(pass: &EvmIdentityPass, ctx: &mut TxContext) {
    let sender = ctx.sender();

    event::emit(MemberBadgeClaimed { eth_address: pass.eth_address, sui_owner: sender });

    transfer::transfer(
        EvmMemberBadge {
            id: object::new(ctx),
            eth_address: pass.eth_address,
        },
        sender,
    );
}

// === Message construction ===

fun bind_preimage(sui_addr: address): vector<u8> {
    let mut statement = STATEMENT_PREFIX;
    statement.append(evm_crypto::to_hex_string(&address::to_bytes(sui_addr)));
    personal_sign_preimage(statement)
}

/// Wrap `message` in the EIP-191 envelope:
/// `0x19 || "Ethereum Signed Message:\n" || ascii(len) || message`.
fun personal_sign_preimage(message: vector<u8>): vector<u8> {
    let mut out = vector<u8>[0x19];
    out.append(b"Ethereum Signed Message:\n");
    out.append(u64_to_ascii(message.length()));
    out.append(message);
    out
}

/// Decimal ASCII encoding of `n`.
fun u64_to_ascii(mut n: u64): vector<u8> {
    if (n == 0) {
        return b"0"
    };
    let mut digits = vector<u8>[];
    while (n > 0) {
        digits.push_back(48 + ((n % 10) as u8));
        n = n / 10;
    };
    digits.reverse();
    digits
}

// === Getters ===

public fun eth_address(pass: &EvmIdentityPass): vector<u8> {
    pass.eth_address
}

public fun sui_owner(pass: &EvmIdentityPass): address {
    pass.sui_owner
}

public fun badge_eth_address(badge: &EvmMemberBadge): vector<u8> {
    badge.eth_address
}

public fun is_bound(registry: &EvmBindingRegistry, eth_address: vector<u8>): bool {
    registry.bound.contains(eth_address)
}

public fun owner_of(registry: &EvmBindingRegistry, eth_address: vector<u8>): address {
    *registry.bound.borrow(eth_address)
}

// === Test-only ===

#[test_only]
public fun bind_preimage_for_test(sui_addr: address): vector<u8> {
    bind_preimage(sui_addr)
}
