//! The [`DomainIngestor`] trait — pluggable source adapters.
//!
//! Shape mirrors the existing [`thoth_core::Embedder`] /
//! [`thoth_core::Synthesizer`] traits for consistency. Adapters are
//! feature-gated so the base crate pulls in no HTTP client unless a
//! caller asks for one.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::Result;
use crate::types::RemoteRule;

/// Incremental-pull hint passed to [`DomainIngestor::list`].
///
/// When `Some`, adapters should return only rules modified at or after the
/// given instant. When `None`, adapters return their full (filtered) set.
/// Adapters that cannot filter by timestamp return everything and let the
/// sync engine dedupe by content hash.
#[derive(Debug, Clone, Copy)]
pub struct IngestFilter {
    /// Only return rules updated at or after this time.
    pub since: Option<OffsetDateTime>,
    /// Hard cap on the number of rules returned in one call.
    ///
    /// Adapters should paginate internally and stop at this cap;
    /// unbounded pulls trigger rate-limit pain on large task managers.
    pub max_items: usize,
}

impl Default for IngestFilter {
    fn default() -> Self {
        Self {
            since: None,
            max_items: 500,
        }
    }
}

/// A pluggable source of [`RemoteRule`]s.
///
/// Implementations are responsible for:
///
/// 1. **Authentication** — typically via env vars read in the constructor.
///    Never read credentials mid-call.
/// 2. **Filtering** — honour `filter.since` and `filter.max_items`.
/// 3. **Context mapping** — `map_to_context` decides which bounded
///    context each rule belongs to. Return `None` to skip a rule.
/// 4. **Redaction** — run [`crate::redact`] on every rule before returning.
///    The trait does not enforce this, but the sync engine will run a
///    final pass regardless.
#[async_trait]
pub trait DomainIngestor: Send + Sync {
    /// Stable identifier for this source — used in snapshot paths and
    /// frontmatter. Lowercase, no spaces (e.g. "notion", "asana").
    fn source_id(&self) -> &str;

    /// Pull rules from the remote side, honoring `filter`.
    async fn list(&self, filter: &IngestFilter) -> Result<Vec<RemoteRule>>;

    /// Map a rule to a bounded context. Return `None` to skip a rule
    /// that doesn't belong to any known context.
    ///
    /// Default: trust the context already set by `list`. Adapters that
    /// classify differently (label conventions, database properties)
    /// should override this.
    fn map_to_context(&self, rule: &RemoteRule) -> Option<String> {
        if rule.context.is_empty() {
            None
        } else {
            Some(rule.context.clone())
        }
    }
}
