//! Error types for the update module.
//!
//! All public functions return `Result<T, UpdateError>` — no unwrap/panic.
//! Errors are logged via `tracing` and never propagate to the main loop.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("manifest parse error: {0}")]
    ManifestParse(String),

    #[error("platform not supported: {os}/{arch}")]
    UnsupportedPlatform { os: String, arch: String },

    #[error("semver parse error: {0}")]
    Semver(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("archive error: {0}")]
    Archive(String),
}
