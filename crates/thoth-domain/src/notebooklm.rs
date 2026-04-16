//! NotebookLM ingestor — **stub**.
//!
//! NotebookLM does not expose a public REST API for reading notebook
//! contents at the time of writing. Two viable integration paths:
//!
//! 1. **MCP** — when the Thoth universal MCP adapter lands, point it
//!    at a NotebookLM MCP server and that path replaces this file.
//! 2. **Export workflow** — users export a notebook as markdown
//!    to a local folder, then ingest with [`crate::file::FileIngestor`]
//!    pointed at that folder.
//!
//! Until then, constructing this ingestor fails with
//! [`DomainError::MissingConfig`] so callers get a loud signal rather
//! than a silent no-op. The shape is preserved so the trait bound holds
//! once the MCP adapter ships.

use async_trait::async_trait;

use crate::error::{DomainError, Result};
use crate::ingest::{DomainIngestor, IngestFilter};
use crate::types::RemoteRule;

/// Placeholder NotebookLM ingestor. Always returns an error on
/// construction; retained so the feature flag compiles and the trait
/// surface stays honest.
pub struct NotebookLmIngestor {
    _priv: (),
}

impl NotebookLmIngestor {
    /// Attempt construction. Always errors until a real implementation
    /// (MCP-based) lands.
    pub fn new() -> Result<Self> {
        Err(DomainError::MissingConfig(
            "notebooklm".into(),
            "NotebookLM has no public read API. For now, export notebook \
             contents to markdown and use FileIngestor; a real adapter \
             will land once the universal MCP adapter ships (see ADR 0001)."
                .into(),
        ))
    }
}

#[async_trait]
impl DomainIngestor for NotebookLmIngestor {
    fn source_id(&self) -> &str {
        "notebooklm"
    }

    async fn list(&self, _filter: &IngestFilter) -> Result<Vec<RemoteRule>> {
        Err(DomainError::Source {
            source_id: "notebooklm".into(),
            message: "stub — see module docs for integration paths".into(),
        })
    }
}
