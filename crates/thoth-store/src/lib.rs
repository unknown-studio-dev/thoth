//! # thoth-store
//!
//! Embedded storage backends used by Thoth. Each submodule wraps a specific
//! backend behind a thin async-friendly API so that `thoth-retrieve` and
//! `thoth-memory` do not depend on concrete engines.
//!
//! | Backend   | Role                                              |
//! |-----------|---------------------------------------------------|
//! | `redb`    | graph nodes / edges + symbol lookup + metadata    |
//! | `tantivy` | BM25 full-text index                              |
//! | `sqlite`  | episodic FTS5 log + flat cosine vector index      |
//! | `markdown`| MEMORY.md / LESSONS.md readers + writers          |
//!
//! See `DESIGN.md` §3 and §7.
//!
//! ## On-disk layout (matches `DESIGN.md` §7)
//!
//! ```text
//! <root>/
//!   config.toml        (optional user config — loaded by thoth::CodeMemory)
//!   MEMORY.md
//!   LESSONS.md
//!   skills/<slug>/SKILL.md
//!   graph.redb         (symbol + call graph)
//!   fts.tantivy/       (BM25 index)
//!   episodes.db        (SQLite + FTS5 episodic log)
//!   chunks.lance/      (LanceDB vector index; Mode::Full with `lance` feature)
//!   vectors.db         (SQLite flat-cosine fallback; Mode::Full without `lance`)
//! ```
//!
//! For backward compat, [`StoreRoot::open`] auto-migrates the old
//! `index/` subdirectory layout (`index/kv.redb`, `index/fts/`,
//! `index/episodes.sqlite`, `index/vectors.sqlite`) to the new root-level
//! names the first time it opens a stale store.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod episodes;
pub mod fts;
pub mod kv;
pub mod markdown;
pub mod vector;
#[cfg(feature = "lance")]
pub mod vector_lance;

use std::path::{Path, PathBuf};

use thoth_core::Result;

pub use episodes::{EpisodeHit, EpisodeLog};
pub use fts::{ChunkDoc, FtsHit, FtsIndex};
pub use kv::{EdgeRow, KvStore, NodeRow, SymbolRow};
pub use markdown::MarkdownStore;
pub use vector::VectorHit;

// `VectorStore` is the public name for *the* vector backend. Which concrete
// implementation you get depends on the `lance` feature:
//
// - default:        SQLite flat-cosine (crate::vector::VectorStore)
// - `--features lance`: LanceDB            (crate::vector_lance::LanceVectorStore)
//
// Both expose identical method signatures (`open`, `upsert`, `upsert_batch`,
// `search`, `delete`, `delete_by_path`, `count`), so every downstream crate
// (`thoth-retrieve`, `thoth`, `thoth-cli`, tests) can keep saying
// `use thoth_store::VectorStore` without changes.
#[cfg(not(feature = "lance"))]
pub use vector::VectorStore;
#[cfg(feature = "lance")]
pub use vector_lance::LanceVectorStore as VectorStore;
// Keep the concrete names around too, for callers that need to be explicit.
pub use vector::VectorStore as SqliteVectorStore;
#[cfg(feature = "lance")]
pub use vector_lance::LanceVectorStore;

/// Root handle bundling every backend living under a `.thoth/` dir.
///
/// Opening a [`StoreRoot`] lazily creates all the sub-paths required by the
/// individual backends, so downstream code can assume "if it opened, it's
/// ready".
#[derive(Clone)]
pub struct StoreRoot {
    /// Root path on disk (typically `.thoth/`).
    pub path: PathBuf,
    /// Markdown memory surface (source of truth).
    pub markdown: MarkdownStore,
    /// redb-backed graph + KV.
    pub kv: KvStore,
    /// tantivy-backed BM25 index.
    pub fts: FtsIndex,
    /// SQLite+FTS5 episodic log.
    pub episodes: EpisodeLog,
}

impl StoreRoot {
    /// Canonical path for the graph/symbol KV store.
    pub fn graph_path(root: &Path) -> PathBuf {
        root.join("graph.redb")
    }
    /// Canonical path for the tantivy BM25 index directory.
    pub fn fts_path(root: &Path) -> PathBuf {
        root.join("fts.tantivy")
    }
    /// Canonical path for the SQLite episodic log.
    pub fn episodes_path(root: &Path) -> PathBuf {
        root.join("episodes.db")
    }
    /// Canonical path for the SQLite flat-cosine vector index (the
    /// Mode::Full fallback when the `lance` feature is not enabled).
    pub fn vectors_sqlite_path(root: &Path) -> PathBuf {
        root.join("vectors.db")
    }
    /// Canonical path for the LanceDB vector index directory (Mode::Full
    /// when built with the `lance` feature).
    pub fn vectors_lance_path(root: &Path) -> PathBuf {
        root.join("chunks.lance")
    }

    /// Canonical path for *the* active vector store — resolves to the
    /// SQLite file by default, or the LanceDB directory when built with
    /// `--features lance`. Call sites that just want "open the vector
    /// store for this root" should use this rather than hard-coding one
    /// of the two above.
    pub fn vectors_path(root: &Path) -> PathBuf {
        #[cfg(feature = "lance")]
        {
            Self::vectors_lance_path(root)
        }
        #[cfg(not(feature = "lance"))]
        {
            Self::vectors_sqlite_path(root)
        }
    }

    /// Open (or create) every backend under `path`.
    ///
    /// The vector store is intentionally *not* opened here — it is needed
    /// only in `Mode::Full` and is constructed separately via
    /// [`VectorStore::open`].
    ///
    /// If the legacy `<root>/index/` subdir layout from earlier versions is
    /// present, it is migrated in-place before opening so existing users
    /// don't lose their indexes.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&path).await?;

        migrate_legacy_layout(&path).await?;

        let markdown = MarkdownStore::open(&path).await?;
        let kv = KvStore::open(Self::graph_path(&path)).await?;
        let fts = FtsIndex::open(Self::fts_path(&path)).await?;
        let episodes = EpisodeLog::open(Self::episodes_path(&path)).await?;

        Ok(Self {
            path,
            markdown,
            kv,
            fts,
            episodes,
        })
    }
}

/// Migrate the pre-0.1 `<root>/index/…` layout to the current root-level
/// filenames. Runs at most once per store — idempotent and a no-op on
/// fresh directories.
async fn migrate_legacy_layout(root: &Path) -> Result<()> {
    let legacy = root.join("index");
    if !legacy.is_dir() {
        return Ok(());
    }
    let moves: [(&str, PathBuf); 4] = [
        ("kv.redb", StoreRoot::graph_path(root)),
        ("fts", StoreRoot::fts_path(root)),
        ("episodes.sqlite", StoreRoot::episodes_path(root)),
        ("vectors.sqlite", StoreRoot::vectors_sqlite_path(root)),
    ];
    let mut moved_any = false;
    for (old_name, new_path) in &moves {
        let old_path = legacy.join(old_name);
        if !old_path.exists() {
            continue;
        }
        if new_path.exists() {
            // Target already exists — safer to leave the old one in place
            // than to clobber current data.
            continue;
        }
        if let Some(parent) = new_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(&old_path, new_path).await?;
        moved_any = true;
        tracing::info!(
            from = %old_path.display(),
            to = %new_path.display(),
            "thoth-store: migrated legacy index file"
        );
    }
    // If the old directory is now empty, drop it so we never try again.
    if moved_any
        && let Ok(mut rd) = tokio::fs::read_dir(&legacy).await
        && rd.next_entry().await.ok().flatten().is_none()
    {
        let _ = tokio::fs::remove_dir(&legacy).await;
    }
    Ok(())
}
