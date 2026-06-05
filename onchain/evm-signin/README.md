# Onchain tool: `xyz.taluslabs.evm.verify-signature@1`

Sui Move tool that verifies an **Ethereum (secp256k1) signature onchain** and
recovers the signer's `0x` address — using Sui's native
`ecdsa_k1::secp256k1_ecrecover` with Keccak256, exactly like Ethereum's
`ecrecover` precompile.

## Why this matters

The #1 friction for EVM users coming to Sui is "install a new wallet, learn a
new seed phrase." This tool removes it: an EVM user signs a message with their
existing wallet (MetaMask `personal_sign` / EIP-712), and a Sui Move contract
verifies it and derives their Ethereum address. That address can then authorize
a Sui action — gated mints, DAO actions, claims, or binding an `0x` identity to a
Sui object — **without the user ever leaving their Ethereum wallet**.

It is the identity half of using Sui + Nexus as a verifiable coordination layer
for EVM.

## Package layout

Three Move modules in one package:

- `evm_crypto` — secp256k1 `ecrecover` + Keccak256 → Ethereum address recovery.
  Shared, dependency-free core.
- `evm_signin` — the Nexus onchain tool (`verify-signature`). Verifies a
  signature and returns the recovered address as tool output.
- `evm_access` — identity binding & access control: `bind` (mint
  `EvmIdentityPass`), `assert_controls` (reusable gate), and
  `claim_member_badge` (example gated action). Emits events for indexers.

End-to-end flow:

```text
MetaMask sign → evm_signin::execute (verify, in a workflow)
              ↘ evm_access::bind (0x ↔ Sui, mint pass, emit event)
                          ↘ assert_controls / claim_member_badge (gated Sui action)
```

## Build

```bash
cd onchain/evm-signin
sui move build
```

Requires `nexus-next` as a sibling of `nexus-tools` (see `Move.toml` local paths).

## Test

```bash
cd onchain/evm-signin
sui move test
```

The test vector is a real `personal_sign` signature generated with foundry
`cast` (Anvil account #0), so the test proves the full Ethereum recovery path
end to end.

## Publish

```bash
# Ensure the Sui CLI active env is testnet
sui client publish --gas-budget 100000000
```

Note the **package ID** and the **witness object ID** (the `EvmSigninWitness`
inside `EvmSigninState`, retrievable via `witness_id`).

On publish, `evm_signin::init` creates **both** shared objects:

- `EvmSigninState` — Nexus tool state + witness (register this tool against it).
- `EvmBindingRegistry` — identity-binding registry for `evm_access::bind`.

## Register onchain

```bash
nexus tool register onchain \
  --package 0xYOUR_PACKAGE_ID \
  --module evm_signin \
  --tool-fqn xyz.taluslabs.evm.verify-signature@1 \
  --description "Verify an Ethereum secp256k1 signature onchain and recover the signer 0x address" \
  --witness-id 0xYOUR_WITNESS_OBJECT_ID \
  --timeout 10s
```

Canonical schemas: `schemas/input.json`, `schemas/output.json`.

## Input

| Port | Move param | Type | Description |
|------|------------|------|-------------|
| `0` | `state` | Shared `EvmSigninState` (mutable) | Tool state + witness |
| `1` | `message` | `vector<u8>` | Exact signed bytes (EIP-191/EIP-712 preimage); Keccak256-hashed internally |
| `2` | `signature` | `vector<u8>` | 65-byte `(r, s, v)` secp256k1 signature; `v` may be 27/28 or 0/1 |
| `3` | `expected_address` | `vector<u8>` | Optional 20-byte address to check against; empty to skip |

## Output

Success:

```json
{ "ok": { "recovered_address": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266", "matches": true } }
```

Error (signature not 65 bytes):

```json
{ "err": { "reason": "Signature must be 65 bytes (r, s, v)" } }
```

A cryptographically invalid (but correctly sized) signature aborts the
transaction via the native `ecrecover`, rather than returning `err`.

## Companion module: identity binding & access (`evm_access`)

`evm_crypto::recover_address` is reusable, so the same package ships
`evm_access`, which turns a verified signature into real Sui actions — the
flagship "EVM user controls Sui with MetaMask" flow.

### Objects

- `EvmBindingRegistry` — shared object mapping each `0x` address to the Sui
  account bound to it.
- `EvmIdentityPass` — soulbound (`key`-only, no transfer) proof that a Sui
  account controls an `0x` address.
- `EvmMemberBadge` — soulbound badge minted by the gated example action.

### Functions

- `bind(registry, signature, ctx)` — recovers the signer of the canonical
  statement for `ctx.sender()`, records the binding, emits `EvmAddressBound`,
  and mints the caller an `EvmIdentityPass`.
- `assert_controls(pass, eth_address)` — reusable gate: aborts unless the pass
  proves control of `eth_address`. Any dApp can call this to require "only the
  holder of this Ethereum address may proceed."
- `claim_member_badge(pass, ctx)` — example gated action: requires the
  `EvmIdentityPass` (a soulbound object only its owner can pass by reference),
  emits `MemberBadgeClaimed`, and mints an `EvmMemberBadge`. Demonstrates the
  full MetaMask → Sui loop.

### Events

- `EvmAddressBound { eth_address, sui_owner }`
- `MemberBadgeClaimed { eth_address, sui_owner }`

**Replay safety.** The caller does not pass the message. The contract
reconstructs the exact statement from `ctx.sender()`:

```text
Bind my Ethereum address to Sui account: 0x<32-byte-sui-address-hex>
```

A signature made to bind account A cannot be replayed under account B, because
the reconstructed message — and therefore the recovered address — differs. The
client must sign this exact statement (via `personal_sign`).

This is the building block for gated mints, DAO membership, and airdrop claims
keyed on a user's existing Ethereum identity, with no new wallet.

### Client: sign the bind statement

The user must sign this exact string with MetaMask (`personal_sign`):

```text
Bind my Ethereum address to Sui account: 0x<32-byte-sui-address-hex>
```

Example for Sui address `@0xA11CE` (padded to 32 bytes):

```bash
cast wallet sign --private-key $PK \
  "Bind my Ethereum address to Sui account: 0x00000000000000000000000000000000000000000000000000000000000a11ce"
```

Then call `bind` with the 65-byte signature (hex-decoded) as the second argument.

## Generating a signature (verify tool)

Any EVM wallet/library produces a compatible signature. With foundry `cast`:

```bash
cast wallet sign --private-key $PK "Login to Talus Nexus"
```

The `message` port must be the **EIP-191 preimage** that was hashed:
`0x19 || "Ethereum Signed Message:\n" || len(msg) || msg`. For EIP-712, pass the
full EIP-712 encoded preimage instead.

## Verify

```bash
nexus tool list
```
