//! File-based ingestor.
//!
//! Reads a directory of TOML files as "remote rules" — this is the
//! reference implementation used by tests, air-gapped environments, and
//! bootstrapping workflows (dump Jira CSV → TOML files → ingest).
//!
//! Each file must contain the fields of [`RemoteRule`] as TOML. Example:
//!
//! ```toml
//! id = "R-001"
//! source_uri = "file:///specs/R-001.md"
//! context = "billing"
//! kind = "invariant"
//! title = "Refund limit $500"
//! body = "Any refund above $500 requires manager approval."
//! updated_at = "2026-04-16T08:00:00Z"      # quoted RFC3339 string
//! tags = ["compliance"]
//! ```
//!
//! `updated_at` must be a **quoted** RFC3339 string — not a native TOML
//! datetime literal — because the deserializer uses `time::serde::rfc3339`,
//! which operates on strings (and this matches our snapshot frontmatter
//! format exactly, so round-tripping just works).
//!
//! Note: `source` is always set to this ingestor's `source_id` ("file"),
//! even if the TOML specifies something else. This keeps snapshot paths
//! predictable.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::fs;

use crate::error::{DomainError, Result};
use crate::ingest::{DomainIngestor, IngestFilter};
use crate::types::{RemoteRule, RuleKind};

/// File-based ingestor. See module docs for the TOML schema.
#[derive(Debug, Clone)]
pub struct FileIngestor {
    /// Directory the ingestor reads from. Scanned recursively.
    root: PathBuf,
    /// Identifier reported as [`DomainIngestor::source_id`].
    source: String,
}

impl FileIngestor {
    /// Create a file ingestor rooted at `dir`. Stable identifier "file".
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            root: dir.as_ref().to_path_buf(),
            source: "file".into(),
        }
    }

    /// Override the source identifier — useful when treating a folder
    /// as a stand-in for a real source during bootstrap
    /// (e.g. `with_source_id("jira")`).
    pub fn with_source_id(mut self, id: impl Into<String>) -> Self {
        self.source = id.into();
        self
    }
}

#[derive(Debug, Deserialize)]
struct FileRule {
    id: String,
    source_uri: String,
    context: String,
    kind: RuleKind,
    title: String,
    body: String,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    #[serde(default)]
    tags: Vec<String>,
}

#[async_trait]
impl DomainIngestor for FileIngestor {
    fn source_id(&self) -> &str {
        &self.source
    }

    async fn list(&self, filter: &IngestFilter) -> Result<Vec<RemoteRule>> {
        let entries = walk_toml(&self.root).await?;
        let mut out = Vec::with_capacity(entries.len());

        for path in entries {
            let text = fs::read_to_string(&path).await?;
            let fr: FileRule = toml::from_str(&text).map_err(|e| DomainError::Source {
                source_id: self.source.clone(),
                message: format!("{}: {e}", path.display()),
            })?;

            if let Some(cutoff) = filter.since
                && fr.updated_at < cutoff
            {
                continue;
            }

            out.push(RemoteRule {
                id: fr.id,
                source: self.source.clone(),
                source_uri: fr.source_uri,
                context: fr.context,
                kind: fr.kind,
                title: fr.title,
                body: fr.body,
                updated_at: fr.updated_at,
                tags: fr.tags,
            });

            if out.len() >= filter.max_items {
                break;
            }
        }

        Ok(out)
    }
}

/// Non-recursive scan is a deliberate choice — keeps ownership obvious.
/// Users who want nested layouts can create one FileIngestor per subtree.
async fn walk_toml(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let p = entry.path();
        if p.is_file() && p.extension().is_some_and(|e| e == "toml") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn reads_toml_files() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("r1.toml");
        tokio::fs::write(
            &p,
            r#"
id = "R-001"
source_uri = "file:///specs/R-001.md"
context = "billing"
kind = "invariant"
title = "Refund limit"
body = "Refunds above $500 need approval."
updated_at = "2026-04-16T08:00:00Z"
tags = ["compliance"]
"#,
        )
        .await
        .unwrap();

        let ing = FileIngestor::new(tmp.path());
        let rules = ing.list(&IngestFilter::default()).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "R-001");
        assert_eq!(rules[0].kind, RuleKind::Invariant);
    }

    #[tokio::test]
    async fn since_filter_works() {
        let tmp = tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join("r1.toml"),
            r#"
id = "R-1"
source_uri = "x"
context = "billing"
kind = "policy"
title = "old"
body = "old"
updated_at = "2026-01-01T00:00:00Z"
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("r2.toml"),
            r#"
id = "R-2"
source_uri = "y"
context = "billing"
kind = "policy"
title = "new"
body = "new"
updated_at = "2026-04-01T00:00:00Z"
"#,
        )
        .await
        .unwrap();

        let ing = FileIngestor::new(tmp.path());
        let filter = IngestFilter {
            since: Some(time::macros::datetime!(2026-03-01 00:00 UTC)),
            max_items: 100,
        };
        let rules = ing.list(&filter).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "R-2");
    }
}
