//! Shared error types for the MemWal tool crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum MemWalError {
    #[error("Auth error: {0}")]
    Auth(#[from] AuthError),

    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("Server returned an error: {0}")]
    Server(String),

    #[error("Job {0} failed on the server")]
    JobFailed(String),

    #[error("Timed out waiting for job {0} to complete")]
    Timeout(String),
}

#[derive(Debug, Error)]
pub(crate) enum AuthError {
    #[error("MEMWAL_DELEGATE_PRIVATE_KEY is not set or empty")]
    MissingKey,

    #[error("Private key is not valid hex: {0}")]
    InvalidHex(#[from] hex::FromHexError),

    #[error("Private key must be 32 bytes, got {0}")]
    InvalidKeyLength(usize),

    #[error("System clock error: {0}")]
    Clock(String),
}
