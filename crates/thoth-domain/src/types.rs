//! Core data types for Domain memory.
//!
//! A [`RemoteRule`] is what an ingestor produces; the snapshot writer then
//! serializes it into markdown + TOML frontmatter under
//! `<root>/domain/<context>/_remote/<source>/<id>.md`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Classification of a business rule.
///
/// This mirrors the sections a well-curated `DOMAIN.md` tends to have.
/// Keep this small — adding a variant means every adapter must know how
/// to route into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// A hard constraint the system must enforce
    /// (e.g. "refund > $500 requires manager approval").
    Invariant,
    /// A multi-step process
    /// (e.g. "order → payment → fulfillment → invoice").
    Workflow,
    /// A domain term with its ubiquitous-language definition
    /// (e.g. "Order = a committed purchase, pre-fulfillment").
    Glossary,
    /// A policy / business decision that may change
    /// (e.g. "free shipping over $100, EU only, expires 2026-06-01").
    Policy,
}

impl RuleKind {
    /// Lowercase stable identifier used in frontmatter.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleKind::Invariant => "invariant",
            RuleKind::Workflow => "workflow",
            RuleKind::Glossary => "glossary",
            RuleKind::Policy => "policy",
        }
    }
}

/// Review status of a rule. Only `Accepted` is served to the agent in
/// Mode::Zero; `Proposed` is shown with a warning flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    /// Just ingested — awaiting human review in a PR.
    #[default]
    Proposed,
    /// Reviewed and merged. Served by retrieval.
    Accepted,
    /// No longer authoritative. Served with a deprecated flag.
    Deprecated,
}

/// A rule produced by a [`crate::DomainIngestor`] — pre-snapshot.
///
/// The `source_hash` is computed over `title + body + kind` so drift
/// detection is stable even if the upstream renumbers ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRule {
    /// Stable id **within** the source — e.g. "PROJ-1234" for Jira,
    /// the Notion page id for Notion, the Asana task gid for Asana.
    pub id: String,
    /// Source adapter identifier, e.g. "notion", "asana", "file".
    pub source: String,
    /// URI on the source system — clickable, displayable to humans.
    pub source_uri: String,
    /// Target bounded context. Ingestor's `map_to_context` decides this.
    pub context: String,
    /// What kind of rule.
    pub kind: RuleKind,
    /// Short human-readable title (one line).
    pub title: String,
    /// Full body in markdown.
    pub body: String,
    /// When the remote side last modified this rule.
    pub updated_at: OffsetDateTime,
    /// Free-form tags (e.g. "compliance", "finance") preserved from source.
    pub tags: Vec<String>,
}

impl RemoteRule {
    /// Compute a blake3 hash over the content that matters for drift
    /// detection. Excludes `updated_at` on purpose — a source system
    /// bumping timestamps without changing content should not flag drift.
    pub fn content_hash(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.title.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.body.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.kind.as_str().as_bytes());
        let hash = hasher.finalize();
        format!("blake3:{}", hash.to_hex())
    }

    /// Filename on disk: `<id>.md` with unsafe chars replaced.
    /// Ids can contain slashes (Notion page paths), so we normalize.
    pub fn safe_filename(&self) -> String {
        let stem: String = self
            .id
            .chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                _ => c,
            })
            .collect();
        format!("{stem}.md")
    }
}

/// TOML frontmatter embedded at the top of each snapshot file.
///
/// Keep field names stable — they are part of the on-disk contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Frontmatter {
    /// Stable id within the source.
    pub id: String,
    /// Source adapter identifier.
    pub source: String,
    /// URI on the source system.
    pub source_uri: String,
    /// blake3 content hash — `blake3:<hex>`.
    pub source_hash: String,
    /// Bounded context this rule belongs to.
    pub context: String,
    /// Rule kind.
    pub kind: RuleKind,
    /// Review status.
    pub status: RuleStatus,
    /// When Thoth last pulled this snapshot (RFC3339).
    #[serde(with = "time::serde::rfc3339")]
    pub last_synced: OffsetDateTime,
    /// When the remote side last changed (RFC3339).
    #[serde(with = "time::serde::rfc3339")]
    pub remote_updated_at: OffsetDateTime,
    /// Preserved tags.
    #[serde(default)]
    pub tags: Vec<String>,
}
