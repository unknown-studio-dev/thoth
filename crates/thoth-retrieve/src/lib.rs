//! # thoth-retrieve
//!
//! Retrieval orchestrator. Given a [`Query`] and a [`Mode`], it fans out to
//! the relevant stores, fuses the results with Reciprocal Rank Fusion, and
//! returns a [`Retrieval`].
//!
//! Pipeline (see `DESIGN.md` §4):
//!
//! ```text
//! Query → { symbol | graph | BM25 | markdown | vector (Mode::Full) }
//!       → RRF fuse
//!       → (Mode::Full) Synthesizer::synthesize
//!       → Retrieval
//! ```
//!
//! This crate also hosts the [`Indexer`], which walks a source tree and
//! populates every backend behind a [`StoreRoot`]. The retriever assumes an
//! indexer has already run.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod config;
pub mod enrich;
pub mod indexer;
pub mod retriever;

pub use config::{IndexConfig, OutputConfig, RetrieveConfig, WatchConfig};
pub use enrich::{enrich_chunks, extract_docstring};
pub use indexer::{IndexProgress, IndexStats, Indexer, chunk_id, read_span};
pub use retriever::Retriever;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use thoth_core::{Embedder, Mode, Query, Result, Retrieval, Synthesizer};
use thoth_store::{StoreRoot, VectorStore};

/// Process-lifetime cache for [`RetrieveConfig`], keyed by store root path.
///
/// Populated on first use per path; subsequent calls skip the disk read.
static RETRIEVE_CFG_CACHE: OnceLock<RwLock<HashMap<PathBuf, RetrieveConfig>>> = OnceLock::new();

/// Return the cached [`RetrieveConfig`] for `root`, loading and inserting it
/// on the first call for each distinct root path.
async fn cached_retrieve_config(root: &std::path::Path) -> RetrieveConfig {
    let cache = RETRIEVE_CFG_CACHE.get_or_init(|| RwLock::new(HashMap::new()));

    // Fast path: config already cached for this root.
    {
        let guard = cache.read().expect("RETRIEVE_CFG_CACHE poisoned");
        if let Some(cfg) = guard.get(root) {
            return cfg.clone();
        }
    }

    // Slow path: load from disk, then insert under write lock.
    let cfg = RetrieveConfig::load_or_default(root).await;
    {
        let mut guard = cache.write().expect("RETRIEVE_CFG_CACHE poisoned");
        // Another task may have raced us — only insert if still absent.
        guard
            .entry(root.to_path_buf())
            .or_insert_with(|| cfg.clone());
    }
    cfg
}

/// Convenience wrapper: opens the right extra backends for the requested
/// [`Mode`] and runs a single recall.
///
/// In Mode::Zero the synthesizer and vector stages are skipped. In Mode::Full
/// the vector store is opened at `<root>/vectors.db` (per DESIGN §7) and
/// the caller-supplied embedder / synthesizer are plugged into the
/// retriever.
pub async fn recall(store: StoreRoot, q: Query, mode: Mode) -> Result<Retrieval> {
    let retrieve_cfg = cached_retrieve_config(&store.path).await;
    match mode {
        Mode::Zero => {
            Retriever::new(store)
                .with_markdown_boost(retrieve_cfg.rerank_markdown_boost)
                .recall(&q)
                .await
        }
        Mode::Full {
            embedder,
            synthesizer,
        } => {
            let vectors_path = StoreRoot::vectors_path(&store.path);
            let vectors = VectorStore::open(&vectors_path).await?;
            let embedder: Option<Arc<dyn Embedder>> = embedder.map(Arc::from);
            let synth: Option<Arc<dyn Synthesizer>> = synthesizer.map(Arc::from);
            Retriever::with_full(store, Some(vectors), embedder, synth)
                .with_markdown_boost(retrieve_cfg.rerank_markdown_boost)
                .recall_full(&q)
                .await
        }
    }
}
