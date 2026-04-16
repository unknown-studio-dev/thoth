//! The sync engine — glue between ingestors and the snapshot store.
//!
//! Responsibilities:
//!
//! 1. Call the ingestor's `list` with the right filter.
//! 2. Run redaction on every returned rule (adapter should also run it,
//!    but this is the defense-in-depth pass).
//! 3. Let the ingestor decide the target context via `map_to_context`.
//! 4. Upsert each accepted rule into the snapshot store.
//! 5. Produce a [`SyncReport`] the CLI can render.

use std::sync::Arc;

use crate::error::{DomainError, Result};
use crate::ingest::{DomainIngestor, IngestFilter};
use crate::redact;
use crate::snapshot::{SnapshotStore, SnapshotWrite};

/// Per-source outcome counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Snapshots newly created on disk.
    pub created: usize,
    /// Snapshots overwritten due to content hash drift.
    pub updated: usize,
    /// Snapshots skipped because content hash matched.
    pub unchanged: usize,
    /// Rules skipped because `map_to_context` returned `None`.
    pub unmapped: usize,
    /// Rules blocked by redaction.
    pub redacted: usize,
}

/// Full report returned to the CLI. Includes per-rule errors so users
/// can see which rules failed without aborting the whole sync.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// Source identifier (from `DomainIngestor::source_id`).
    pub source: String,
    /// Aggregate counts.
    pub stats: SyncStats,
    /// Individual errors, tagged by rule id. Non-fatal.
    pub errors: Vec<(String, String)>,
}

/// Run one sync pass for a single source.
///
/// Errors for *individual rules* (malformed data, redaction, write
/// failure) are collected into the report rather than aborting. Errors
/// from the ingestor itself (auth failure, rate limit) abort with `Err`.
pub async fn sync_source(
    ingestor: Arc<dyn DomainIngestor>,
    snapshots: &SnapshotStore,
    filter: &IngestFilter,
) -> Result<SyncReport> {
    let source = ingestor.source_id().to_string();
    let mut report = SyncReport {
        source: source.clone(),
        ..Default::default()
    };

    let rules = ingestor.list(filter).await.map_err(|e| {
        tracing::error!(?e, source = %source, "ingestor.list failed");
        e
    })?;

    for mut rule in rules {
        // Context mapping. Adapters can override `map_to_context`.
        let ctx = match ingestor.map_to_context(&rule) {
            Some(c) if !c.is_empty() => c,
            _ => {
                report.stats.unmapped += 1;
                continue;
            }
        };
        rule.context = ctx;

        // Defense-in-depth redaction pass.
        if let Err(e) = redact::scan(&rule) {
            report.stats.redacted += 1;
            report.errors.push((rule.id.clone(), e.to_string()));
            tracing::warn!(id = %rule.id, "rule redacted: {e}");
            continue;
        }

        match snapshots.upsert(&rule).await {
            Ok(SnapshotWrite::Created) => report.stats.created += 1,
            Ok(SnapshotWrite::Updated) => report.stats.updated += 1,
            Ok(SnapshotWrite::Unchanged) => report.stats.unchanged += 1,
            Err(e) => {
                report.errors.push((rule.id.clone(), e.to_string()));
                tracing::error!(id = %rule.id, "snapshot upsert failed: {e}");
            }
        }
    }

    Ok(report)
}

impl std::fmt::Display for SyncReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = &self.stats;
        writeln!(
            f,
            "[{}] created={} updated={} unchanged={} unmapped={} redacted={}",
            self.source, s.created, s.updated, s.unchanged, s.unmapped, s.redacted
        )?;
        for (id, e) in &self.errors {
            writeln!(f, "  error {id}: {e}")?;
        }
        Ok(())
    }
}

/// Helper for CLI wiring: turn a [`DomainError`] into a displayable line.
pub fn format_err(e: &DomainError) -> String {
    e.to_string()
}
