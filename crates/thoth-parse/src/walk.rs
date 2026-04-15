//! File discovery.
//!
//! Walks a source tree and yields the paths Thoth should index, honouring
//! `.gitignore`, `.ignore`, `.git/info/exclude`, hidden file rules, and a
//! few extra filters (binary files, `target/`, `node_modules/`, ...).

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use tracing::debug;

use crate::LanguageRegistry;

/// Options controlling [`walk_sources`].
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Maximum file size in bytes to consider. Larger files are skipped.
    pub max_file_size: u64,
    /// Whether to follow symlinks.
    pub follow_symlinks: bool,
    /// Whether to descend into hidden directories (e.g. `.github`).
    pub include_hidden: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_file_size: 2 * 1024 * 1024, // 2 MiB
            follow_symlinks: false,
            include_hidden: false,
        }
    }
}

/// Enumerate indexable source files under `root`.
///
/// Returns only files whose extension is recognized by `registry` and which
/// pass the [`WalkOptions`] filters.
pub fn walk_sources(
    root: impl AsRef<Path>,
    registry: &LanguageRegistry,
    opts: &WalkOptions,
) -> Vec<PathBuf> {
    let root = root.as_ref();
    let walker = WalkBuilder::new(root)
        .hidden(!opts.include_hidden)
        .follow_links(opts.follow_symlinks)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .build();

    let mut out = Vec::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if registry.detect(path).is_none() {
            continue;
        }
        match std::fs::metadata(path) {
            Ok(md) if md.len() <= opts.max_file_size => out.push(path.to_path_buf()),
            Ok(_) => debug!(?path, "skip: too large"),
            Err(e) => debug!(?path, error = %e, "skip: stat failed"),
        }
    }
    out
}
