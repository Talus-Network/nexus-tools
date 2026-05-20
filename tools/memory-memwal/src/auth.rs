//! Ed25519 request signing for the MemWal relayer.
//!
//! Every protected route requires four headers constructed from the operator's
//! Ed25519 delegate private key (held in `MEMWAL_DELEGATE_PRIVATE_KEY`):
//!
//! ```text
//! x-public-key : hex(public_key)
//! x-timestamp  : unix_seconds
//! x-nonce      : UUID v4 (one per request — checked server-side against Redis
//!                for replay protection; 600 s TTL)
//! x-signature  : hex(Ed25519_sign(key,
//!                  "{ts}.{METHOD}.{path}.{sha256hex(body)}.{nonce}.{account_id}"))
//! ```
//!
//! `{account_id}` is the empty string when the optional `x-account-id` header
//! is not sent — the relayer derives the account from the public key on chain.
//!
//! Replay protection: the relayer rejects any timestamp outside a ±5-minute
//! window of its clock, and rejects any nonce it has seen in the last 10 min.
//! Together these mean we MUST mint a fresh UUID per request.
//!
//! Backwards compatibility: missing `x-nonce` returns HTTP 426 Upgrade Required
//! (the relayer's signal that the client is a legacy SDK that needs to upgrade).

use {
    crate::error::AuthError,
    ed25519_dalek::{Signer, SigningKey},
    sha2::{Digest, Sha256},
    std::time::{SystemTime, UNIX_EPOCH},
};

/// Headers produced by [`sign_request`].
pub(crate) struct AuthHeaders {
    pub(crate) public_key: String,
    pub(crate) signature: String,
    pub(crate) timestamp: String,
    pub(crate) nonce: String,
}

/// Parse a hex-encoded 32-byte Ed25519 secret into a `SigningKey` plus the hex
/// representation of its derived public key.
///
/// Done **once** at startup (or per `MemWalClient` construction) so signing
/// no longer pays for hex decoding + `SigningKey::from_bytes` (a SHA-512 over
/// the secret) + `verifying_key()` (Curve25519 scalar mult) per request.
pub(crate) fn parse_signing_key(private_key_hex: &str) -> Result<(SigningKey, String), AuthError> {
    if private_key_hex.is_empty() {
        return Err(AuthError::MissingKey);
    }
    let raw: Vec<u8> = hex::decode(private_key_hex)?;
    let key_bytes: [u8; 32] = raw
        .try_into()
        .map_err(|v: Vec<u8>| AuthError::InvalidKeyLength(v.len()))?;
    let signing_key = SigningKey::from_bytes(&key_bytes);
    let public_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
    Ok((signing_key, public_key_hex))
}

/// Build the canonical signed message exactly as the relayer's `auth.rs`
/// expects it. Extracted as a pure function so a unit test can lock the
/// format independently of the crypto.
///
/// See `services/server/src/auth.rs` in tag `@mysten-incubation/memwal@0.0.4`
/// for the server-side reference.
fn canonical_message(
    ts: &str,
    method: &str,
    path: &str,
    body_hash: &str,
    nonce: &str,
    account_id: &str,
) -> String {
    format!("{ts}.{method}.{path}.{body_hash}.{nonce}.{account_id}")
}

/// Build the MemWal auth headers for a single request.
///
/// `signing_key`    is the pre-parsed delegate key.
/// `public_key_hex` is the hex form of the derived public key (computed once
///                  by [`parse_signing_key`] at startup so signing doesn't
///                  redo Curve25519 scalar mult per call).
/// `method`         must be uppercase (e.g. `"POST"`).
/// `path`           must include the leading `/` (e.g. `"/api/remember"`).
/// `body`           is the raw request body bytes; pass an empty slice for
///                  requests with no body.
/// `account_id`     is the MemWal account object ID. Pass `""` when not
///                  configured — the server then resolves the account from
///                  the public key via an on-chain registry scan. The value
///                  passed here MUST match what the caller sends in the
///                  `x-account-id` header.
///
/// A fresh UUID v4 nonce is minted on every call — never re-use one or the
/// server will reject the request as a replay.
pub(crate) fn sign_request(
    signing_key: &SigningKey,
    public_key_hex: &str,
    method: &str,
    path: &str,
    body: &[u8],
    account_id: &str,
) -> Result<AuthHeaders, AuthError> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AuthError::Clock(e.to_string()))?
        .as_secs()
        .to_string();

    let nonce = uuid::Uuid::new_v4().to_string();
    let body_hash = hex::encode(Sha256::digest(body));

    let message = canonical_message(&ts, method, path, &body_hash, &nonce, account_id);
    let signature = signing_key.sign(message.as_bytes());

    Ok(AuthHeaders {
        public_key: public_key_hex.to_string(),
        signature: hex::encode(signature.to_bytes()),
        timestamp: ts,
        nonce,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> (SigningKey, String) {
        // Deterministic test key: 32 bytes of 0x42.
        parse_signing_key(&hex::encode([0x42u8; 32])).expect("test key must parse")
    }

    /// `canonical_message` matches the relayer's expected 6-segment dot-separated
    /// wire format byte-for-byte.
    /// Failure mode caught: any drift in the canonical-message format (separator
    /// swap, segment reorder, missing field) would pass every existing shape
    /// test and only fail with HTTP 401 against the live relayer.
    #[test]
    fn canonical_message_format_locked() {
        let m = canonical_message(
            "1700000000",
            "POST",
            "/api/remember",
            "abc123",
            "11111111-2222-3333-4444-555555555555",
            "0xacct",
        );
        assert_eq!(
            m,
            "1700000000.POST./api/remember.abc123.11111111-2222-3333-4444-555555555555.0xacct"
        );
    }

    /// `parse_signing_key` rejects an empty hex string with `MissingKey`.
    /// Failure mode caught: missing key is silently ignored, producing
    /// unauthenticated requests.
    #[test]
    fn parse_signing_key_rejects_empty() {
        assert!(matches!(parse_signing_key(""), Err(AuthError::MissingKey)));
    }

    /// `parse_signing_key` rejects non-hex with `InvalidHex`.
    /// Failure mode caught: bad key bypasses validation and produces garbage
    /// headers.
    #[test]
    fn parse_signing_key_rejects_non_hex() {
        assert!(matches!(
            parse_signing_key("not-hex!!"),
            Err(AuthError::InvalidHex(_))
        ));
    }

    /// `parse_signing_key` rejects an under-length key with `InvalidKeyLength`.
    /// Failure mode caught: a 16-byte key would silently produce a 32-byte
    /// signing key by zero-extension if the byte-length check were removed.
    #[test]
    fn parse_signing_key_rejects_short_key() {
        let short = hex::encode([0u8; 16]);
        assert!(matches!(
            parse_signing_key(&short),
            Err(AuthError::InvalidKeyLength(16))
        ));
    }

    /// `parse_signing_key` returns a public key whose hex form is 64 chars.
    /// Failure mode caught: public-key derivation drops bytes or returns an
    /// empty string, so all subsequent x-public-key headers are malformed.
    #[test]
    fn parse_signing_key_derives_public_key_hex() {
        let (_sk, pk_hex) = test_key();
        assert_eq!(pk_hex.len(), 64);
    }

    /// `sign_request` produces non-empty headers with the right lengths for a
    /// valid key and request.
    /// Failure mode caught: auth module silently returns empty headers.
    #[test]
    fn sign_request_returns_headers() {
        let (sk, pk_hex) = test_key();
        let h = sign_request(&sk, &pk_hex, "POST", "/api/remember", b"{\"text\":\"hello\"}", "")
            .expect("should sign successfully");

        assert_eq!(h.public_key, pk_hex);
        assert!(!h.signature.is_empty());
        assert!(!h.timestamp.is_empty());
        assert!(!h.nonce.is_empty());
        assert_eq!(h.public_key.len(), 64);
        assert_eq!(h.signature.len(), 128);
    }

    /// `sign_request` mints a fresh UUID-formatted nonce on every call.
    /// Failure mode caught: a static or empty nonce would trigger replay
    /// rejection (or HTTP 426) on the second invocation against the relayer.
    #[test]
    fn sign_request_nonce_is_unique_uuid_per_call() {
        let (sk, pk_hex) = test_key();
        let h1 = sign_request(&sk, &pk_hex, "POST", "/api/remember", b"{}", "").unwrap();
        let h2 = sign_request(&sk, &pk_hex, "POST", "/api/remember", b"{}", "").unwrap();
        assert_ne!(h1.nonce, h2.nonce);
        assert!(uuid::Uuid::parse_str(&h1.nonce).is_ok());
        assert!(uuid::Uuid::parse_str(&h2.nonce).is_ok());
    }

    /// Repeated signing with the same key returns the same public key.
    /// Failure mode caught: per-call public-key derivation produces a different
    /// public key, which would break attribution server-side.
    #[test]
    fn sign_request_same_key_same_pubkey() {
        let (sk, pk_hex) = test_key();
        let h1 = sign_request(&sk, &pk_hex, "POST", "/api/recall", b"{\"query\":\"foo\"}", "").unwrap();
        let h2 = sign_request(&sk, &pk_hex, "POST", "/api/recall", b"{\"query\":\"foo\"}", "").unwrap();
        assert_eq!(h1.public_key, h2.public_key);
    }

    /// `sign_request` with an empty body does not panic.
    /// Failure mode caught: empty-body path panics or errors for GET requests.
    #[test]
    fn sign_request_handles_empty_body() {
        let (sk, pk_hex) = test_key();
        let result = sign_request(&sk, &pk_hex, "GET", "/api/remember/some-job-id", b"", "");
        assert!(result.is_ok());
    }
}
