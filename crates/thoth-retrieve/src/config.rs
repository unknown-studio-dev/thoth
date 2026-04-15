//! User-facing indexer config.
//!
//! Loaded from `<root>/config.toml` under the `[index]` table. Anything the
//! user can tweak without touching code lives here — most importantly the
//! `ignore` list, so users can exclude build outputs, vendored trees, or
//! generated code from retrieval without writing Rust.
//!
//! Example:
//!
//! ```toml
//! [index]
//! ignore = [
//!     "target/",
//!     "node_modules/",
//!     "*.generated.rs",
//!     "docs/internal/",
//! ]
//! # Optional: raise/lower the max file size (bytes). 2 MiB by default.
//! max_file_size = 2_097_152
//! # Optional: recurse into dotfile dirs. Off by default.
//! include_hidden = false
//! ```
//!
//! Unknown keys are rejected (so typos surface loudly). If the file is
//! missing, malformed, or has no `[index]` table, the defaults kick in.
//!
//! The config is read by the CLI / library entrypoint and wired into the
//! [`Indexer`](crate::Indexer) via [`Indexer::with_ignore_patterns`] and the
//! walker's [`WalkOptions`](thoth_parse::walk::WalkOptions).

use std::path::Path;

use serde::Deserialize;

/// Indexer-side config. Mirrors the `[index]` table in `<root>/config.toml`.
///
/// See the module docs for an annotated example.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// Gitignore-syntax patterns to exclude from indexing. Applied on top
    /// of `.gitignore`, `.ignore`, and `.thothignore`. Supports negation
    /// with a leading `!`.
    pub ignore: Vec<String>,
    /// Max file size (bytes) considered for indexing. Files larger than
    /// this are skipped with a `debug!` line. Default: 2 MiB.
    pub max_file_size: u64,
    /// Whether to descend into hidden (dotfile) directories. Default: off.
    pub include_hidden: bool,
    /// Whether to follow symlinks. Default: off — symlinks into other
    /// projects would otherwise balloon the index.
    pub follow_symlinks: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            max_file_size: 2 * 1024 * 1024,
            include_hidden: false,
            follow_symlinks: false,
        }
    }
}

/// TOML file schema — the outer document. We only care about `[index]`;
/// other tables (e.g. `[memory]`, read by `thoth-memory`) are left to
/// their owners, so we tolerate them instead of `deny_unknown_fields` here.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    index: Option<IndexConfig>,
}

impl IndexConfig {
    /// Load `<root>/config.toml` if it exists, returning the `[index]`
    /// table (or [`Self::default`] if the file / table are missing).
    ///
    /// A malformed file emits a `warn!` and falls back to defaults — the
    /// user's index must not become unusable because they mistyped one key.
    pub async fn load_or_default(root: &Path) -> Self {
        let path = root.join("config.toml");
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "index: could not read config.toml, using defaults");
                return Self::default();
            }
        };
        match toml::from_str::<ConfigFile>(&text) {
            Ok(cf) => cf.index.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "index: config.toml parse error, using defaults");
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn parses_index_table() {
        let dir = tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("config.toml"),
            r#"
            [index]
            ignore = ["target/", "*.bak"]
            include_hidden = true
            "#,
        )
        .await
        .unwrap();

        let cfg = IndexConfig::load_or_default(dir.path()).await;
        assert_eq!(cfg.ignore, vec!["target/".to_string(), "*.bak".to_string()]);
        assert!(cfg.include_hidden);
        assert!(!cfg.follow_symlinks);
        assert_eq!(cfg.max_file_size, 2 * 1024 * 1024);
    }

    #[tokio::test]
    async fn missing_file_falls_back_to_defaults() {
        let dir = tempdir().unwrap();
        let cfg = IndexConfig::load_or_default(dir.path()).await;
        assert!(cfg.ignore.is_empty());
        assert!(!cfg.include_hidden);
    }

    #[tokio::test]
    async fn other_tables_are_tolerated() {
        // `[memory]` belongs to thoth-memory; presence here must not break
        // the index loader. `deny_unknown_fields` is inside IndexConfig, not
        // at the top level.
        let dir = tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("config.toml"),
            r#"
            [memory]
            episodic_ttl_days = 7

            [index]
            ignore = ["dist/"]
            "#,
        )
        .await
        .unwrap();
        let cfg = IndexConfig::load_or_default(dir.path()).await;
        assert_eq!(cfg.ignore, vec!["dist/".to_string()]);
    }

    #[tokio::test]
    async fn malformed_file_falls_back_to_defaults() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("config.toml"), "this is not = toml ][[")
            .await
            .unwrap();
        let cfg = IndexConfig::load_or_default(dir.path()).await;
        // Defaults apply — load_or_default must not panic.
        assert!(cfg.ignore.is_empty());
    }
}
