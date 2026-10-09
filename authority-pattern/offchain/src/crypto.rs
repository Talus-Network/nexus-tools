use {
    base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _},
    chacha20poly1305::{
        aead::{Aead, Payload},
        KeyInit,
        XChaCha20Poly1305,
        XNonce,
    },
    hkdf::Hkdf,
    rand::{rngs::OsRng, RngCore},
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    sha2::Sha256,
    std::sync::Arc,
    thiserror::Error,
    x25519_dalek::{PublicKey, StaticSecret},
    zeroize::Zeroizing,
};

const EXPORT_AAD: &[u8] = b"talus-agent-api-key-export-v1";
const STORAGE_VERSION: u8 = 1;
const EXPORT_VERSION: u8 = 1;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("master key must be exactly 32 bytes of hexadecimal")]
    InvalidMasterKey,
    #[error("encryption operation failed")]
    Encryption,
    #[error("stored provider key is invalid or cannot be decrypted")]
    Decryption,
    #[error("approved owner encryption public key must be exactly 32 bytes")]
    InvalidRecipientKey,
    #[error("owner public key is invalid")]
    InvalidDhKey,
    #[error("key envelope is malformed or cannot be decrypted")]
    InvalidEnvelope,
}

#[derive(Clone)]
pub struct MasterKey(Arc<Zeroizing<[u8; 32]>>);

impl MasterKey {
    pub fn from_hex(value: &str) -> Result<Self, CryptoError> {
        let decoded = hex::decode(value).map_err(|_| CryptoError::InvalidMasterKey)?;
        let key: [u8; 32] = decoded
            .try_into()
            .map_err(|_| CryptoError::InvalidMasterKey)?;
        Ok(Self(Arc::new(Zeroizing::new(key))))
    }

    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyEnvelope {
    pub version: u8,
    pub suite: String,
    pub ephemeral_public_key: String,
    pub nonce: String,
    pub ciphertext: String,
}

pub fn seal_at_rest(
    master_key: &MasterKey,
    binding_id: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new_from_slice(master_key.bytes())
        .map_err(|_| CryptoError::Encryption)?;
    let mut nonce_bytes = [0_u8; 24];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);
    let aad = format!("talus-agent-api-provider-key-v1:{binding_id}");
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| CryptoError::Encryption)?;
    let mut result = Vec::with_capacity(1 + nonce_bytes.len() + ciphertext.len());
    result.push(STORAGE_VERSION);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

pub fn open_at_rest(
    master_key: &MasterKey,
    binding_id: &str,
    sealed: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() < 1 + 24 + 16 || sealed[0] != STORAGE_VERSION {
        return Err(CryptoError::Decryption);
    }
    let cipher = XChaCha20Poly1305::new_from_slice(master_key.bytes())
        .map_err(|_| CryptoError::Decryption)?;
    let nonce_bytes: [u8; 24] = sealed[1..25]
        .try_into()
        .map_err(|_| CryptoError::Decryption)?;
    let nonce = XNonce::from(nonce_bytes);
    let aad = format!("talus-agent-api-provider-key-v1:{binding_id}");
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &sealed[25..],
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| CryptoError::Decryption)
}

pub fn encrypt_for_owner(public_key: &[u8], plaintext: &[u8]) -> Result<KeyEnvelope, CryptoError> {
    let recipient_bytes: [u8; 32] = public_key
        .try_into()
        .map_err(|_| CryptoError::InvalidRecipientKey)?;
    let recipient = PublicKey::from(recipient_bytes);
    let ephemeral_secret = StaticSecret::random_from_rng(OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);
    let shared = ephemeral_secret.diffie_hellman(&recipient);
    if shared.as_bytes().iter().all(|byte| *byte == 0) {
        return Err(CryptoError::InvalidDhKey);
    }
    let mut derived = [0_u8; 32];
    let hkdf = Hkdf::<Sha256>::new(Some(EXPORT_AAD), shared.as_bytes());
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(ephemeral_public.as_bytes());
    info.extend_from_slice(recipient.as_bytes());
    hkdf.expand(&info, &mut derived)
        .map_err(|_| CryptoError::Encryption)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(&derived).map_err(|_| CryptoError::Encryption)?;
    let mut nonce_bytes = [0_u8; 24];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);
    let mut aad = Vec::with_capacity(EXPORT_AAD.len() + 64);
    aad.extend_from_slice(EXPORT_AAD);
    aad.extend_from_slice(ephemeral_public.as_bytes());
    aad.extend_from_slice(recipient.as_bytes());
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Encryption)?;
    Ok(KeyEnvelope {
        version: EXPORT_VERSION,
        suite: "X25519-HKDF-SHA256-XChaCha20Poly1305".to_owned(),
        ephemeral_public_key: URL_SAFE_NO_PAD.encode(ephemeral_public.as_bytes()),
        nonce: URL_SAFE_NO_PAD.encode(nonce_bytes),
        ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
    })
}

pub fn decrypt_export(
    private_key: &[u8; 32],
    envelope: &KeyEnvelope,
) -> Result<Vec<u8>, CryptoError> {
    if envelope.version != EXPORT_VERSION
        || envelope.suite != "X25519-HKDF-SHA256-XChaCha20Poly1305"
    {
        return Err(CryptoError::InvalidEnvelope);
    }
    let ephemeral_bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&envelope.ephemeral_public_key)
        .map_err(|_| CryptoError::InvalidEnvelope)?
        .try_into()
        .map_err(|_| CryptoError::InvalidEnvelope)?;
    let nonce: [u8; 24] = URL_SAFE_NO_PAD
        .decode(&envelope.nonce)
        .map_err(|_| CryptoError::InvalidEnvelope)?
        .try_into()
        .map_err(|_| CryptoError::InvalidEnvelope)?;
    let nonce = XNonce::from(nonce);
    let ciphertext = URL_SAFE_NO_PAD
        .decode(&envelope.ciphertext)
        .map_err(|_| CryptoError::InvalidEnvelope)?;
    let recipient_secret = StaticSecret::from(*private_key);
    let recipient = PublicKey::from(&recipient_secret);
    let ephemeral = PublicKey::from(ephemeral_bytes);
    let shared = recipient_secret.diffie_hellman(&ephemeral);
    if shared.as_bytes().iter().all(|byte| *byte == 0) {
        return Err(CryptoError::InvalidEnvelope);
    }
    let mut derived = [0_u8; 32];
    let hkdf = Hkdf::<Sha256>::new(Some(EXPORT_AAD), shared.as_bytes());
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(ephemeral.as_bytes());
    info.extend_from_slice(recipient.as_bytes());
    hkdf.expand(&info, &mut derived)
        .map_err(|_| CryptoError::InvalidEnvelope)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(&derived).map_err(|_| CryptoError::InvalidEnvelope)?;
    let mut aad = Vec::with_capacity(EXPORT_AAD.len() + 64);
    aad.extend_from_slice(EXPORT_AAD);
    aad.extend_from_slice(ephemeral.as_bytes());
    aad.extend_from_slice(recipient.as_bytes());
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::InvalidEnvelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_ciphertext_is_bound_to_the_binding_and_master_key() {
        let master = MasterKey::from_hex(&"12".repeat(32)).unwrap();
        let other_master = MasterKey::from_hex(&"34".repeat(32)).unwrap();
        let sealed = seal_at_rest(&master, "binding-1", b"provider-secret").unwrap();
        assert_eq!(
            open_at_rest(&master, "binding-1", &sealed).unwrap(),
            b"provider-secret"
        );
        assert!(open_at_rest(&master, "binding-2", &sealed).is_err());
        assert!(open_at_rest(&other_master, "binding-1", &sealed).is_err());
    }

    #[test]
    fn owner_envelope_decrypts_only_for_the_approved_private_key() {
        let owner_private = [7_u8; 32];
        let owner_public = PublicKey::from(&StaticSecret::from(owner_private));
        let other_private = [8_u8; 32];
        let envelope = encrypt_for_owner(owner_public.as_bytes(), b"provider-secret").unwrap();
        assert_eq!(
            decrypt_export(&owner_private, &envelope).unwrap(),
            b"provider-secret"
        );
        assert!(decrypt_export(&other_private, &envelope).is_err());
        assert!(encrypt_for_owner(&[0_u8; 31], b"secret").is_err());
    }
}
