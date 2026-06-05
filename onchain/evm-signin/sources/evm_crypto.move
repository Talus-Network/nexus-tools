/// Shared Ethereum secp256k1 signature recovery for the `evm_signin` package.
///
/// Isolated so `evm_signin` (Nexus tool) and `evm_access` (identity binding)
/// can both use the same crypto without a circular module dependency.
module evm_signin::evm_crypto;

use sui::ecdsa_k1;
use sui::hash;

/// Hash-function selector for `secp256k1_ecrecover`: 0 = Keccak256 (Ethereum).
const KECCAK256: u8 = 0;

/// Recover the 20-byte Ethereum address that signed `message` with `signature`.
///
/// Aborts on a cryptographically invalid (but correctly sized) signature; guard
/// `signature.length() == 65` first.
public fun recover_address(message: vector<u8>, signature: vector<u8>): vector<u8> {
    recover_eth_address(&message, signature)
}

/// Encodes bytes as a `0x`-prefixed lowercase hex ASCII string.
public fun to_hex_string(bytes: &vector<u8>): vector<u8> {
    let hex_chars = b"0123456789abcdef";
    let mut out = b"0x";
    let mut i = 0;
    let n = bytes.length();
    while (i < n) {
        let byte = bytes[i];
        out.push_back(hex_chars[((byte >> 4) as u64)]);
        out.push_back(hex_chars[((byte & 0x0f) as u64)]);
        i = i + 1;
    };
    out
}

/// Recover the 20-byte Ethereum address that signed `message`.
fun recover_eth_address(message: &vector<u8>, mut signature: vector<u8>): vector<u8> {
    let v = signature[64];
    if (v >= 27) {
        *signature.borrow_mut(64) = v - 27;
    };

    let compressed = ecdsa_k1::secp256k1_ecrecover(&signature, message, KECCAK256);
    let uncompressed = ecdsa_k1::decompress_pubkey(&compressed);

    let pubkey_xy = slice(&uncompressed, 1, 65);
    let hashed = hash::keccak256(&pubkey_xy);

    slice(&hashed, 12, 32)
}

fun slice(bytes: &vector<u8>, start: u64, end: u64): vector<u8> {
    let mut out = vector<u8>[];
    let mut i = start;
    while (i < end) {
        out.push_back(bytes[i]);
        i = i + 1;
    };
    out
}
