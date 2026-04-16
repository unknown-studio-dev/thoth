//! Domain-specific error wrapper.
//!
//! All errors ultimately bubble up as [`thoth_core::Error`] to keep the
//! top-level error surface uniform; `DomainError` is a richer variant
//! for crate-internal callers.

use thoth_core::Error as CoreError;

/// Errors produced by ingestors, snapshot writers, and the sync engine.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// The external source rejected the request or returned malformed data.
    ///
    /// The field is named `source_id` (not `source`) so `thiserror`'s
    /// convention-based `#[source]` inference does not kick in — `String`
    /// is not a `std::error::Error`.
    #[error("source `{source_id}` failed: {message}")]
    Source {
        /// Stable source identifier (e.g. "notion", "asana").
        source_id: String,
        /// Human-readable error detail.
        message: String,
    },

    /// Required configuration (API key, base URL, database id, ...) missing.
    #[error("missing config for `{0}`: {1}")]
    MissingConfig(String, String),

    /// Snapshot write/read failure.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// Content was rejected by the redaction filter.
    #[error("redaction blocked ingest of `{0}`: {1}")]
    Redacted(String, String),

    /// Underlying IO.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// TOML frontmatter (de)serialization.
    #[error("toml: {0}")]
    Toml(String),

    /// JSON response parsing.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<toml::de::Error> for DomainError {
    fn from(e: toml::de::Error) -> Self {
        DomainError::Toml(e.to_string())
    }
}

impl From<toml::ser::Error> for DomainError {
    fn from(e: toml::ser::Error) -> Self {
        DomainError::Toml(e.to_string())
    }
}

impl From<DomainError> for CoreError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::Io(io) => CoreError::Io(io),
            DomainError::Json(j) => CoreError::Json(j),
            DomainError::Source { source_id, message } => {
                CoreError::Provider(format!("{source_id}: {message}"))
            }
            DomainError::MissingConfig(k, v) => CoreError::Config(format!("{k}: {v}")),
            DomainError::Snapshot(s) => CoreError::Store(s),
            DomainError::Redacted(id, reason) => {
                CoreError::Other(anyhow::anyhow!("redacted {id}: {reason}"))
            }
            DomainError::Toml(s) => CoreError::Config(s),
        }
    }
}

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, DomainError>;
