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
//! ## On-disk layout
//!
//! ```text
//! <root>/
//!   MEMORY.md
//!   LESSONS.md
//!   skills/<slug>/SKILL.md
//!   index/
//!     kv.redb
//!     fts/            (tantivy directory)
//!     episodes.sqlite
//!     vectors.sqlite  (flat cosine index; Mode::Full only)
//! ```

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod episodes;
pub mod fts;
pub mod kv;
pub mod markdown;
pub mod vector;

use std::path::{Path, PathBuf};

use thoth_core::Result;

pub use episodes::{EpisodeHit, EpisodeLog};
pub use fts::{ChunkDoc, FtsHit, FtsIndex};
pub use kv::{EdgeRow, KvStore, NodeRow, SymbolRow};
pub use markdown::MarkdownStore;
pub use vector::{VectorHit, VectorStore};

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
    /// Open (or create) every backend under `path`.
    ///
    /// The vector store is intentionally *not* opened here — it is needed
    /// only in `Mode::Full` and is constructed separately via
    /// [`VectorStore::open`].
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&path).await?;
        let index_dir = path.join("index");
        tokio::fs::create_dir_all(&index_dir).await?;

        let markdown = MarkdownStore::open(&path).await?;
        let kv = KvStore::open(index_dir.join("kv.redb")).await?;
        let fts = FtsIndex::open(index_dir.join("fts")).await?;
        let episodes = EpisodeLog::open(index_dir.join("episodes.sqlite")).await?;

        Ok(Self {
            path,
            markdown,
            kv,
            fts,
            episodes,
        })
    }
}
