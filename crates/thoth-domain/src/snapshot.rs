//! On-disk snapshot store for Domain memory.
//!
//! Writes each ingested rule as one markdown file under
//! `<root>/domain/<context>/_remote/<source>/<id>.md` with a TOML
//! frontmatter block (`+++` delimiters — chosen over `---` so `id:`
//! and `kind:` never collide with YAML edge cases).
//!
//! The on-disk shape is the **contract**. Downstream retrieval (tantivy
//! ingest, vector indexing) reads these files directly; changing the
//! frontmatter schema is a breaking change.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::error::{DomainError, Result};
use crate::types::{Frontmatter, RemoteRule, RuleStatus};

/// Delimiter for the TOML frontmatter block.
const DELIM: &str = "+++";

/// Writes snapshot files and reads them back for drift checks.
///
/// Snapshots live under `<root>/domain/<context>/_remote/<source>/<id>.md`.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    /// Domain subtree root — usually `<thoth_root>/domain`.
    pub root: PathBuf,
}

/// Outcome of a single snapshot write. Callers use this to build a
/// [`crate::SyncReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotWrite {
    /// New file created — rule is new to this store.
    Created,
    /// Existing file overwritten because content hash changed upstream.
    Updated,
    /// File existed and content hash matched — nothing written.
    Unchanged,
}

impl SnapshotStore {
    /// Create a snapshot store rooted at `<thoth_root>/domain`.
    pub fn new(thoth_root: impl AsRef<Path>) -> Self {
        Self {
            root: thoth_root.as_ref().join("domain"),
        }
    }

    /// Filesystem path for a given rule. Stable — do not change.
    pub fn path_for(&self, rule: &RemoteRule) -> PathBuf {
        self.root
            .join(&rule.context)
            .join("_remote")
            .join(&rule.source)
            .join(rule.safe_filename())
    }

    /// Write (or skip) a snapshot. Returns [`SnapshotWrite::Unchanged`]
    /// when the existing file's `source_hash` matches the incoming rule's
    /// content hash — this is how drift is detected without re-parsing.
    pub async fn upsert(&self, rule: &RemoteRule) -> Result<SnapshotWrite> {
        let path = self.path_for(rule);
        let incoming_hash = rule.content_hash();

        if let Ok(existing) = fs::read_to_string(&path).await {
            if let Ok((fm, _body)) = parse(&existing)
                && fm.source_hash == incoming_hash
            {
                return Ok(SnapshotWrite::Unchanged);
            }
            self.write(rule, &incoming_hash).await?;
            return Ok(SnapshotWrite::Updated);
        }

        self.write(rule, &incoming_hash).await?;
        Ok(SnapshotWrite::Created)
    }

    /// Low-level write. Always produces a fresh file — caller decides
    /// whether it's a create or update.
    async fn write(&self, rule: &RemoteRule, content_hash: &str) -> Result<()> {
        let path = self.path_for(rule);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let fm = Frontmatter {
            id: rule.id.clone(),
            source: rule.source.clone(),
            source_uri: rule.source_uri.clone(),
            source_hash: content_hash.to_string(),
            context: rule.context.clone(),
            kind: rule.kind,
            status: RuleStatus::Proposed,
            last_synced: OffsetDateTime::now_utc(),
            remote_updated_at: rule.updated_at,
            tags: rule.tags.clone(),
        };

        let contents = render(&fm, &rule.title, &rule.body)?;

        // Write atomically: write to `.tmp`, fsync, rename. Survives
        // mid-sync crashes cleanly.
        let tmp = path.with_extension("md.tmp");
        {
            let mut f = fs::File::create(&tmp).await?;
            f.write_all(contents.as_bytes()).await?;
            f.sync_all().await?;
        }
        fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

/// Serialize a frontmatter + title + body into the on-disk format.
pub fn render(fm: &Frontmatter, title: &str, body: &str) -> Result<String> {
    let toml_block = toml::to_string_pretty(fm)?;
    let mut out = String::with_capacity(toml_block.len() + body.len() + 64);
    out.push_str(DELIM);
    out.push('\n');
    out.push_str(&toml_block);
    if !toml_block.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(DELIM);
    out.push_str("\n\n# ");
    out.push_str(title.trim());
    out.push_str("\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Parse an on-disk snapshot back into (frontmatter, body-without-title).
/// The title is preserved inside `body` — we don't strip it because the
/// snapshot is meant to be human-readable as-is.
pub fn parse(contents: &str) -> Result<(Frontmatter, String)> {
    let rest = contents
        .strip_prefix(DELIM)
        .ok_or_else(|| DomainError::Snapshot("missing opening +++ delimiter".into()))?;
    let rest = rest.trim_start_matches('\n');

    let close = rest
        .find(&format!("\n{DELIM}"))
        .ok_or_else(|| DomainError::Snapshot("missing closing +++ delimiter".into()))?;
    let toml_block = &rest[..close];
    let body = rest[close + DELIM.len() + 1..].trim_start().to_string();

    let fm: Frontmatter = toml::from_str(toml_block)?;
    Ok((fm, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RemoteRule, RuleKind};
    use tempfile::tempdir;

    fn rule() -> RemoteRule {
        RemoteRule {
            id: "PROJ-1".into(),
            source: "file".into(),
            source_uri: "file:///local/PROJ-1".into(),
            context: "billing".into(),
            kind: RuleKind::Invariant,
            title: "Refund over 500 needs manager".into(),
            body: "Any refund above $500 requires manager approval.\n".into(),
            updated_at: time::macros::datetime!(2026-04-16 08:00 UTC),
            tags: vec!["compliance".into()],
        }
    }

    #[tokio::test]
    async fn upsert_creates_then_detects_unchanged() {
        let tmp = tempdir().unwrap();
        let store = SnapshotStore::new(tmp.path());
        let r = rule();
        assert_eq!(store.upsert(&r).await.unwrap(), SnapshotWrite::Created);
        assert_eq!(store.upsert(&r).await.unwrap(), SnapshotWrite::Unchanged);
    }

    #[tokio::test]
    async fn body_change_triggers_update() {
        let tmp = tempdir().unwrap();
        let store = SnapshotStore::new(tmp.path());
        let mut r = rule();
        store.upsert(&r).await.unwrap();
        r.body = "Updated: refund threshold now $1000.\n".into();
        assert_eq!(store.upsert(&r).await.unwrap(), SnapshotWrite::Updated);
    }

    #[tokio::test]
    async fn round_trip_preserves_frontmatter() {
        let tmp = tempdir().unwrap();
        let store = SnapshotStore::new(tmp.path());
        let r = rule();
        store.upsert(&r).await.unwrap();

        let path = store.path_for(&r);
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let (fm, body) = parse(&raw).unwrap();

        assert_eq!(fm.id, "PROJ-1");
        assert_eq!(fm.context, "billing");
        assert_eq!(fm.kind, RuleKind::Invariant);
        assert_eq!(fm.status, RuleStatus::Proposed);
        assert_eq!(fm.source_hash, r.content_hash());
        assert!(body.starts_with("# Refund"));
    }
}
