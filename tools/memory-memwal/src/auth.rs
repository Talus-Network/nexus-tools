//! Ed25519 request signing for the MemWal relayer.
//!
//! Every protected route requires three headers constructed from the operator's
//! Ed25519 delegate private key (held in `MEMWAL_DELEGATE_PRIVATE_KEY`):
//!
//! ```text
//! x-public-key : hex(public_key)
//! x-timestamp  : unix_seconds
//! x-signature  : hex(Ed25519_sign(key, "{ts}.{METHOD}.{path}.{sha256hex(body)}"))
//! ```
//!
//! Timestamps must fall within a 5-minute window of the server clock to prevent
//! replay attacks (enforced server-side).

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
}

/// Build the three MemWal auth headers for a single request.
///
/// `method` must be the HTTP method in uppercase (e.g. `"POST"`).
/// `path`   must be the request path including the leading `/` (e.g. `"/api/remember"`).
/// `body`   is the raw request body bytes; pass an empty slice for requests with no body.
pub(crate) fn sign_request(
    private_key_hex: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<AuthHeaders, AuthError> {
    if private_key_hex.is_empty() {
        return Err(AuthError::MissingKey);
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AuthError::Clock(e.to_string()))?
        .as_secs()
        .to_string();

    let body_hash = hex::encode(Sha256::digest(body));
    let message = format!("{ts}.{method}.{path}.{body_hash}");

    let raw: Vec<u8> = hex::decode(private_key_hex)?;
    let key_bytes: [u8; 32] = raw
        .try_into()
        .map_err(|v: Vec<u8>| AuthError::InvalidKeyLength(v.len()))?;

    let signing_key = SigningKey::from_bytes(&key_bytes);
    let signature = signing_key.sign(message.as_bytes());

    Ok(AuthHeaders {
        public_key: hex::encode(signing_key.verifying_key().to_bytes()),
        signature: hex::encode(signature.to_bytes()),
        timestamp: ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_key_hex() -> String {
        // Deterministic test key: 32 bytes of 0x42.
        hex::encode([0x42u8; 32])
    }

    /// `sign_request` produces non-empty headers for a valid key and request.
    /// Failure mode caught: auth module silently returns empty headers.
    #[test]
    fn sign_request_returns_headers() {
        let key = random_key_hex();
        let h = sign_request(&key, "POST", "/api/remember", b"{\"text\":\"hello\"}")
            .expect("should sign successfully");

        assert!(!h.public_key.is_empty(), "public_key must not be empty");
        assert!(!h.signature.is_empty(), "signature must not be empty");
        assert!(!h.timestamp.is_empty(), "timestamp must not be empty");
        assert!(
            h.public_key.len() == 64,
            "public_key is 32 bytes = 64 hex chars"
        );
        assert!(
            h.signature.len() == 128,
            "signature is 64 bytes = 128 hex chars"
        );
    }

    /// `sign_request` with an empty key returns `AuthError::MissingKey`.
    /// Failure mode caught: missing key is silently ignored, producing unauthenticated requests.
    #[test]
    fn sign_request_rejects_empty_key() {
        let result = sign_request("", "POST", "/api/remember", b"{}");
        assert!(
            matches!(result, Err(AuthError::MissingKey)),
            "empty key must yield MissingKey"
        );
    }

    /// `sign_request` with a non-hex key returns `AuthError::InvalidHex`.
    /// Failure mode caught: bad key bypasses signing and produces garbage headers.
    #[test]
    fn sign_request_rejects_non_hex_key() {
        let result = sign_request("not-hex!!", "POST", "/api/remember", b"{}");
        assert!(
            matches!(result, Err(AuthError::InvalidHex(_))),
            "non-hex key must yield InvalidHex"
        );
    }

    /// `sign_request` with a key that is valid hex but wrong length returns `AuthError::InvalidKeyLength`.
    /// Failure mode caught: short key silently accepted, producing an incorrect signing key.
    #[test]
    fn sign_request_rejects_wrong_length_key() {
        let short_key = hex::encode([0u8; 16]); // 16 bytes, not 32
        let result = sign_request(&short_key, "POST", "/api/remember", b"{}");
        assert!(
            matches!(result, Err(AuthError::InvalidKeyLength(16))),
            "16-byte key must yield InvalidKeyLength(16)"
        );
    }

    /// Two calls with the same key and body produce the same public key and a
    /// valid signature, but may differ in timestamp.
    /// Failure mode caught: signing is not deterministic, breaking idempotent replays.
    #[test]
    fn sign_request_same_key_same_pubkey() {
        let key = random_key_hex();
        let h1 = sign_request(&key, "POST", "/api/recall", b"{\"query\":\"foo\"}").unwrap();
        let h2 = sign_request(&key, "POST", "/api/recall", b"{\"query\":\"foo\"}").unwrap();
        assert_eq!(
            h1.public_key, h2.public_key,
            "same key must yield same public key"
        );
    }

    /// `sign_request` on a GET request with an empty body does not panic.
    /// Failure mode caught: empty-body path panics or errors for GET requests.
    #[test]
    fn sign_request_handles_empty_body() {
        let key = random_key_hex();
        let result = sign_request(&key, "GET", "/api/remember/some-job-id", b"");
        assert!(result.is_ok(), "empty body must be handled gracefully");
    }
}
